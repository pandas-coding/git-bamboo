//! Graph filtering as a query condition (`getGraph` with a `filter`).
//!
//! The unfiltered graph read pages over the rev walk directly (windowed,
//! O(limit)). A filter cannot work that way — the walk has to visit every
//! commit to know which ones match — so the first filtered request for a
//! given (filter, tips, epoch) materializes the FULL hit list once and
//! memoizes it on the session as a [`FilteredView`]; subsequent pages
//! (virtual scrolling) slice that list, keeping page reads O(limit) and
//! letting `total_approx` be the EXACT hit count.
//!
//! Path conditions are the expensive part: every candidate commit needs a
//! tree-diff against its first parent. Safety caps bound a hostile repo
//! (see [`FilterLimits`]): a walked-commit cap and a wall-clock budget
//! over the WHOLE walk (a filtered walk holds the session's repo lock, so
//! bounding the walk bounds how long reads and writes queue behind it);
//! hitting either stops the walk and flags the response `truncated` so
//! the UI shows "≥ N commits" instead of pretending completeness.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bamboo_protocol::{BambooError, Commit, GraphFilter, GraphPage, GraphViewport};
use tracing::{info, warn};

use crate::gix_read::{anchored_start, assign_lanes, resolve_commit_metas, rev_walk_newest_first};
use crate::session::Session;

/// How many distinct filters keep their materialized view per (epoch,
/// tips) generation. Beyond that the least recently used view is evicted
/// — a user flailing on the filter box must not grow session memory.
const MAX_MEMOIZED_VIEWS: usize = 8;

/// Deadline clock checks happen every this-many commits for the
/// metadata-cheap phases: reading a clock per commit would itself be
/// measurable at 100k commits. The expensive per-commit work (tree
/// diffs) checks after EVERY call — each diff is individually unbounded,
/// so checking only at this granularity could overshoot the budget by
/// `BUDGET_CHECK_INTERVAL - 1` full diffs.
const BUDGET_CHECK_INTERVAL: usize = 512;

/// Metadata is resolved (and the cache lock held) per chunk of this many
/// commits: the gaps let other requests take the cache lock between
/// chunks instead of queueing behind a whole filtered walk.
const META_CHUNK: usize = 4096;

/// Safety caps for one filtered walk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FilterLimits {
    /// Maximum number of commits WALKED (not hits) before the walk gives
    /// up and flags the response truncated.
    pub max_commits: usize,
    /// Wall-clock budget for the ENTIRE filtered walk (enumerate,
    /// metadata, predicates) — bounds how long the walk holds the
    /// session's repo lock, so a filter-cycling client cannot starve
    /// reads and writes.
    pub time_budget: Duration,
}

impl Default for FilterLimits {
    fn default() -> Self {
        Self {
            max_commits: 200_000,
            time_budget: Duration::from_secs(5),
        }
    }
}

/// A materialized filtered commit list.
#[derive(Debug)]
struct FilteredView {
    /// Hit ids in walk order (newest first).
    ids: Vec<gix::ObjectId>,
    /// Lane per hit, parallel to `ids`. Lanes are assigned over the FULL
    /// walked topology (the hit list alone would mis-thread merges), so
    /// threading matches the unfiltered graph.
    lanes: Vec<u16>,
    /// The walk hit a safety cap; `ids` is then incomplete and
    /// `total_approx` is a lower bound.
    truncated: bool,
}

/// Memoized filtered views hung on the session, keyed by (filter,
/// limits) — a view built under injected test caps can never be served
/// to a default-limits call as if complete. In memory (not redb): the
/// view is derived, rebuildable data, and per-session by nature. All
/// views share one (epoch, tips) generation, invalidated wholesale when
/// the graph moves; within a generation eviction is LRU.
#[derive(Debug)]
pub struct FilteredViews {
    epoch: u64,
    tips: Vec<gix::ObjectId>,
    /// LRU order: front = least recently used.
    order: VecDeque<(GraphFilter, FilterLimits)>,
    views: HashMap<(GraphFilter, FilterLimits), Arc<FilteredView>>,
}

