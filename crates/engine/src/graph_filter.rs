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
//! tree-diff against its first parent. Two safety caps bound a hostile
//! repo (see [`FilterLimits`]): a walked-commit cap and a tree-diff time
//! budget; hitting either stops the walk and flags the response
//! `truncated` so the UI shows "≥ N commits" instead of pretending
//! completeness.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use bamboo_protocol::{BambooError, Commit, GraphFilter, GraphPage, GraphViewport};
use tracing::{info, warn};

use crate::cache::Cache;
use crate::gix_read::{assign_lanes, resolve_commit_metas, rev_walk_newest_first};
use crate::session::Session;

/// How many distinct filters keep their materialized view per (epoch,
/// tips) generation. Beyond that the old views are dropped wholesale — a
/// user flailing on the filter box must not grow session memory.
const MAX_MEMOIZED_VIEWS: usize = 8;

/// Deadline clock checks happen every this-many commits: reading a clock
/// per commit would itself be measurable at 100k commits, while a
/// half-walk overshoot of the budget is harmless (it bounds abuse, not
/// latency).
const BUDGET_CHECK_INTERVAL: usize = 512;

/// Safety caps for one filtered walk.
#[derive(Debug, Clone)]
pub struct FilterLimits {
    /// Maximum number of commits WALKED (not hits) before the walk gives
    /// up and flags the response truncated.
    pub max_commits: usize,
    /// Maximum cumulative time spent tree-diffing for a path condition.
    /// Only enforced when the filter HAS a path condition — author/message
    /// walks are metadata-bound (commit-graph + redb cache) and don't need
    /// a budget.
    pub tree_diff_time_budget: Duration,
}

impl Default for FilterLimits {
    fn default() -> Self {
        Self {
            max_commits: 200_000,
            tree_diff_time_budget: Duration::from_secs(5),
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

/// Memoized filtered views hung on the session. In memory (not redb): the
/// view is derived, rebuildable data, and per-session by nature. All views
/// share one (epoch, tips) generation and are invalidated wholesale when
/// the graph moves — there is no per-view staleness to track.
#[derive(Debug, Default)]
pub struct FilteredViews {
    epoch: u64,
    tips: Vec<gix::ObjectId>,
    views: HashMap<GraphFilter, FilteredView>,
}

/// Serve one filtered page. The caller (`gix_read::read_graph_page_with_limits`)
/// holds the repo and cache locks and has already checked the epoch;
/// `filtered_page` builds (or reuses) the memoized view, then slices it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filtered_page(
    session: &Session,
    repo: &gix::Repository,
    cache: &Cache,
    tips: &[gix::ObjectId],
    viewport: &GraphViewport,
    filter: &GraphFilter,
    current_epoch: u64,
    limits: &FilterLimits,
) -> anyhow::Result<GraphPage> {
    let mut views = session
        .filtered_views
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Wholesale invalidation: any epoch or tip-set change makes every
    // memoized view stale, no matter which filter built it.
    if let Some(held) = views.as_ref() {
        if held.epoch != current_epoch || held.tips != tips {
            *views = None;
        }
    }

    let missing = views
        .as_ref()
        .and_then(|held| held.views.get(filter))
        .is_none();
    if missing {
        let built = build_filtered_view(repo, cache, filter, tips, current_epoch, limits)?;
        // Epoch TOCTOU guard: a filtered walk can take seconds (path
        // condition on a big repo). Never memoize (nor serve) data from a
        // superseded epoch.
        if session.current_epoch() != current_epoch {
            return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
        }
        let slot = views.get_or_insert_with(|| FilteredViews {
            epoch: current_epoch,
            tips: tips.to_vec(),
            views: HashMap::new(),
        });
        if slot.views.len() >= MAX_MEMOIZED_VIEWS && !slot.views.contains_key(filter) {
            slot.views.clear();
        }
        slot.views.insert(filter.clone(), built);
    }
    let view = views
        .as_ref()
        .and_then(|held| held.views.get(filter))
        .expect("the view was just ensured");

    // Page slicing: same anchor semantics as the unfiltered read — the
    // anchor's index in the (filtered) list is the effective offset, so
    // the pinned commit stays on screen; unknown anchors keep the offset.
    let start = match viewport.anchor_commit.as_deref().and_then(parse_oid) {
        Some(anchor) => view
            .ids
            .iter()
            .position(|id| *id == anchor)
            .map(|idx| idx.min(view.ids.len().saturating_sub(1)))
            .unwrap_or(viewport.offset as usize),
        None => viewport.offset as usize,
    };
    let end = start
        .saturating_add(viewport.limit as usize)
        .min(view.ids.len());
    let mut commits = resolve_commit_metas(repo, cache, &view.ids[start..end])?;
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

/// Parse a full (40/64-char) hex commit id. Anchored viewports only pin
/// ids the graph itself handed out; anything unparsable simply doesn't
/// pin and the offset is used as-is.
fn parse_oid(id: &str) -> Option<gix::ObjectId> {
    gix::ObjectId::from_hex(id.as_bytes()).ok()
}

/// Materialize the full filtered hit list for (filter, tips) under the
/// given caps.
fn build_filtered_view(
    repo: &gix::Repository,
    cache: &Cache,
    filter: &GraphFilter,
    tips: &[gix::ObjectId],
    current_epoch: u64,
    limits: &FilterLimits,
) -> anyhow::Result<FilteredView> {
    // Phase 1 — enumerate the full walk (bounded by max_commits). Ids
    // only; metadata comes next, cache-first.
    let mut ids: Vec<gix::ObjectId> = Vec::with_capacity(4096);
    let mut truncated = false;
    for (idx, info) in rev_walk_newest_first(repo, tips)?.enumerate() {
        if idx >= limits.max_commits {
            truncated = true;
            break;
        }
        ids.push(info?.id);
    }

    // Phase 2 — metadata for every walked commit (cache-first; the walk
    // backfills the COMMIT_META cache exactly like the windowed read).
    let mut all = resolve_commit_metas(repo, cache, &ids)?;

    // Phase 3 — predicate pass. Conditions AND. Author/message needles
    // are pre-lowercased; the path needle is raw (case-sensitive).
    let author_needle = filter.author.as_ref().map(|s| s.to_lowercase());
    let message_needle = filter.message.as_ref().map(|s| s.to_lowercase());
    let path_needle = filter.path.as_ref().map(|s| s.as_bytes());
    let diff_deadline = path_needle.map(|_| Instant::now() + limits.tree_diff_time_budget);
    let mut diff_state = gix::diff::tree::State::default();

    let mut hit_idx: Vec<usize> = Vec::new();
    for (idx, commit) in all.iter().enumerate() {
        if let Some(deadline) = diff_deadline {
            if idx % BUDGET_CHECK_INTERVAL == 0 && Instant::now() >= deadline {
                truncated = true;
                break;
            }
        }
        if commit_matches(
            repo,
            &mut diff_state,
            commit,
            author_needle.as_deref(),
            message_needle.as_deref(),
            path_needle,
        )? {
            hit_idx.push(idx);
        }
    }

    // Phase 4 — lanes over the FULL walked topology, then keep only the
    // hits' lanes; persist them like the unfiltered read does.
    let lane_entries = assign_lanes(&mut all);
    if let Err(e) = cache.put_lanes_with_epoch(&lane_entries, current_epoch) {
        warn!(error = %e, "failed to cache lanes");
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