impl FilteredViews {
    /// An empty slot for the (epoch, tips) generation — the first
    /// filtered request of a generation creates it.
    fn new() -> Self {
        Self {
            epoch: 0,
            tips: Vec::new(),
            order: VecDeque::new(),
            views: HashMap::new(),
        }
    }
    /// The memoized view for `key` under the CURRENT (epoch, tips)
    /// generation — None when the generation moved (the slot is reset
    /// wholesale: every memoized view is stale together) or the view is
    /// not memoized. Touches the key (LRU) on a hit.
    fn get(
        &mut self,
        key: &(GraphFilter, FilterLimits),
        current_epoch: u64,
        tips: &[gix::ObjectId],
    ) -> Option<Arc<FilteredView>> {
        if self.epoch != current_epoch || self.tips != tips {
            self.order.clear();
            self.views.clear();
            self.epoch = current_epoch;
            self.tips = tips.to_vec();
        }
        let view = self.views.get(key)?.clone();
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
            self.order.push_back(key.clone());
        }
        Some(view)
    }

    /// Memoize a freshly built view for the CURRENT generation. Evicts
    /// the least recently used view when the slot is full.
    fn insert(
        &mut self,
        key: (GraphFilter, FilterLimits),
        view: Arc<FilteredView>,
        current_epoch: u64,
        tips: &[gix::ObjectId],
    ) {
        if self.epoch != current_epoch || self.tips != tips {
            self.order.clear();
            self.views.clear();
            self.epoch = current_epoch;
            self.tips = tips.to_vec();
        }
        if !self.views.contains_key(&key) {
            while self.views.len() >= MAX_MEMOIZED_VIEWS {
                match self.order.pop_front() {
                    Some(evict) => {
                        self.views.remove(&evict);
                    }
                    None => break,
                }
            }
        }
        if let Some(pos) = self.order.iter().position(|k| *k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
        self.views.insert(key, view);
    }
}

/// Serve one filtered page. The caller (`gix_read::read_graph_page_with_limits`)
/// holds ONLY the repo lock here; this fn re-acquires the cache lock per
/// metadata chunk and never holds the filtered-views lock across a
/// build. `ensure_view` builds (or reuses) the memoized view, then the
/// page is sliced out of it.
pub(crate) fn filtered_page(
    session: &Session,
    repo: &gix::Repository,
    tips: &[gix::ObjectId],
    viewport: &GraphViewport,
    filter: &GraphFilter,
    current_epoch: u64,
    limits: &FilterLimits,
) -> anyhow::Result<GraphPage> {
    let view = ensure_view(session, repo, tips, filter, current_epoch, limits)?;

    // Page slicing: same anchor semantics as the unfiltered read (see
    // `gix_read::anchored_start`) — the anchor's index in the (filtered)
    // list is the effective offset, so the pinned commit stays on
    // screen; unknown anchors keep the offset, and an out-of-range
    // offset pages past the end (empty page — offsets are
    // client-supplied and must never panic a slice).
    let anchor_row = viewport
        .anchor_commit
        .as_deref()
        .and_then(parse_oid)
        .and_then(|anchor| view.ids.iter().position(|id| *id == anchor));
    let start = anchored_start(anchor_row, view.ids.len(), viewport.offset);
    let end = start
        .saturating_add(viewport.limit as usize)
        .min(view.ids.len());
    let mut commits = {
        let cache = session.cache.lock().unwrap_or_else(|e| e.into_inner());
        resolve_commit_metas(repo, &cache, &view.ids[start..end])?
    };
    for (commit, lane) in commits
        .iter_mut()
        .zip(view.lanes[start..end].iter().copied())
    {
        commit.lane = lane;
    }
    let has_more = end < view.ids.len();

    // Final epoch guard: never serve a page whose epoch is no longer
    // current by the time we finish.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }

    info!(
        hits = view.ids.len(),
        truncated = view.truncated,
        returned = commits.len(),
        "filtered graph page returned"
    );

    Ok(GraphPage {
        commits,
        // Filtered: exact hit count (full traversal), so the scrollbar
        // always matches the content.
        total_approx: view.ids.len() as u64,
        has_more,
        epoch: current_epoch,
        anchor_commit: viewport.anchor_commit.clone(),
        truncated: view.truncated,
    })
}

/// The memoized view for (filter, limits), building it when absent. The
/// build runs with NO session lock held beyond the repo lock the caller
/// already holds: a filtered walk can take seconds, and holding the
/// filtered-views mutex (or the cache mutex) across it would queue every
/// other request behind one walk.
fn ensure_view(
    session: &Session,
    repo: &gix::Repository,
    tips: &[gix::ObjectId],
    filter: &GraphFilter,
    current_epoch: u64,
    limits: &FilterLimits,
) -> anyhow::Result<Arc<FilteredView>> {
    let key = (filter.clone(), limits.clone());
    {
        let mut guard = session
            .filtered_views
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(view) = guard.as_mut().and_then(|views| views.get(&key, current_epoch, tips))
        {
            return Ok(view);
        }
    }
    let built = Arc::new(build_filtered_view(
        session, repo, filter, tips, current_epoch, limits,
    )?);
    // Epoch TOCTOU guard: a filtered walk can take seconds. Never
    // memoize (nor serve) data from a superseded epoch.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }
    let mut guard = session
        .filtered_views
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // A concurrent request may have won the build race and memoized an
    // equivalent view — prefer it so LRU order stays coherent.
    if let Some(view) = guard
        .as_mut()
        .and_then(|views| views.get(&key, current_epoch, tips))
    {
        return Ok(view);
    }
    guard
        .get_or_insert_with(FilteredViews::new)
        .insert(key, built.clone(), current_epoch, tips);
    Ok(built)
}

/// Parse a full (40/64-char) hex commit id. Anchored viewports only pin
/// ids the graph itself handed out; anything unparsable simply doesn't
/// pin and the offset is used as-is.
fn parse_oid(id: &str) -> Option<gix::ObjectId> {
    gix::ObjectId::from_hex(id.as_bytes()).ok()
}

/// Materialize the full filtered hit list for (filter, tips) under the
/// given caps. Holds only the repo lock (the caller's); the cache lock
/// is taken per metadata chunk so other requests interleave.
fn build_filtered_view(
    session: &Session,
    repo: &gix::Repository,
    filter: &GraphFilter,
    tips: &[gix::ObjectId],
    current_epoch: u64,
    limits: &FilterLimits,
) -> anyhow::Result<FilteredView> {
    let deadline = Instant::now() + limits.time_budget;
    let mut truncated = false;

    // Phase 1 — enumerate the full walk (bounded by max_commits). Ids
    // only; metadata comes next, cache-first.
    let mut ids: Vec<gix::ObjectId> = Vec::with_capacity(4096);
    for (idx, info) in rev_walk_newest_first(repo, tips)?.enumerate() {
        if idx >= limits.max_commits
            || (idx % BUDGET_CHECK_INTERVAL == 0 && Instant::now() >= deadline)
        {
            truncated = true;
            break;
        }
        ids.push(info?.id);
    }

    // Phase 2 — metadata for every walked commit (cache-first; the walk
    // backfills the COMMIT_META cache exactly like the windowed read).
    // Chunked: the cache lock is released between chunks and the
    // deadline is enforced per chunk.
    let mut all: Vec<Commit> = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(META_CHUNK) {
        let metas = {
            let cache = session.cache.lock().unwrap_or_else(|e| e.into_inner());
            resolve_commit_metas(repo, &cache, chunk)?
        };
        all.extend(metas);
        if Instant::now() >= deadline {
            truncated = true;
            break;
        }
    }

    // Phase 3 — predicate pass. Conditions AND. Author/message needles
    // are pre-lowercased; the path needle is raw (case-sensitive).
    let author_needle = filter.author.as_ref().map(|s| s.to_lowercase());
    let message_needle = filter.message.as_ref().map(|s| s.to_lowercase());
    let path_needle = filter.path.as_ref().map(|s| s.as_bytes());
    let mut diff_state = gix::diff::tree::State::default();

    let mut hit_idx: Vec<usize> = Vec::new();
    for (idx, commit) in all.iter().enumerate() {
        // Metadata-cheap conditions check the deadline at
        // BUDGET_CHECK_INTERVAL granularity (a clock read per commit
        // would itself be measurable at 100k commits).
        if idx % BUDGET_CHECK_INTERVAL == 0 && Instant::now() >= deadline {
            truncated = true;
            break;
        }
        let matched = commit_matches(
            repo,
            &mut diff_state,
            commit,
            author_needle.as_deref(),
            message_needle.as_deref(),
            path_needle,
        )?;
        if matched {
            hit_idx.push(idx);
        }
        // A path condition's tree diff is individually unbounded: check
        // the deadline after EVERY one, so the overshoot is at most one
        // diff's cost — never BUDGET_CHECK_INTERVAL of them.
        if path_needle.is_some() && Instant::now() >= deadline {
            truncated = true;
            break;
        }
    }

    // Phase 4 — lanes over the FULL walked topology, then keep only the
    // hits' lanes; persist them like the unfiltered read does.
    let lane_entries = assign_lanes(&mut all);
    {
        let cache = session.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = cache.put_lanes_with_epoch(&lane_entries, current_epoch) {
            warn!(error = %e, "failed to cache lanes");
        }
    }

    Ok(FilteredView {
        ids: hit_idx.iter().map(|&i| ids[i]).collect(),
        lanes: hit_idx.iter().map(|&i| all[i].lane).collect(),
        truncated,
    })
}

/// One commit vs. the filter conditions (AND). `path` is the raw
/// (case-sensitive) needle; author/message needles arrive pre-lowercased.
fn commit_matches(
    repo: &gix::Repository,
    diff_state: &mut gix::diff::tree::State,
    commit: &Commit,
    author: Option<&str>,
    message: Option<&str>,
    path: Option<&[u8]>,
) -> anyhow::Result<bool> {
    if let Some(needle) = author {
        if !(ci_contains(&commit.author_name, needle) || ci_contains(&commit.author_email, needle))
        {
            return Ok(false);
        }
    }
    if let Some(needle) = message {
        if !ci_contains(&commit.message, needle) {
            return Ok(false);
        }
    }
    if let Some(needle) = path {
        return tree_diff_contains(repo, diff_state, &commit.id, needle);
    }
    Ok(true)
}

/// Case-insensitive substring: Unicode `to_lowercase` on both sides.
/// Deliberately NOT git-log's `--regex` semantics — regex is hostile in a
/// GUI input box and an injection surface (see `GraphFilter` docs).
fn ci_contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

/// Whether any path changed by commit `id` vs. its first parent (the
/// full tree for root commits) contains `needle` as a substring.
///
/// In-process `gix` tree diff — the write path's `diff_tree_files` shells
/// out to git twice per commit, which is hopeless at graph-walk scale —
/// with a delegate that breaks the traversal at the FIRST matching path:
/// the common case (a file's own history) costs one tree pair, not the
/// whole diff.
fn tree_diff_contains(
    repo: &gix::Repository,
    state: &mut gix::diff::tree::State,
    id: &str,
    needle: &[u8],
) -> anyhow::Result<bool> {
    let oid = gix::ObjectId::from_hex(id.as_bytes())
        .map_err(|e| anyhow::anyhow!("commit id {id:?}: {e}"))?;
    let commit = repo.find_commit(oid)?;
    let new_tree = commit.tree()?;
    let old_tree = match commit.parent_ids().next() {
        Some(parent_id) => Some(repo.find_commit(parent_id)?.tree()?),
        // Root commit: everything is an addition vs. the empty tree.
        None => None,
    };
    let empty_tree: Vec<u8> = Vec::new();
    let lhs = gix::objs::TreeRefIter::from_bytes(
        old_tree
            .as_ref()
            .map(|tree| tree.data.as_slice())
            .unwrap_or(&empty_tree),
        repo.object_hash(),
    );
    let rhs = gix::objs::TreeRefIter::from_bytes(&new_tree.data, repo.object_hash());

    let mut matcher = PathMatcher::new(needle);
    match gix::diff::tree(lhs, rhs, state, &repo.objects, &mut matcher) {
        // Traversal ran to completion: whatever `visit()` saw decides.
        Ok(()) => Ok(matcher.found),
        // The delegate broke the traversal — that only happens on a match.
        Err(gix::diff::tree::Error::Cancelled) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// `Visit` delegate for substring path matching. Mirrors the path
/// bookkeeping of gix's `Recorder` (a current path plus a deque of
/// directory prefixes for recursion), but records nothing and cancels the
/// traversal (`ControlFlow::Break`) at the first matching path.
struct PathMatcher<'a> {
    needle: &'a [u8],
    found: bool,
    path: Vec<u8>,
    path_deque: VecDeque<Vec<u8>>,
}

impl<'a> PathMatcher<'a> {
    fn new(needle: &'a [u8]) -> Self {
        Self {
            needle,
            found: false,
            path: Vec::new(),
            path_deque: VecDeque::new(),
        }
    }

    fn push_element(&mut self, name: &[u8]) {
        if name.is_empty() {
            return;
        }
        if !self.path.is_empty() {
            self.path.push(b'/');
        }
        self.path.extend_from_slice(name);
    }

    fn pop_element(&mut self) {
        match self.path.iter().rposition(|&b| b == b'/') {
            Some(pos) => self.path.truncate(pos),
            None => self.path.clear(),
        }
    }
}

impl gix::diff::tree::Visit for PathMatcher<'_> {
    fn pop_front_tracked_path_and_set_current(&mut self) {
        if let Some(path) = self.path_deque.pop_front() {
            self.path = path;
        }
    }

    fn push_back_tracked_path_component(&mut self, component: &gix::bstr::BStr) {
        self.push_element(component);
        self.path_deque.push_back(self.path.clone());
    }

    fn push_path_component(&mut self, component: &gix::bstr::BStr) {
        self.push_element(component);
    }

    fn pop_path_component(&mut self) {
        self.pop_element();
    }

    fn visit(&mut self, _change: gix::diff::tree::visit::Change) -> gix::diff::tree::visit::Action {
        if contains_subslice(&self.path, self.needle) {
            self.found = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

/// Substring over bytes. Paths are raw bytes and stay case-sensitive —
/// git tree paths are.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
