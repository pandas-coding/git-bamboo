use anyhow::Context;
use std::collections::{HashMap, HashSet};

use tracing::{debug, info, warn};

use bamboo_protocol::{
    BambooError, BlameCommit, BlameEntry, BlameVia, BranchStatus, Commit, DiffHunk, DiffLine,
    DiffLineKind, GetBlame, GetBlameResult, GetDiffResult, GraphFilter, GraphPage, GraphViewport,
    Ref, RefKind, RemoteInfo, StashEntry, StatusCode, StatusItem,
};

use crate::cache::{Cache, CachedCommit};
use crate::graph_filter::{self, FilterLimits};
use crate::rebase::{current_branch_name, read_rebase_state, rebase_chain, resolve_rev_commit};
use crate::session::Session;
use crate::write_queue::{redact_credentials, validate_path_arg, validate_ref_arg};

const PRELOAD_MARGIN: u32 = 500;

pub fn read_graph_page(session: &Session, viewport: &GraphViewport) -> anyhow::Result<GraphPage> {
    read_graph_page_with_limits(session, viewport, None, &FilterLimits::default())
}

/// `read_graph_page` with a filter: `Some(filter)` routes to the
/// engine-side filtered traversal (see `graph_filter`); `None` — or an
/// all-empty filter, which is a no-op — keeps the exact unfiltered
/// behavior.
pub fn read_graph_page_with_filter(
    session: &Session,
    viewport: &GraphViewport,
    filter: Option<&GraphFilter>,
) -> anyhow::Result<GraphPage> {
    read_graph_page_with_limits(session, viewport, filter, &FilterLimits::default())
}

/// Graph read entry point with injectable safety caps for the filtered
/// walk. Production uses `FilterLimits::default()`; tests inject small
/// caps to exercise truncation deterministically.
pub fn read_graph_page_with_limits(
    session: &Session,
    viewport: &GraphViewport,
    filter: Option<&GraphFilter>,
    limits: &FilterLimits,
) -> anyhow::Result<GraphPage> {
    let current_epoch = session.current_epoch();
    if viewport.epoch != 0 && viewport.epoch != current_epoch {
        return Err(BambooError::epoch_mismatch(current_epoch, viewport.epoch).into());
    }

    // Poison-tolerant lock recovery: a panicked request handler must not
    // take down the whole connection.
    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
    let cache = session.cache.lock().unwrap_or_else(|e| e.into_inner());

    // Invalidate lane cache if epoch changed.
    let _ = cache.invalidate_on_epoch_change(current_epoch)?;

    let tips = collect_tips(&repo)?;
    if tips.is_empty() {
        return Ok(GraphPage {
            commits: Vec::new(),
            total_approx: 0,
            has_more: false,
            epoch: current_epoch,
            anchor_commit: viewport.anchor_commit.clone(),
            truncated: false,
        });
    }

    match filter.filter(|f| !f.is_empty()) {
        Some(f) => graph_filter::filtered_page(
            session,
            &repo,
            &cache,
            &tips,
            viewport,
            f,
            current_epoch,
            limits,
        ),
        None => read_unfiltered_page(session, &repo, &cache, &tips, viewport, current_epoch),
    }
}

/// The unfiltered page read: windowed rev walk, cache-first metadata,
/// sweep-line lanes, anchor re-pinning. This is the pre-filter behavior,
/// untouched by the `graph_filter` module.
fn read_unfiltered_page(
    session: &Session,
    repo: &gix::Repository,
    cache: &Cache,
    tips: &[gix::ObjectId],
    viewport: &GraphViewport,
    current_epoch: u64,
) -> anyhow::Result<GraphPage> {
    // Walk commits newest-first with commit graph acceleration. This pass
    // only enumerates object ids (cheap via the commit-graph); commit
    // metadata is resolved afterwards, cache-first.
    let walk = rev_walk_newest_first(repo, tips)?;

    let end = viewport
        .offset
        .saturating_add(viewport.limit as u64)
        .saturating_add(PRELOAD_MARGIN as u64);

    let mut ids: Vec<gix::ObjectId> = Vec::with_capacity(end.min(10_000) as usize);
    for (idx, info) in walk.enumerate() {
        let info = info?;
        if idx >= end as usize {
            break;
        }
        ids.push(info.id);
    }

    // Epoch TOCTOU guard: re-check AFTER the walk. If the watcher bumped
    // the epoch mid-read (refs moved), stale data must neither be returned
    // nor cached under the new epoch.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }

    // Resolve commit metadata cache-first (see `resolve_commit_metas`).
    let mut all_commits = resolve_commit_metas(repo, cache, &ids)?;

    let total_approx = all_commits.len() as u64;
    // NOTE: `all_commits` is walked from row 0 (the rev walk enumerates
    // newest-first from the top), so its length is already a lower bound of
    // the true total — never add `viewport.offset` here.

    // Assign lanes over the walk window and persist them + the epoch they
    // are valid for in ONE redb transaction.
    let lane_entries = assign_lanes(&mut all_commits);
    if let Err(e) = cache.put_lanes_with_epoch(&lane_entries, current_epoch) {
        warn!(error = %e, "failed to cache lanes");
    }

    // Anchor re-pinning: when the client pins a viewport to a commit, use
    // that commit's index as the effective offset so the view stays stable
    // across invalidations (new commits landing on top shift the page down
    // instead of scrolling the anchor out). Unknown anchors keep the
    // requested offset.
    let start = match &viewport.anchor_commit {
        Some(anchor) => all_commits
            .iter()
            .position(|c| &c.id == anchor)
            .map(|idx| idx.min(all_commits.len().saturating_sub(1)))
            .unwrap_or(viewport.offset as usize),
        None => viewport.offset as usize,
    };
    let has_more = start + (viewport.limit as usize) < all_commits.len();
    let commits: Vec<Commit> = all_commits
        .into_iter()
        .skip(start)
        .take(viewport.limit as usize)
        .collect();

    // Final epoch guard: never serve (nor acknowledge) a page whose epoch
    // is no longer current by the time we finish.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }

    info!(
        offset = viewport.offset,
        limit = viewport.limit,
        returned = commits.len(),
        "graph page returned"
    );

    Ok(GraphPage {
        commits,
        total_approx,
        has_more,
        epoch: current_epoch,
        anchor_commit: viewport.anchor_commit.clone(),
        truncated: false,
    })
}

/// Collect all ref targets plus HEAD as tips. Refs that point at
/// annotated tags (refs/tags/*) hold a *tag object* id, and the rev walk
/// does not peel tips itself — feeding it a tag id fails with "Expected
/// object of kind commit but got tag". Peel every ref to the commit it
/// ultimately references instead; refs that don't resolve to a commit
/// (e.g. refs/notes trees) are skipped rather than failing the page.
fn collect_tips(repo: &gix::Repository) -> anyhow::Result<Vec<gix::ObjectId>> {
    let mut tips = Vec::new();
    for reference in repo.references()?.all()? {
        let mut reference = reference.map_err(|e| anyhow::anyhow!("{e}"))?;
        match reference.peel_to_commit() {
            Ok(commit) => {
                if !tips.contains(&commit.id) {
                    tips.push(commit.id);
                }
            }
            Err(err) => {
                debug!(
                    reference = %reference.name().as_bstr(),
                    error = %err,
                    "skipping ref tip that does not resolve to a commit"
                );
            }
        }
    }
    if let Ok(head_commit) = repo.head_commit() {
        if !tips.contains(&head_commit.id) {
            tips.push(head_commit.id);
        }
    }
    Ok(tips)
}

/// The shared graph walk: newest-first by commit time, commit-graph
/// accelerated. Both the unfiltered windowed read and the filtered full
/// walk enumerate from this.
pub(crate) fn rev_walk_newest_first<'repo>(
    repo: &'repo gix::Repository,
    tips: &[gix::ObjectId],
) -> anyhow::Result<gix::revision::Walk<'repo>> {
    Ok(repo
        .rev_walk(tips.iter().copied())
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .use_commit_graph(true)
        .all()?)
}

/// Resolve commit metadata for `ids`, cache-first: hits are served from
/// the COMMIT_META cache (keyed by immutable object id, so entries never
/// go stale) and misses are decoded and written back in one batch.
/// `lane` is left at 0 — callers assign lanes afterwards.
pub(crate) fn resolve_commit_metas(
    repo: &gix::Repository,
    cache: &Cache,
    ids: &[gix::ObjectId],
) -> anyhow::Result<Vec<Commit>> {
    let id_strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    let cached = cache.get_commit_metas(&id_strs)?;
    let mut all_commits: Vec<Commit> = Vec::with_capacity(ids.len());
    let mut misses: Vec<(String, CachedCommit)> = Vec::new();
    for oid in ids {
        let id = oid.to_string();
        if let Some(meta) = cached.get(&id) {
            all_commits.push(Commit {
                id,
                message: meta.message.clone(),
                author_name: meta.author_name.clone(),
                author_email: meta.author_email.clone(),
                author_time: meta.author_time,
                parent_ids: meta.parent_ids.clone(),
                lane: 0,
            });
            continue;
        }

        let commit_obj = repo.find_object(*oid)?;
        let commit = commit_obj.try_into_commit()?;
        let commit_ref = commit.decode()?;

        let message = String::from_utf8_lossy(&commit_ref.message_summary()).to_string();
        let author = commit_ref.author()?;
        let author_name = String::from_utf8_lossy(author.name.as_ref()).to_string();
        let author_email = String::from_utf8_lossy(author.email.as_ref()).to_string();
        let author_time = author.time().map(|t| t.seconds).unwrap_or(0);
        let parent_ids: Vec<String> = commit_ref.parents().map(|id| id.to_string()).collect();
        // Committer time rides along so a blame join gets a COMPLETE cache
        // hit (getBlame serves both timestamps per entry). `.ok()` because
        // a malformed committer line must not fail the graph read; blame
        // treats the resulting `None` as a partial hit and re-decodes.
        let committer_time = commit_ref
            .committer()
            .ok()
            .and_then(|c| c.time().ok())
            .map(|t| t.seconds);

        misses.push((
            id.clone(),
            CachedCommit {
                message: message.clone(),
                author_name: author_name.clone(),
                author_email: author_email.clone(),
                author_time,
                parent_ids: parent_ids.clone(),
                committer_time,
            },
        ));
        all_commits.push(Commit {
            id,
            message,
            author_name,
            author_email,
            author_time,
            parent_ids,
            lane: 0,
        });
    }
    if let Err(e) = cache.put_commit_metas(&misses) {
        warn!(error = %e, "failed to cache commit metadata");
    }
    Ok(all_commits)
}

/// Assign lanes using a sweep-line algorithm, walking newest → oldest.
/// lanes[i] = Some(commit_id) means lane i is occupied by commit_id's
/// line. Lanes are u16: with pathological graphs (> 65 535 concurrent
/// lanes) the assignment collapses instead of panicking — extra commits
/// fall back to lane 0. Returns the (id, lane) pairs for persistence.
pub(crate) fn assign_lanes(all_commits: &mut [Commit]) -> Vec<(String, u16)> {
    // Build children map for lane assignment.
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for commit in all_commits.iter() {
        for parent_id in &commit.parent_ids {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(commit.id.clone());
        }
    }

    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut commit_lanes: HashMap<String, u16> = HashMap::new();

    for commit in all_commits.iter_mut() {
        let commit_children = children.get(&commit.id).cloned().unwrap_or_default();

        let lane = match commit_children.len() {
            0 => {
                // Tip: find lowest free lane.
                let mut found = None;
                for (i, slot) in lanes.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(commit.id.clone());
                        found = Some(i as u16);
                        break;
                    }
                }
                match found {
                    Some(l) => l,
                    None if lanes.len() < u16::MAX as usize => {
                        let l = lanes.len() as u16;
                        lanes.push(Some(commit.id.clone()));
                        l
                    }
                    // No free lane and the u16 lane space is exhausted:
                    // collapse onto lane 0 rather than panicking. (A tip
                    // has no assigned child to inherit from in this
                    // newest-first pass, so lane 0 is the fallback.)
                    None => 0,
                }
            }
            1 => {
                // Single child: inherit its lane.
                let child_id = &commit_children[0];
                let child_lane = *commit_lanes.get(child_id).unwrap_or(&0);
                if (child_lane as usize) < lanes.len() {
                    lanes[child_lane as usize] = Some(commit.id.clone());
                }
                child_lane
            }
            _ => {
                // Merge: pick leftmost child's lane, free the others.
                let mut child_lanes: Vec<u16> = commit_children
                    .iter()
                    .filter_map(|cid| commit_lanes.get(cid).copied())
                    .collect();
                child_lanes.sort_unstable();
                if child_lanes.is_empty() {
                    // Children not yet assigned (shouldn't happen in topological order).
                    if lanes.len() < u16::MAX as usize {
                        let l = lanes.len() as u16;
                        lanes.push(Some(commit.id.clone()));
                        l
                    } else {
                        0
                    }
                } else {
                    let chosen = child_lanes[0];
                    if (chosen as usize) < lanes.len() {
                        lanes[chosen as usize] = Some(commit.id.clone());
                    }
                    for &l in &child_lanes[1..] {
                        if (l as usize) < lanes.len() {
                            lanes[l as usize] = None;
                        }
                    }
                    chosen
                }
            }
        };

        commit.lane = lane;
        commit_lanes.insert(commit.id.clone(), lane);
    }

    all_commits.iter().map(|c| (c.id.clone(), c.lane)).collect()
}

pub fn read_status(session: &Session, paths: Option<&[String]>) -> anyhow::Result<Vec<StatusItem>> {
    let current_epoch = session.current_epoch();
    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());

    let platform = repo.status(gix::progress::Discard)?;

    // gix status: empty pattern list matches everything.
    let patterns: Vec<gix::bstr::BString> = Vec::new();
    let iter = platform.into_iter(patterns)?;

    let mut items = Vec::new();
    for entry in iter {
        let entry = entry.map_err(|e| anyhow::anyhow!("{e}"))?;

        let item = match entry {
            gix::status::Item::TreeIndex(change) => map_tree_index_change(change).map(|mut i| {
                i.staged = true;
                i
            }),
            gix::status::Item::IndexWorktree(iw) => map_index_worktree_item(iw),
        };

        if let Some(status_item) = item {
            // Filter by requested paths (component-aware): a path matches
            // filter F iff path == F or path starts with F + '/'. A plain
            // substring/prefix match would wrongly include sibling files
            // sharing a name stem (filter "src" matching "src-main.rs").
            if let Some(filter) = paths {
                if !filter.iter().any(|p| path_matches(&status_item.path, p)) {
                    continue;
                }
            }
            items.push(status_item);
        }
    }

    // Epoch TOCTOU guard: if the repo was mutated mid-read, report the
    // mismatch so the client refreshes instead of trusting stale status.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }

    debug!(count = items.len(), "status read");
    Ok(items)
}

/// Component-aware path filter match: `path == filter` or
/// `path` starts with `filter + "/"`.
fn path_matches(path: &str, filter: &str) -> bool {
    let filter = filter.trim_end_matches('/');
    path == filter || path.starts_with(&format!("{filter}/"))
}

/// Map gix's conflict summary onto the protocol `ConflictKind` (same
/// shape: which side added / modified / deleted the entry).
fn map_conflict_summary(
    summary: gix::status::plumbing::index_as_worktree::Conflict,
) -> bamboo_protocol::ConflictKind {
    use bamboo_protocol::ConflictKind as K;
    use gix::status::plumbing::index_as_worktree::Conflict as C;
    match summary {
        C::BothDeleted => K::BothDeleted,
        C::AddedByUs => K::AddedByUs,
        C::DeletedByThem => K::DeletedByThem,
        C::AddedByThem => K::AddedByThem,
        C::DeletedByUs => K::DeletedByUs,
        C::BothAdded => K::BothAdded,
        C::BothModified => K::BothModified,
    }
}

fn map_tree_index_change(change: gix::diff::index::Change) -> Option<StatusItem> {
    use gix::diff::index::Change as C;

    match change {
        C::Addition { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Added,
            old_path: None,
            staged: false,
            conflict_kind: None,
        }),
        C::Deletion { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Deleted,
            old_path: None,
            staged: false,
            conflict_kind: None,
        }),
        C::Modification { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Modified,
            old_path: None,
            staged: false,
            conflict_kind: None,
        }),
        C::Rewrite {
            source_location,
            location,
            copy,
            ..
        } => Some(StatusItem {
            path: location.to_string(),
            status: if copy {
                StatusCode::Copied
            } else {
                StatusCode::Renamed
            },
            old_path: Some(source_location.to_string()),
            staged: false,
            conflict_kind: None,
        }),
    }
}

fn map_index_worktree_item(item: gix::status::index_worktree::Item) -> Option<StatusItem> {
    use gix::status::index_worktree::Item as IW;
    use gix::status::plumbing::index_as_worktree::EntryStatus as ES;

    match item {
        IW::Modification {
            rela_path, status, ..
        } => {
            // Conflict entries map to Conflict + a classification (which
            // side added/modified/deleted); the classification lets the
            // frontend offer only the resolve actions that are legal for
            // this file's conflict kind.
            let (code, conflict_kind) = match status {
                ES::Conflict { summary, .. } => {
                    (StatusCode::Conflict, Some(map_conflict_summary(summary)))
                }
                ES::Change(change) => (
                    match change {
                        gix::status::plumbing::index_as_worktree::Change::Removed => {
                            StatusCode::Deleted
                        }
                        _ => StatusCode::Modified,
                    },
                    None,
                ),
                ES::IntentToAdd => (StatusCode::Added, None),
                ES::NeedsUpdate(_) => return None,
            };
            Some(StatusItem {
                path: rela_path.to_string(),
                status: code,
                old_path: None,
                staged: false,
                conflict_kind,
            })
        }
        IW::DirectoryContents { entry, .. } => {
            let code = match entry.status {
                gix::dir::entry::Status::Untracked => StatusCode::Untracked,
                gix::dir::entry::Status::Ignored(_) => StatusCode::Ignored,
                _ => return None,
            };
            Some(StatusItem {
                path: entry.rela_path.to_string(),
                status: code,
                old_path: None,
                staged: false,
                conflict_kind: None,
            })
        }
        IW::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => {
            let old_path = match source {
                gix::status::index_worktree::RewriteSource::RewriteFromIndex {
                    source_rela_path,
                    ..
                } => Some(source_rela_path.to_string()),
                gix::status::index_worktree::RewriteSource::CopyFromDirectoryEntry {
                    source_dirwalk_entry,
                    ..
                } => Some(source_dirwalk_entry.rela_path.to_string()),
            };
            Some(StatusItem {
                path: dirwalk_entry.rela_path.to_string(),
                status: if copy {
                    StatusCode::Copied
                } else {
                    StatusCode::Renamed
                },
                old_path,
                staged: false,
                conflict_kind: None,
            })
        }
    }
}

pub fn read_refs(session: &Session) -> anyhow::Result<Vec<Ref>> {
    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
    let mut refs = Vec::new();

    for reference in repo.references()?.all()? {
        let mut reference = reference.map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = reference.name().as_bstr().to_string();
        // Peel to the underlying commit: annotated tag refs point at tag
        // objects, but consumers (graph row chips, branch pickers) key and
        // match on commit ids. Refs that don't resolve to a commit fall
        // back to their direct target.
        let target = match reference.peel_to_commit() {
            Ok(commit) => commit.id.to_string(),
            Err(_) => reference
                .target()
                .try_id()
                .map(|id| id.to_owned().to_string())
                .unwrap_or_default(),
        };

        let (kind, display) = if name.starts_with("refs/tags/") {
            (RefKind::Tag, name.strip_prefix("refs/tags/"))
        } else if name.starts_with("refs/remotes/") {
            (RefKind::RemoteBranch, name.strip_prefix("refs/remotes/"))
        } else {
            (RefKind::Branch, name.strip_prefix("refs/heads/"))
        };

        refs.push(Ref {
            name: display.unwrap_or(&name).to_string(),
            kind,
            target,
        });
    }

    info!(count = refs.len(), "refs read");
    Ok(refs)
}

/// Read the UTF-8 content of `path` as stored in `revision`'s tree.
/// Returns an empty string when the path is absent (the UI treats that as
/// no content rather than an error). `revision` is parsed by gix (no shell),
/// but reject option-like strings defensively anyway.
///
/// Stage revisions `:1`/`:2`/`:3` are served from the unmerged index
/// entries of `path` (base/ours/theirs of a conflict) instead of a
/// commit — the merge-editor flow needs all three sides. An absent
/// stage resolves to the same empty string as an absent path.
pub fn read_blob(session: &Session, revision: &str, path: &str) -> anyhow::Result<String> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.chars().any(|c| c.is_ascii_control())
    {
        anyhow::bail!("invalid revision: {revision:?}");
    }
    if path.is_empty() || path.starts_with('-') || path.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("invalid path: {path:?}");
    }

    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(stage) = parse_stage_revision(revision) {
        return read_conflict_stage_blob(&repo, path, stage);
    }

    // ":0" serves the stage-0 (normal) index entry — the left side of the
    // worktree diff and the right side of the staged diff (hunk staging,
    // phase 5). Missing from the index (untracked, or staged as deleted)
    // resolves to the same empty string as an absent path.
    if revision == INDEX_REVISION {
        return read_index_blob(&repo, path);
    }

    let id = repo
        .rev_parse_single(revision.as_bytes())
        .map_err(|e| anyhow::anyhow!("revision {revision:?} not found: {e}"))?;
    let commit = id
        .object()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .peel_to_commit()
        .map_err(|e| anyhow::anyhow!("{revision:?} is not a commit: {e}"))?;
    let tree = commit.tree().map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(entry) = tree
        .lookup_entry_by_path(path)
        .map_err(|e| anyhow::anyhow!("{e}"))?
    else {
        return Ok(String::new());
    };
    let blob = entry
        .object()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .try_into_blob()
        .map_err(|e| anyhow::anyhow!("{path:?} is not a file: {e}"))?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

/// `":N"` (N in 1..=3) as a conflict-stage revision; anything else is
/// None (a normal rev-parse revision).
fn parse_stage_revision(revision: &str) -> Option<gix::index::entry::Stage> {
    match revision {
        ":1" => Some(gix::index::entry::Stage::Base),
        ":2" => Some(gix::index::entry::Stage::Ours),
        ":3" => Some(gix::index::entry::Stage::Theirs),
        _ => None,
    }
}

/// Blob content of `path` at the given unmerged index stage. A missing
/// stage (e.g. `:1` for a both-added conflict, `:3` for a deleted-by-
/// them conflict) — and a non-conflicted path, which has no unmerged
/// entries at all — resolves to an empty string, matching the
/// absent-path convention the normal-revision `read_blob` path (and the
/// diff/merge views) already rely on. Real failures (index read, blob
/// lookup) stay errors so callers can tell "that side is legitimately
/// absent" from "the engine failed".
fn read_conflict_stage_blob(
    repo: &gix::Repository,
    path: &str,
    stage: gix::index::entry::Stage,
) -> anyhow::Result<String> {
    let index = repo.index()?;
    let Some(entry) = index.entries().iter().find(|entry| {
        entry.stage() == stage && entry.path_in(index.path_backing()) == path.as_bytes()
    }) else {
        return Ok(String::new());
    };
    let blob = repo.find_blob(entry.id)?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

/// The stage-0 (normal) index revision, served by `read_blob` from the
/// index (see the diff/openResource flow in the extension: the left side
/// of a worktree diff, the right side of a staged diff). Distinct from the
/// `:1`/`:2`/`:3` conflict stages.
pub(crate) const INDEX_REVISION: &str = ":0";

/// Blob content of `path`'s stage-0 index entry (lossy UTF-8, like every
/// read_blob variant). An absent entry resolves to an empty string.
fn read_index_blob(repo: &gix::Repository, path: &str) -> anyhow::Result<String> {
    let index = repo.index()?;
    let Some(entry) = index.entries().iter().find(|entry| {
        entry.stage() == gix::index::entry::Stage::Unconflicted
            && entry.path_in(index.path_backing()) == path.as_bytes()
    }) else {
        return Ok(String::new());
    };
    let blob = repo.find_blob(entry.id)?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

/// Read HEAD for the session's repository: the commit id (empty on an
/// unborn branch) plus the short branch name when HEAD is symbolic.
/// Uses the resolved git dir, so linked worktrees work.
pub fn read_head(session: &Session) -> anyhow::Result<bamboo_protocol::GetHeadResult> {
    let git_dir = crate::gitdir::resolve_git_dir(&session.repo_path);
    let raw = std::fs::read_to_string(git_dir.join("HEAD"))
        .with_context(|| format!("failed to read {}", git_dir.join("HEAD").display()))?;
    let raw = raw.trim().to_string();
    let branch = raw.strip_prefix("ref: refs/heads/").map(str::to_string);

    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
    // Unborn HEAD (fresh repo): no commit yet, but the branch name is valid.
    let head = repo
        .head_commit()
        .map(|c| c.id.to_string())
        .unwrap_or_default();

    Ok(bamboo_protocol::GetHeadResult { head, branch })
}

/// The empty tree object id: the diff baseline for root commits
/// (`git diff-tree <empty-tree> <root>` lists the root commit's files as
/// additions).
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Strict commit-id validation, mirroring the extension's graph-side rule
/// (`^[0-9a-f]{40,64}$`): 40–64 lowercase hex chars, nothing else. Host and
/// engine both check so neither side trusts the other's input.
fn is_valid_commit_id(id: &str) -> bool {
    (40..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Read full commit metadata for the detail view: complete message,
/// author/committer identities and timestamps, parent ids, and the file
/// list with per-file line counts. Files are diffed against the first
/// parent (or the empty tree for root commits) via
/// `git diff-tree -r -M -z` — one invocation per output format
/// (`--name-status`, `--numstat`), paired by path; combined flags would
/// print only one of the two formats.
pub fn read_commit_detail(
    session: &Session,
    id: &str,
) -> anyhow::Result<bamboo_protocol::CommitDetail> {
    if !is_valid_commit_id(id) {
        return Err(BambooError::invalid_argument(format!(
            "getCommitDetail: invalid commit id {id:?}"
        ))
        .into());
    }

    // Repo data is extracted under a scoped lock: the guard is released
    // before the `git diff-tree` subprocesses below run, so concurrent
    // readers (getGraph/getStatus/getRefs) are not serialized behind
    // process spawns.
    let (message, author, committer, parent_ids, detail_id) = {
        let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());

        let oid = repo
            .rev_parse_single(id.as_bytes())
            .map_err(|_| BambooError::git_error(format!("commit {id} not found")))?;
        let commit = oid
            .object()
            .map_err(|_| BambooError::git_error(format!("commit {id} not found")))?
            .peel_to_commit()
            .map_err(|_| BambooError::git_error(format!("{id} is not a commit")))?;
        let commit_ref = commit.decode()?;

        // Owned copies: the borrowed signature refs die with the lock
        // guard above.
        (
            String::from_utf8_lossy(commit_ref.message).into_owned(),
            owned_signature(commit_ref.author()?),
            owned_signature(commit_ref.committer()?),
            commit_ref
                .parents()
                .map(|p| p.to_string())
                .collect::<Vec<String>>(),
            commit.id.to_string(),
        )
    };

    // First parent (or the empty tree for root commits) is the diff
    // baseline. Both args are host/engine-validated hex oids, and argv is
    // passed without a shell, so they can never be read as options.
    let base = parent_ids
        .first()
        .cloned()
        .unwrap_or_else(|| EMPTY_TREE_OID.to_string());
    let files = diff_tree_files(&session.repo_path, &base, &detail_id)?;

    Ok(bamboo_protocol::CommitDetail {
        id: detail_id,
        message,
        author_name: author.0,
        author_email: author.1,
        author_time: author.2,
        committer_name: committer.0,
        committer_email: committer.1,
        committer_time: committer.2,
        parent_ids,
        files,
    })
}

/// Owned (name, email, unix-seconds) copy of a decoded git signature.
fn owned_signature(sig: gix::actor::SignatureRef<'_>) -> (String, String, i64) {
    (
        String::from_utf8_lossy(sig.name.as_ref()).into_owned(),
        String::from_utf8_lossy(sig.email.as_ref()).into_owned(),
        sig.time().map(|t| t.seconds).unwrap_or(0),
    )
}

/// Run `git diff-tree` twice against `base..id` (once for `--name-status`,
/// once for `--numstat`) and pair the NUL-separated records by path into
/// `CommitFile`s. In `-z` mode a rename/copy record carries two paths
/// (`<old>\0<new>`) in both formats; `--numstat` line counts are
/// `added\tdeleted` (binary files report `-\t-`, mapped to 0/0).
fn diff_tree_files(
    repo_path: &std::path::Path,
    base: &str,
    id: &str,
) -> anyhow::Result<Vec<bamboo_protocol::CommitFile>> {
    let run = |format_flag: &str| -> anyhow::Result<Vec<String>> {
        let (ok, stdout, stderr) = git_output(
            repo_path,
            &[
                "diff-tree",
                "--no-commit-id",
                "-r",
                "-M",
                "-z",
                format_flag,
                base,
                id,
            ],
        )?;
        if !ok {
            // Raw git diagnostics stay engine-side (log only): the client
            // gets a generic error, so subprocess stderr never reaches the
            // RPC error surface.
            warn!(
                base,
                id,
                stderr = %stderr,
                "git diff-tree failed"
            );
            return Err(BambooError::git_error("commit diff unavailable").into());
        }
        Ok(stdout
            .split('\0')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect())
    };

    // name-status records: status token, then one path (or two for R/C).
    let status_tokens = run("--name-status")?;
    // numstat records: "added\tdeleted[\tpath]" token, then one path (or
    // two for renames — the embedded path slot is empty in that case).
    let numstat_tokens = run("--numstat")?;

    // Pair numstat counts by new path (for R/C) / sole path otherwise.
    let mut counts: HashMap<String, (u32, u32)> = HashMap::new();
    let mut i = 0;
    while i < numstat_tokens.len() {
        let token = &numstat_tokens[i];
        let mut parts = token.splitn(3, '\t');
        let additions = parse_numstat_count(parts.next().unwrap_or("-"));
        let deletions = parse_numstat_count(parts.next().unwrap_or("-"));
        let path_slot = parts.next().unwrap_or("");
        let path = if path_slot.is_empty() {
            // Rename: old and new paths follow as separate tokens.
            i += 2;
            numstat_tokens.get(i).cloned().unwrap_or_default()
        } else {
            path_slot.to_string()
        };
        counts.insert(path, (additions, deletions));
        i += 1;
    }

    let mut files = Vec::new();
    let mut i = 0;
    while i < status_tokens.len() {
        let status_raw = status_tokens[i].clone();
        // Keep only the letter ("R100" -> "R"); unknown statuses pass
        // through verbatim rather than being guessed at.
        let status = status_raw
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
        let (path, old_path) = if status == "R" || status == "C" {
            i += 1;
            let old = status_tokens.get(i).cloned().unwrap_or_default();
            i += 1;
            let new = status_tokens.get(i).cloned().unwrap_or_default();
            (new, if old.is_empty() { None } else { Some(old) })
        } else {
            i += 1;
            let path = status_tokens.get(i).cloned().unwrap_or_default();
            (path, None)
        };
        let (additions, deletions) = counts.get(&path).cloned().unwrap_or((0, 0));
        files.push(bamboo_protocol::CommitFile {
            path,
            old_path,
            status,
            additions,
            deletions,
        });
        i += 1;
    }
    Ok(files)
}

/// numstat counts are decimal; `-` marks a binary file (no line counts
/// available) and is reported as 0/0.
fn parse_numstat_count(s: &str) -> u32 {
    s.parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Remotes / branch status / stash list (read RPCs)
// ---------------------------------------------------------------------------

/// One stdout line per remote (`git remote`), NUL/newline-free names.
fn run_git_stdout_lines(repo_path: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<String>> {
    let (ok, stdout, stderr) = git_output(repo_path, args)?;
    if !ok {
        return Err(BambooError::git_error(format!(
            "git {:?} failed: {}",
            args,
            redact_credentials(&stderr).trim()
        ))
        .into());
    }
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Read one git config value (empty/missing → `None`).
fn read_config(repo_path: &std::path::Path, key: &str) -> Option<String> {
    let (ok, stdout, _) = git_output(repo_path, &["config", "--get", key]).ok()?;
    if !ok {
        return None;
    }
    let v = stdout.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Run one git subcommand synchronously (argv only, no paths) and return
/// (success, stdout, stderr). The single subprocess shape for the
/// git-shelling read side — non-interactive auth and a closed stdin so
/// a read can never hang waiting for terminal input.
fn git_output(
    repo_path: &std::path::Path,
    args: &[&str],
) -> anyhow::Result<(bool, String, String)> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// `getRemotes`: one entry per configured remote with its fetch URL and
/// the default flag. The URL is redacted before it leaves the engine:
/// git remote URLs frequently embed credentials
/// (`https://user:pat@host/repo.git`), which must never reach the RPC
/// client. The `default` flag is a bamboo-side convention (see
/// `RemoteInfo::default`), not a git-honored notion.
pub fn read_remotes(session: &Session) -> anyhow::Result<Vec<RemoteInfo>> {
    let repo_path = &session.repo_path;
    let names = run_git_stdout_lines(repo_path, &["remote"])?;
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let configured_default = read_config(repo_path, "remote.default");

    let mut remotes = Vec::with_capacity(names.len());
    for (idx, name) in names.iter().enumerate() {
        // `git remote get-url --all <name>` (the name as its own argv
        // element) instead of building a `remote.<name>.url` config key
        // — a crafted remote name could otherwise resolve a different
        // config key. `--all` lists every configured URL; the first is
        // the fetch URL (insteadOf rewrites applied).
        let (ok, stdout, _) = git_output(repo_path, &["remote", "get-url", "--all", name])?;
        let url = if ok {
            stdout
                .lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(redact_credentials)
        } else {
            None
        };
        // The configured default only counts when it names a real remote;
        // otherwise "default" falls back to the first in list order.
        let is_default = match configured_default.as_deref() {
            Some(d) => d == name,
            None => idx == 0,
        };
        remotes.push(RemoteInfo {
            name: name.clone(),
            url,
            default: is_default,
        });
    }
    info!(count = remotes.len(), "remotes read");
    Ok(remotes)
}

/// `getBranchStatus`: the branch's upstream and ahead/behind counts vs.
/// that upstream. `branch: None` = the current HEAD branch; a detached
/// HEAD is an INVALID_ARGUMENT error so the client can disable the
/// affected buttons. A branch without an upstream reports `upstream: null`
/// with zero counts.
pub fn read_branch_status(session: &Session, branch: Option<&str>) -> anyhow::Result<BranchStatus> {
    let repo_path = &session.repo_path;
    let branch = match branch {
        Some(b) => {
            // Positional rev argument: reject option-like names before
            // they reach git (gix is not involved here, this is argv).
            validate_ref_arg(b)
                .map_err(|e| BambooError::invalid_argument(format!("getBranchStatus: {e}")))?;
            if b == "HEAD" {
                // The current branch (incl. detached-HEAD detection) is
                // expressed by `branch: null`; an explicit "HEAD" would
                // silently resolve against detached HEAD.
                return Err(BambooError::invalid_argument(
                    "getBranchStatus: branch must name a local branch; pass branch: null \
                     for the current branch",
                )
                .into());
            }
            // An unknown branch must be an explicit INVALID_ARGUMENT, not
            // an indistinguishable "no upstream, 0/0" result.
            let (ok, _, _) = git_output(
                repo_path,
                &["show-ref", "--verify", &format!("refs/heads/{b}")],
            )?;
            if !ok {
                return Err(BambooError::invalid_argument(format!(
                    "getBranchStatus: branch {b:?} not found"
                ))
                .into());
            }
            b.to_string()
        }
        None => {
            // Current branch; `--abbrev-ref` prints "HEAD" when detached.
            let lines = run_git_stdout_lines(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            let current = lines.first().cloned().unwrap_or_default();
            if current.is_empty() || current == "HEAD" {
                return Err(BambooError::invalid_argument(
                    "getBranchStatus: HEAD is detached; there is no current branch",
                )
                .into());
            }
            current
        }
    };

    // Upstream: `branch@{upstream}` fails when none is configured → null.
    // (Branch names containing '@' were rejected by validate_ref_arg, so
    // this rev spec cannot be confused with a user-supplied @{...}.)
    let (ok, stdout, _) = git_output(
        repo_path,
        &[
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ],
    )?;
    let upstream = if ok {
        let name = stdout.trim().to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    } else {
        None
    };

    let (ahead, behind) = match &upstream {
        None => (0u32, 0u32),
        Some(upstream) => {
            // `upstream...branch` symmetric difference: --count prints
            // left (upstream-only = behind) then right (branch-only =
            // ahead).
            let (ok, stdout, stderr) = git_output(
                repo_path,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{upstream}...{branch}"),
                ],
            )?;
            if !ok {
                return Err(BambooError::git_error(format!(
                    "git rev-list failed: {}",
                    redact_credentials(&stderr).trim()
                ))
                .into());
            }
            let mut parts = stdout.split_whitespace();
            let behind = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            let ahead = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            (ahead, behind)
        }
    };

    Ok(BranchStatus {
        branch,
        upstream,
        ahead,
        behind,
    })
}

/// `stashList`: `git stash list` parsed into entries
/// (`ref` = reflog selector, `message` = subject, `date` = commit date).
pub fn read_stash_list(session: &Session) -> anyhow::Result<Vec<StashEntry>> {
    // `%cI` = strict ISO-8601 commit date (the doc contract on
    // `StashEntry::date`); `%ci` would print a non-ISO format.
    let (ok, stdout, stderr) = git_output(
        &session.repo_path,
        &["stash", "list", "--format=%gd\u{1f}%s\u{1f}%cI"],
    )?;
    if !ok {
        return Err(BambooError::git_error(format!(
            "git stash list failed: {}",
            redact_credentials(&stderr).trim()
        ))
        .into());
    }
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\u{1f}');
        let stash_ref = fields.next().unwrap_or_default().to_string();
        let message = fields.next().unwrap_or_default().to_string();
        let date = fields.next().unwrap_or_default().to_string();
        if stash_ref.is_empty() {
            continue;
        }
        entries.push(StashEntry {
            stash_ref,
            message,
            date,
        });
    }
    info!(count = entries.len(), "stash list read");
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Unified diff parsing / getDiff (hunk-level staging, phase 5)
// ---------------------------------------------------------------------------

/// A parsed `git diff -U3` output for ONE path: the hunk list with exact
/// old/new line positions. Byte-level (not `String`) so the write path can
/// reconstruct index blobs byte-exactly — lossy UTF-8 conversion would
/// corrupt non-UTF-8 file content on re-hash.
#[derive(Debug, Default)]
pub(crate) struct RawFileDiff {
    pub binary: bool,
    pub hunks: Vec<RawHunk>,
}

/// One hunk, as emitted by git. `old_*` describe the old side (index for
/// the worktree diff, HEAD for the staged diff); `new_*` the new side.
#[derive(Debug)]
pub(crate) struct RawHunk {
    pub old_start: u64,
    pub old_count: u64,
    pub new_start: u64,
    pub new_count: u64,
    /// Hunk body lines with their origin marker (` ` context, `-` old,
    /// `+` new); `text` excludes the marker byte.
    pub lines: Vec<RawDiffLine>,
    /// `\ No newline at end of file` appeared after the LAST old-side
    /// line of this hunk (the old side's final line lacks a newline).
    pub old_no_newline: bool,
    /// Same, for the new side's last line.
    pub new_no_newline: bool,
}

#[derive(Debug)]
pub(crate) struct RawDiffLine {
    pub origin: u8,
    pub text: Vec<u8>,
}

impl RawHunk {
    /// The action interval this hunk occupies on the NEW side, as the
    /// shared host/engine contract for `stageRanges`/`unstageRanges`:
    /// 1-based inclusive, with a floor of line 1 and a minimum width of
    /// one line so empty hunks (whole-file deletions, `+0,0`) still have
    /// an addressable position. Any client range overlapping this
    /// interval selects the whole hunk.
    pub fn new_side_interval(&self) -> (u64, u64) {
        let start = self.new_start.max(1);
        (start, start + self.new_count.max(1) - 1)
    }
}

/// Common diff flags: reproducible, engine-parseable output regardless of
/// the user's git config (external diff drivers, textconv, color) or the
/// diff.renames default — renames are deliberately off so a rename shows
/// as delete+add and every hunk belongs to exactly one path.
pub(crate) const DIFF_FLAGS: [&str; 5] = [
    "-U3",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
];

/// Parse one file's `git diff` output into hunks. Headers
/// (`diff --git`, `index`, `new file mode`, `---`, `+++`) are skipped;
/// `@@` starts a hunk; `\` marks the no-newline-at-EOF state of the
/// preceding line's side. `Binary files … differ` / `GIT binary patch`
/// mark the whole diff binary (no hunks).
pub(crate) fn parse_unified_diff(out: &[u8]) -> RawFileDiff {
    let mut diff = RawFileDiff::default();
    let mut hunk: Option<RawHunk> = None;

    // Include a final line without trailing newline (git always ends its
    // output with \n, but stay defensive — the parser must never silently
    // drop a content line).
    for line in split_bytes_lines(out) {
        if line.starts_with(b"@@") {
            if let Some(done) = hunk.take() {
                diff.hunks.push(done);
            }
            hunk = parse_hunk_header(line);
        } else if let Some(h) = hunk.as_mut() {
            if line.first() == Some(&b'\\') {
                // The marker follows the last line of one side: '+' marks
                // the new side, '-' the old side, a shared context line
                // marks both (identical content ⇒ identical newline state).
                match h.lines.last().map(|l| l.origin) {
                    Some(b'+') => h.new_no_newline = true,
                    Some(b'-') => h.old_no_newline = true,
                    Some(b' ') => {
                        h.old_no_newline = true;
                        h.new_no_newline = true;
                    }
                    _ => {}
                }
            } else if line.is_empty() {
                // Defensive: a bare empty line inside a hunk body is a
                // context line of empty content.
                h.lines.push(RawDiffLine {
                    origin: b' ',
                    text: Vec::new(),
                });
            } else {
                let origin = line[0];
                if origin == b' ' || origin == b'+' || origin == b'-' {
                    h.lines.push(RawDiffLine {
                        origin,
                        text: line[1..].to_vec(),
                    });
                } else {
                    // A header line inside what we thought was a hunk:
                    // end the hunk (malformed input degrades to fewer
                    // hunks, never to wrong ones).
                    if let Some(done) = hunk.take() {
                        diff.hunks.push(done);
                    }
                }
            }
        } else if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            diff.binary = true;
        }
    }
    if let Some(done) = hunk.take() {
        diff.hunks.push(done);
    }
    diff
}

/// Split bytes into lines (without terminators), keeping a final line
/// that lacks a trailing newline.
fn split_bytes_lines(out: &[u8]) -> Vec<&[u8]> {
    if out.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    if let Some(last) = lines.last() {
        if last.is_empty() {
            lines.pop();
        }
    }
    lines
}

/// Parse `@@ -a[,b] +c[,d] @@ …` into a hunk skeleton. A missing count
/// means 1 (unified-diff convention); `-0,0`/`+0,0` mark empty sides.
fn parse_hunk_header(line: &[u8]) -> Option<RawHunk> {
    let text = String::from_utf8_lossy(line);
    // Spec lives between the first '@@' and the next '@@'.
    let spec = text.split("@@").nth(1)?.trim();
    let mut parts = spec.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range_spec(old)?;
    let (new_start, new_count) = parse_range_spec(new)?;
    Some(RawHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
        old_no_newline: false,
        new_no_newline: false,
    })
}

/// `a,b` → (start, count); bare `a` → (a, 1).
fn parse_range_spec(s: &str) -> Option<(u64, u64)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `getDiff` read RPC: the hunk list of one path. `staged=false` diffs
/// the worktree against the index, `staged=true` the index against HEAD.
/// The engine is the only hunk source — the extension renders hunks from
/// this response and echoes only line RANGES back, which the write path
/// re-validates against a freshly computed diff (see
/// `write_queue::stage_ranges`).
pub fn read_diff(session: &Session, path: &str, staged: bool) -> anyhow::Result<GetDiffResult> {
    if path.is_empty() || path.starts_with('-') || path.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("invalid path: {path:?}");
    }

    let mut args: Vec<&str> = vec!["diff"];
    args.extend(DIFF_FLAGS.iter().copied());
    if staged {
        args.push("--cached");
    }
    args.push("--");
    let literal = format!(":(literal){path}");
    args.push(&literal);

    let (ok, stdout, stderr) = git_output(&session.repo_path, &args)?;
    if !ok {
        return Err(BambooError::git_error(format!(
            "git diff failed for {path:?}: {}",
            redact_credentials(&stderr).trim()
        ))
        .into());
    }

    let raw = parse_unified_diff(stdout.as_bytes());
    let hunks: Vec<_> = raw
        .hunks
        .iter()
        .map(|h| DiffHunk {
            old_start: u32::try_from(h.old_start).unwrap_or(u32::MAX),
            old_lines: u32::try_from(h.old_count).unwrap_or(u32::MAX),
            new_start: u32::try_from(h.new_start).unwrap_or(u32::MAX),
            new_lines: u32::try_from(h.new_count).unwrap_or(u32::MAX),
            lines: h
                .lines
                .iter()
                .map(|l| DiffLine {
                    kind: match l.origin {
                        b'+' => DiffLineKind::Added,
                        b'-' => DiffLineKind::Removed,
                        _ => DiffLineKind::Context,
                    },
                    // Lossy UTF-8 is display-only here: the write path
                    // re-runs git diff itself and works on raw bytes.
                    text: String::from_utf8_lossy(&l.text).into_owned(),
                })
                .collect(),
        })
        .collect();

    info!(
        path,
        staged,
        hunks = hunks.len(),
        binary = raw.binary,
        "diff read"
    );
    Ok(GetDiffResult {
        binary: raw.binary,
        hunks,
    })
}

// ---------------------------------------------------------------------------
// Blame (getBlame): line-level attribution
// ---------------------------------------------------------------------------

/// Upper bound for the dirty-buffer payload (`GetBlame::contents`). Sized
/// BELOW the 16 MiB framing cap (`server::MAX_CONTENT_LENGTH`) on purpose:
/// a message that big never reaches a handler at all — it closes the
/// connection — so an oversized buffer must be rejected here, in band, with
/// a save-first hint the client can show. JSON escaping inflates the
/// payload further, hence the headroom.
const MAX_BLAME_CONTENTS_BYTES: usize = 8 * 1024 * 1024;

/// The null object id both blame implementations attribute uncommitted
/// lines to (`gix_blame::Start::Contents` for buffer-only lines, `git blame`
/// for dirty worktree lines). It is never a real commit, so its metadata is
/// synthesized by the join instead of decoded.
const NULL_COMMIT_ID: &str = "0000000000000000000000000000000000000000";

/// Which implementation a blame read runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameBackend {
    /// gix-blame, falling back to `git blame --porcelain` on any gix error.
    GixThenGitFallback,
    /// Only the porcelain fallback. Test hook: a gix failure cannot be
    /// injected through the public API, and the fallback's parsed output
    /// still has to be proven equivalent to the gix path.
    GitPorcelainOnly,
}

/// Which bytes a blame walk attributes lines to. The three cases exist so
/// the gix path and `git blame` answer the same question (see
/// `read_blame_with`): blaming HEAD's blob while the editor shows a dirtier
/// file misaligns every annotation below the first change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlamedContent {
    /// The caller's unsaved editor buffer (`GetBlame::contents`).
    Buffer,
    /// The worktree file, read from disk and blamed against HEAD.
    Worktree,
    /// The revision's own blob — nothing dirty is involved.
    Revision,
}

/// One attributed run of lines, before commit metadata is joined. Both
/// implementations produce this shape so the join — and therefore the wire
/// format — is identical no matter which one ran.
struct RawBlameHunk {
    /// First line in the blamed file, 0-based.
    start_line: u32,
    len: u32,
    commit_id: String,
    /// The file's path at `commit_id` when it differs from the blamed path
    /// (rename tracking).
    source_path: Option<String>,
}

/// `getBlame` read RPC: attribute every line of `path` to the commit that
/// introduced it, with commit metadata joined engine-side so the client
/// renders gutter annotations and hovers from a single round trip.
///
/// Session-gated and epoch-checked like every other read: the blame walk
/// can span hundreds of commits, and a repository that moved underneath it
/// would produce attribution mixing two states.
///
/// WHICH bytes are annotated follows `git blame`: a caller-supplied buffer,
/// else the worktree file (blamed against HEAD), else the revision's blob.
/// Entry line numbers refer to that content, and the decision is made (and
/// explained) in `read_blame_with`.
pub fn read_blame(session: &Session, req: &GetBlame) -> anyhow::Result<GetBlameResult> {
    read_blame_with(session, req, BlameBackend::GixThenGitFallback)
}

/// `read_blame` with an explicit backend; the split exists so tests can
/// force the porcelain fallback (see [`BlameBackend`]).
pub fn read_blame_with(
    session: &Session,
    req: &GetBlame,
    backend: BlameBackend,
) -> anyhow::Result<GetBlameResult> {
    // Input validation is structured (INVALID_ARGUMENT) rather than a bare
    // bail: all three failures are things the client can act on — a bad
    // path, a bad revision, or a buffer it should save first.
    validate_path_arg(&req.path).map_err(|e| BambooError::invalid_argument(e.to_string()))?;
    // Containment, on top of that. Blame is the only read that opens a
    // worktree FILE from the client's path; every other path argument is
    // handed to git, which resolves pathspecs inside the repository and
    // refuses to leave it. `validate_path_arg` rejects option-like and
    // control-character paths but neither absolute ones nor `..`, and
    // joining either onto the worktree root would read outside it.
    //
    // Decided by COMPONENT, not by `is_absolute`, because the two disagree
    // exactly where it matters: on Windows `/etc/passwd` is NOT absolute (no
    // drive prefix), yet `root.join("/etc/passwd")` discards everything after
    // the prefix and lands at `C:\etc\passwd` — outside the worktree. Requiring
    // plain relative components is the only form that joins inside the root on
    // every platform.
    let blame_path = std::path::Path::new(&req.path);
    if !blame_path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(BambooError::invalid_argument(format!(
            "getBlame: path must be a plain repository-relative path \
             (no leading '/', no drive, no '..'): {:?}",
            req.path
        ))
        .into());
    }
    let rev_spec = req.rev.as_deref().unwrap_or("HEAD");
    validate_blame_rev(rev_spec).map_err(|e| BambooError::invalid_argument(e.to_string()))?;
    if let Some(contents) = &req.contents {
        if contents.len() > MAX_BLAME_CONTENTS_BYTES {
            return Err(BambooError::invalid_argument(format!(
                "getBlame: contents is {} bytes, above the {} byte limit; \
                 save the file and blame the worktree copy instead",
                contents.len(),
                MAX_BLAME_CONTENTS_BYTES
            ))
            .into());
        }
    }

    let current_epoch = session.current_epoch();
    // `via` for the early returns below, where no implementation ran: report
    // the one that WOULD have, so a caller that forced the porcelain backend
    // is not told the gix path answered (the equivalence tests check this).
    let nominal_via = match backend {
        BlameBackend::GitPorcelainOnly => BlameVia::GitPorcelain,
        BlameBackend::GixThenGitFallback => BlameVia::Gix,
    };

    // STAGE 1 — everything that needs the repository: resolve the revision,
    // decide which bytes are blamed, run the gix walk. The lock is scoped to
    // this block because stage 2 may spawn `git blame`, and a reader must not
    // sit on the session locks across a process spawn (the convention
    // `read_commit_detail` documents): a slow or hung child would otherwise
    // stall every other read, and the write queue with it.
    // Poison-tolerant locks, as in every other read: a panicked request
    // handler must not take down the whole connection.
    let (rev_out, blamed_commit, gix_hunks, porcelain_total) = {
        let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());

        let Some(blamed_commit) = resolve_blame_commit(&repo, rev_spec)? else {
            // Unborn HEAD (a repo without a single commit): there is nothing
            // to attribute lines to. Empty entries instead of an error, so
            // toggling blame in a fresh repo is a no-op the client need not
            // special-case — the same convention `read_blob` uses for an
            // absent path.
            return Ok(GetBlameResult {
                rev: String::new(),
                entries: Vec::new(),
                via: nominal_via,
            });
        };
        let rev_out = blamed_commit.to_string();

        // Which bytes the walk attributes lines to, mirroring `git blame`'s
        // own semantics so the porcelain fallback agrees by construction
        // instead of by luck:
        //   contents given          → the caller's buffer is the blamed file
        //   no contents, no rev     → the WORKTREE file is the blamed file
        //   no contents, rev given  → the file as of that revision
        // The worktree case is what a gutter renders against. Blaming HEAD's
        // blob for a file the editor shows with extra lines would misalign
        // every annotation below the first change — the line numbers on the
        // wire refer to the blamed content, so that content has to be the
        // content the user is looking at.
        let blamed = match (&req.contents, req.rev.as_deref()) {
            (Some(_), _) => BlamedContent::Buffer,
            (None, None) => BlamedContent::Worktree,
            (None, Some(_)) => BlamedContent::Revision,
        };
        // A worktree path with no regular file behind it (deleted, a symlink,
        // a directory) has no lines to annotate: answer empty rather than
        // blaming HEAD's copy — which would render against a buffer that is
        // gone — or letting `git blame` fail on a missing path.
        let worktree_bytes = match blamed {
            BlamedContent::Worktree => read_worktree_blame_bytes(&session.repo_path, &req.path)?,
            _ => None,
        };
        if matches!(blamed, BlamedContent::Worktree) && worktree_bytes.is_none() {
            debug!(path = %req.path, "blame: no worktree file to annotate");
            return Ok(GetBlameResult {
                rev: rev_out,
                entries: Vec::new(),
                via: nominal_via,
            });
        }
        let contents_bytes: Option<&[u8]> = match (&blamed, &worktree_bytes) {
            (BlamedContent::Buffer, _) => req.contents.as_deref().map(str::as_bytes),
            (BlamedContent::Worktree, Some(bytes)) => Some(bytes),
            _ => None,
        };

        // A path that is not in the blamed commit's tree has no history to
        // suspect. When there IS content to show (an unsaved new file, or an
        // untracked one) every line is by definition new, so attribute it all
        // to the null id — which is also what both implementations would have
        // to be talked into, since neither blames a path absent from the
        // revision. With no content either (absent path, or a `rev` that
        // predates the file) answer empty, the convention `read_blob` uses.
        if tree_entry_at(&repo, blamed_commit, &req.path)?.is_none() {
            let entries = match contents_bytes {
                Some(bytes) if !bytes.is_empty() => {
                    vec![BlameEntry {
                        start_line: 0,
                        len: blame_line_count(bytes)?,
                        commit: uncommitted_blame_commit(),
                        source_path: None,
                    }]
                }
                _ => Vec::new(),
            };
            return Ok(GetBlameResult {
                rev: rev_out,
                entries,
                via: nominal_via,
            });
        }

        // Line endings have to match what the walk diffs against (see
        // `normalize_blame_eol`); `None` means the revision's own blob is the
        // blamed content and there is nothing to convert.
        let normalized = match contents_bytes {
            Some(bytes) => Some(normalize_blame_eol(&repo, blamed_commit, &req.path, bytes)?),
            None => None,
        };
        let blame_bytes: Option<&[u8]> = normalized.as_deref();

        let gix_hunks = match backend {
            BlameBackend::GitPorcelainOnly => None,
            BlameBackend::GixThenGitFallback => Some(
                blame_via_gix(&repo, &req.path, blamed_commit, blame_bytes)
                    .and_then(|outcome| hunks_from_gix(&outcome, &req.path)),
            ),
        };
        // What the porcelain fallback's hunks must add up to, when that is
        // predictable. An OWNED cow means we converted line endings, so git
        // will convert them its own way (a path-level `.gitattributes` can
        // override `core.autocrlf`) and a total computed here would be a
        // guess; the revision's blob is not read either, because porcelain
        // may never run. Both are settled in stage 3, only if it did.
        let porcelain_total = match &normalized {
            Some(std::borrow::Cow::Borrowed(bytes)) => {
                PorcelainTotal::Lines(count_blame_lines(bytes))
            }
            Some(std::borrow::Cow::Owned(_)) => PorcelainTotal::Deferred,
            None => PorcelainTotal::RevisionBlob,
        };
        (rev_out, blamed_commit, gix_hunks, porcelain_total)
    };

    // STAGE 2 — the porcelain fallback, with no session lock held.
    let (raw, via) = match gix_hunks {
        Some(Ok(hunks)) => (hunks, BlameVia::Gix),
        other => {
            // The fallback is a degradation, not a silent one: log it here and
            // report it on the wire (`via`) so a file that always takes this
            // path stays diagnosable.
            if let Some(Err(e)) = &other {
                warn!(
                    path = %req.path,
                    error = %e,
                    "gix blame failed; falling back to git blame --porcelain"
                );
            }
            (
                // Only a caller-supplied buffer goes over stdin, and it goes
                // RAW: git applies its own clean filter to `--contents=-`,
                // which is the same conversion `normalize_blame_eol` mirrors
                // for the gix path. The worktree case deliberately passes
                // nothing — git reads the file itself.
                blame_via_git(
                    &session.repo_path,
                    &req.path,
                    req.rev.as_deref(),
                    req.contents.as_deref(),
                )?,
                BlameVia::GitPorcelain,
            )
        }
    };

    // STAGE 3 — the metadata join, which needs the repository again (cache
    // misses are decoded from objects) and the cache.
    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
    let cache = session.cache.lock().unwrap_or_else(|e| e.into_inner());
    if via == BlameVia::GitPorcelain {
        // The gix path validates its own tiling before it may answer; the
        // fallback has to satisfy the same invariant, or the line numbers on
        // the wire describe a file the client is not looking at. (A file
        // edited between stage 1's read and git's own surfaces here as a
        // mismatch — the honest answer, and the client asks again on the next
        // keystroke pause.)
        let expected = match porcelain_total {
            PorcelainTotal::Lines(n) => Some(n),
            PorcelainTotal::RevisionBlob => {
                Some(match blob_at_path(&repo, blamed_commit, &req.path)? {
                    Some(blob) => count_blame_lines(&blob.data),
                    None => 0,
                })
            }
            PorcelainTotal::Deferred => None,
        };
        validate_hunk_tiling(&raw, expected, &req.path)?;
    }
    let entries = join_blame_metadata(&repo, &cache, &raw)?;

    // Epoch TOCTOU guard, same shape as read_graph_page: if the watcher
    // bumped the epoch while we walked history, the attribution may straddle
    // two repository states. Fail instead of letting the client cache a mix.
    if session.current_epoch() != current_epoch {
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
    }

    info!(
        path = %req.path,
        rev = %rev_out,
        hunks = entries.len(),
        dirty = req.contents.is_some(),
        ?via,
        "blame read"
    );
    Ok(GetBlameResult {
        rev: rev_out,
        entries,
        via,
    })
}

/// Validate a revision expression for blame. Deliberately NOT
/// `validate_ref_arg`: that one validates ref NAMES handed to git as
/// positional arguments (it rejects `@` and `..`), while blame takes a
/// revspec resolved in-process by gix, where `<sha>^` and `HEAD~2` are
/// exactly the forms the annotate-previous chain needs. What matters here
/// is what `read_blob` already enforces: non-empty, not option-like, no
/// control characters. gix parses it — no shell, no argv, is involved.
fn validate_blame_rev(rev: &str) -> anyhow::Result<()> {
    if rev.is_empty() {
        anyhow::bail!("revision must not be empty");
    }
    if rev.starts_with('-') {
        anyhow::bail!("revision may not begin with '-': {rev:?}");
    }
    if rev.len() > 512 || rev.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("invalid revision: {rev:?}");
    }
    Ok(())
}

/// Resolve the revision to blame into a commit id. `None` means unborn
/// HEAD, which callers answer with empty entries rather than an error.
fn resolve_blame_commit(
    repo: &gix::Repository,
    rev: &str,
) -> anyhow::Result<Option<gix::ObjectId>> {
    match repo.rev_parse_single(rev.as_bytes()) {
        Ok(id) => {
            let commit = id
                .object()
                .map_err(|e| {
                    BambooError::git_error(format!("revision {rev:?} does not resolve: {e}"))
                })?
                .peel_to_commit()
                .map_err(|e| {
                    BambooError::git_error(format!("revision {rev:?} is not a commit: {e}"))
                })?;
            Ok(Some(commit.id))
        }
        Err(e) => {
            // Tell a fresh repository apart from a bad revision: only a HEAD
            // with no commit behind it is the former. A typo'd sha must stay
            // a visible error instead of silently rendering no annotations.
            if rev == "HEAD" && repo.head_commit().is_err() {
                return Ok(None);
            }
            Err(BambooError::git_error(format!("revision {rev:?} does not resolve: {e}")).into())
        }
    }
}

/// The object id `path` names in `commit`'s tree, or None when the path is
/// absent. Shared by the three places that ask "what is at this path" — the
/// absent-path early return, the CR probe, and the porcelain line count —
/// which each used to walk commit → tree → entry themselves.
fn tree_entry_at(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    path: &str,
) -> anyhow::Result<Option<gix::ObjectId>> {
    let tree = repo
        .find_object(commit)
        .map_err(|e| anyhow::anyhow!("commit {commit} not found: {e}"))?
        .try_into_commit()
        .map_err(|e| anyhow::anyhow!("{commit} is not a commit: {e}"))?
        .tree()
        .map_err(|e| anyhow::anyhow!("tree of {commit} unreadable: {e}"))?;
    Ok(tree
        .lookup_entry_by_path(path)
        .map_err(|e| anyhow::anyhow!("path {path:?} lookup failed: {e}"))?
        .map(|e| e.id().detach()))
}

/// The blob `path` names in `commit`'s tree, or None when the path is absent.
/// A symlink IS a blob (its content is the target path); a subtree is not, and
/// neither has lines to compare or count.
fn blob_at_path<'repo>(
    repo: &'repo gix::Repository,
    commit: gix::ObjectId,
    path: &str,
) -> anyhow::Result<Option<gix::Blob<'repo>>> {
    let Some(id) = tree_entry_at(repo, commit, path)? else {
        return Ok(None);
    };
    match repo.find_object(id) {
        Ok(obj) => Ok(obj.try_into_blob().ok()),
        Err(e) => Err(anyhow::anyhow!("blob {id} of {path:?} unreadable: {e}")),
    }
}

/// Read the worktree file a blame walk annotates.
///
/// None means "nothing to annotate": a deleted path, a symlink, or a
/// directory. Symlinks are refused deliberately — this is the only read that
/// opens a client-named path itself, and the containment check upstream is
/// LEXICAL, so a tracked symlink (`notes/secret -> ../../.ssh/id_rsa`) passes
/// it and `fs::read` would follow it out of the worktree. Blaming the link's
/// own blob (what `git blame` does) is not the alternative either: that numbers
/// lines against a one-line target path while the editor shows the resolved
/// file, so every annotation below line 1 would be wrong.
///
/// A file that is present but unreadable IS an error, not None: a permissions
/// or I/O failure should surface once as a structured error rather than render
/// as "this file has no history".
fn read_worktree_blame_bytes(
    root: &std::path::Path,
    path: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let full = root.join(path);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("cannot stat {path:?}: {e}")),
    };
    if meta.file_type().is_symlink() {
        warn!(path = %path, "blame: refusing to follow a symlink out of the worktree");
        return Ok(None);
    }
    if !meta.is_file() {
        debug!(path = %path, "blame: not a regular file, nothing to annotate");
        return Ok(None);
    }
    if meta.len() as usize > MAX_BLAME_CONTENTS_BYTES {
        return Err(BambooError::invalid_argument(format!(
            "getBlame: {path:?} is {} bytes, above the {} byte limit; \
             blame a revision of it instead",
            meta.len(),
            MAX_BLAME_CONTENTS_BYTES
        ))
        .into());
    }
    std::fs::read(&full)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("cannot read {path:?}: {e}"))
}

/// Run gix-blame over the whole file.
///
/// The commit-graph is passed EXPLICITLY. `gix_blame::file` takes
/// `cache: Option<gix_commitgraph::Graph>`, and without it every history
/// step re-decodes a commit object from the odb — the measurements that made
/// gix-blame viable at Chromium scale (14.9s → 290ms) come from the graph.
/// `Repository::blame_file()` would pass it for us, but it only supports
/// `Start::Commit`; the dirty-buffer variant (`Start::Contents`) has no
/// convenience wrapper, so both paths are spelled out here to keep the
/// graph argument visible in one place.
fn blame_via_gix(
    repo: &gix::Repository,
    path: &str,
    commit: gix::ObjectId,
    contents: Option<&[u8]>,
) -> anyhow::Result<gix::blame::Outcome> {
    let cache = repo.commit_graph_if_enabled()?;
    let mut resource_cache = repo.diff_resource_cache_for_tree_diff()?;
    let options = gix::blame::Options {
        diff_algorithm: repo.diff_algorithm()?,
        // WholeFile only, on purpose: `BlameRanges::PartialFile` panics on
        // empty, inverted or overlapping ranges (upstream fix unmerged as of
        // gix-blame 0.17), and we never blame a sub-range — viewport
        // slicing is a client-side rendering concern, not a blame one.
        ranges: gix::blame::BlameRanges::WholeFile,
        since: None,
        // Rename tracking on (copies off) so a renamed file's history does
        // not stop at the rename with every older line attributed to the
        // renaming commit. `git blame` follows renames by default too, so the
        // porcelain fallback reports the same `source_path` — asserted in
        // `blame_follows_renames_and_reports_the_source_path`. Detected COPIES
        // are where the two differ: neither backend looks for them here.
        rewrites: Some(gix::diff::Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            track_empty: false,
        }),
        debug_track_path: false,
    };
    let start = match contents {
        // Dirty-content blame (an editor buffer or the worktree copy): the
        // bytes replace the worktree file as the *blamed file* and `commit`
        // becomes the first suspect, so lines that exist only in `contents`
        // come back as the null id. Line endings are already normalized by
        // the caller (`normalize_blame_eol`); other clean filters — LFS
        // pointers, `ident`, custom drivers — are NOT applied, so such a
        // file blames as its worktree text. Blame is display-only, which is
        // what makes that boundary acceptable.
        Some(c) => gix::blame::Start::Contents {
            first_suspect: commit,
            contents: std::borrow::Cow::Borrowed(c),
        },
        None => gix::blame::Start::Commit(commit),
    };
    let path = path.as_bytes().into();
    let outcome = gix::blame::file(
        &repo.objects,
        start,
        cache,
        &mut resource_cache,
        path,
        options,
    )?;
    Ok(outcome)
}

/// Convert a gix outcome into raw hunks, validating that the entries TILE the
/// blamed content.
fn hunks_from_gix(outcome: &gix::blame::Outcome, path: &str) -> anyhow::Result<Vec<RawBlameHunk>> {
    let hunks = outcome
        .entries
        .iter()
        .map(|e| RawBlameHunk {
            start_line: e.start_in_blamed_file,
            len: e.len.get(),
            commit_id: e.commit_id.to_string(),
            // `source_file_name` is set only when the *source* file's name
            // differs from the blamed file's (a rename in the chain).
            source_path: e
                .source_file_name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .filter(|n| n != path),
        })
        .collect::<Vec<_>>();
    // `start_in_blamed_file` is a TOKEN index into the blob split by
    // `byte_lines` (newline included in the token), so the blob's own line
    // count is the total the hunks must reach.
    validate_hunk_tiling(&hunks, Some(count_blame_lines(&outcome.blob)), path)?;
    Ok(hunks)
}

/// Reject a hunk list that does not describe a whole file: hunks must be
/// contiguous, start at line 0, and — when `expected` is known — cover exactly
/// the blamed content's lines.
///
/// A mismatch means our line model and the implementation's disagree (CRLF, no
/// trailing newline, a filter boundary, a parse bug), and answering anyway
/// would silently mislabel every line after the gap: better to fail. For the
/// gix path the error triggers the porcelain fallback; for porcelain itself it
/// is a structured error, since there is nothing left to fall back to.
fn validate_hunk_tiling(
    hunks: &[RawBlameHunk],
    expected: Option<usize>,
    path: &str,
) -> anyhow::Result<()> {
    let mut covered: u32 = 0;
    for h in hunks {
        if h.start_line != covered {
            anyhow::bail!(
                "blame for {path:?} is not contiguous at line {covered} (next hunk starts at {})",
                h.start_line
            );
        }
        covered = covered.saturating_add(h.len);
    }
    if let Some(expected) = expected {
        if covered as usize != expected {
            anyhow::bail!(
                "blame for {path:?} covers {covered} lines but the blamed content has {expected}"
            );
        }
    }
    Ok(())
}

/// What the porcelain fallback's hunks must add up to. Settled in stage 3 of
/// `read_blame_with`, only if porcelain actually ran.
#[derive(Debug, Clone, Copy)]
enum PorcelainTotal {
    /// A known line count.
    Lines(usize),
    /// The blamed revision's blob: read it then, not before.
    RevisionBlob,
    /// Not predictable — we converted line endings, so git's own conversion
    /// decides the line model. Contiguity is still checked.
    Deferred,
}

/// Convert CRLF line endings in the blamed bytes to LF — the way git's
/// clean filter does — and ONLY when the repository is configured for that
/// conversion.
///
/// Why it is needed: with `core.autocrlf=true` (the Windows default) the
/// worktree copy and any editor buffer are CRLF while blobs stay LF. Diffing
/// the raw bytes against the blob marks every line modified, so an entire
/// file would render as "Not Committed Yet" while `git blame` — which
/// converts first — shows the real attribution.
///
/// Why it is gated on the config: with no conversion configured git compares
/// the bytes it finds, and so must we. Normalizing unconditionally would hide
/// genuinely changed line endings and diverge from the porcelain fallback,
/// which delegates the decision to git.
///
/// Boundary: only `core.autocrlf` is consulted. A path-specific
/// `.gitattributes` `text=`/`eol=` override needs the attribute stack (an
/// index pass per call); blame is display-only, so that refinement is
/// deliberately not paid for.
fn normalize_blame_eol<'a>(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    path: &str,
    contents: &'a [u8],
) -> anyhow::Result<std::borrow::Cow<'a, [u8]>> {
    if !autocrlf_converts(repo) || !contents.contains(&b'\r') {
        return Ok(std::borrow::Cow::Borrowed(contents));
    }
    // A blob that itself contains CR is compared byte for byte: stripping
    // would make every line of a file committed WITH CRLF look new.
    if blob_has_cr(repo, commit, path)? {
        return Ok(std::borrow::Cow::Borrowed(contents));
    }
    let mut out = Vec::with_capacity(contents.len());
    for (i, &b) in contents.iter().enumerate() {
        // Drop the CR of a CRLF pair; its LF is pushed on the next round. A
        // lone CR (old-Mac endings, or a CR inside a line) is content and
        // stays.
        if b == b'\r' && contents.get(i + 1) == Some(&b'\n') {
            continue;
        }
        out.push(b);
    }
    Ok(std::borrow::Cow::Owned(out))
}

/// Whether `core.autocrlf` makes git convert CRLF→LF on the way in. Both
/// `true` and `input` clean that way; `false` and any unrecognized value
/// mean git compares the bytes it finds.
fn autocrlf_converts(repo: &gix::Repository) -> bool {
    let Some(value) = repo.config_snapshot().string("core.autocrlf") else {
        return false;
    };
    let value: &[u8] = &value[..];
    // git accepts several spellings for a boolean; `input` is the one
    // non-boolean value that also cleans CRLF→LF.
    value.eq_ignore_ascii_case(b"true")
        || value.eq_ignore_ascii_case(b"input")
        || value.eq_ignore_ascii_case(b"yes")
        || value.eq_ignore_ascii_case(b"on")
        || value.eq_ignore_ascii_case(b"1")
}

/// Whether `path`'s blob at `commit` contains a CR byte (see
/// `normalize_blame_eol`). A missing or non-blob entry counts as "no CR":
/// there is then nothing to compare against and the caller keeps the bytes
/// verbatim.
fn blob_has_cr(repo: &gix::Repository, commit: gix::ObjectId, path: &str) -> anyhow::Result<bool> {
    Ok(match blob_at_path(repo, commit, path)? {
        Some(blob) => blob.data.contains(&b'\r'),
        None => false,
    })
}

/// Line count under the tokenizer gix-blame uses (`byte_lines`: the newline
/// is part of the token). An empty blob has zero lines; a final line without
/// a trailing newline still counts as one.
fn count_blame_lines(blob: &[u8]) -> usize {
    blob.split_inclusive(|b| *b == b'\n').count()
}

/// `count_blame_lines` as the u32 the wire type uses. More than four billion
/// lines cannot happen, but a silent `as` truncation would put line numbers on
/// the wire that do not describe the content.
fn blame_line_count(blob: &[u8]) -> anyhow::Result<u32> {
    u32::try_from(count_blame_lines(blob))
        .map_err(|_| anyhow::anyhow!("blamed content has more lines than u32::MAX"))
}

/// `git blame --porcelain` — the fallback for when the gix path fails
/// (pathological files, an API boundary we do not cover) and the backend
/// tests force.
///
/// Boundary: porcelain output is read as lossy UTF-8, so a non-UTF-8 author
/// name or summary can arrive mangled. Acceptable because blame is
/// display-only and no write path consumes it — the same boundary
/// `read_blob` already documents for file content.
fn blame_via_git(
    repo_path: &std::path::Path,
    path: &str,
    rev: Option<&str>,
    contents: Option<&str>,
) -> anyhow::Result<Vec<RawBlameHunk>> {
    let mut cmd = std::process::Command::new("git");
    // `core.quotepath=false`: porcelain C-quotes any non-ASCII path
    // (`filename "notes-\303\251.md"`), and a quoted name is neither
    // displayable in the hover nor a path the client can open for
    // annotate-previous. `-z` does not help — blame quotes the filename
    // either way.
    cmd.arg("-c").arg("core.quotepath=false");
    cmd.arg("blame").arg("--porcelain");
    if contents.is_some() {
        // `-` = read the blamed contents from stdin, so an unsaved buffer
        // never has to be written to a temp file (and never touches disk).
        cmd.arg("--contents=-");
    }
    if let Some(rev) = rev {
        cmd.arg(rev);
    }
    // Plain path after `--`: `git blame` does NOT understand pathspec magic
    // (`:(literal)…` is taken as a filename and fails with "no such path"),
    // and `--` already ends option parsing so a leading `-` cannot be read
    // as a flag (validate_path_arg rejects those anyway).
    cmd.arg("--").arg(path);
    cmd.current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        // git would otherwise opportunistically refresh the index, i.e. take
        // `index.lock` — which the write queue needs. Blame can fire once per
        // keystroke pause, so a read must never contend for it.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if contents.is_some() {
            std::process::Stdio::piped()
        } else {
            // Closed stdin: a read must never hang waiting for terminal input.
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    if let Some(contents) = contents {
        // Written from a thread, not inline: stdout and stderr are pipes
        // nobody drains until `wait_with_output`, so a child that fills a
        // 64 KiB pipe while we are still writing up to 8 MiB would block
        // forever — and take a blocking-pool thread with it.
        let bytes = contents.as_bytes().to_vec();
        let mut stdin = child.stdin.take();
        std::thread::spawn(move || {
            use std::io::Write;
            // EPIPE here just means git died early; its stderr is the real
            // error, so a failed write is deliberately ignored.
            if let Some(mut stdin) = stdin.take() {
                let _ = stdin.write_all(&bytes);
            }
        });
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(BambooError::git_error(format!(
            "git blame failed for {path:?}: {}",
            redact_credentials(&String::from_utf8_lossy(&out.stderr)).trim()
        ))
        .into());
    }
    Ok(parse_blame_porcelain(
        &String::from_utf8_lossy(&out.stdout),
        path,
    ))
}

/// One porcelain record header: `<sha> <source-line> <final-line> [<lines>]`.
/// Line numbers are 1-based; the optional fourth field appears only on the
/// first record of a group and is ignored (we merge records ourselves).
struct PorcelainRecord {
    commit_id: String,
    source_line: u32,
    final_line: u32,
}

fn parse_porcelain_record_header(line: &str) -> Option<PorcelainRecord> {
    let mut fields = line.split(' ');
    let commit_id = fields.next()?;
    // Content lines start with a tab and keyword lines (`author`, `summary`,
    // `filename`, …) are not hex, so a hex id of plausible length is enough
    // to identify a record header.
    if !(40..=64).contains(&commit_id.len()) || !commit_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(PorcelainRecord {
        commit_id: commit_id.to_string(),
        source_line: fields.next()?.parse().ok()?,
        final_line: fields.next()?.parse().ok()?,
    })
}

/// Parse `git blame --porcelain` into hunks. Porcelain emits one record per
/// LINE (only the first record of a group carries the full header), so
/// consecutive records with the same commit AND consecutive positions in
/// both the blamed and the source file are merged back into the hunks the
/// gix path produces natively — the client must not see a different shape
/// depending on which implementation ran.
fn parse_blame_porcelain(out: &str, path: &str) -> Vec<RawBlameHunk> {
    let mut hunks: Vec<RawBlameHunk> = Vec::new();
    // `filename` is emitted only in a group's full header, but every record
    // of that group needs it to merge — remember it per commit.
    let mut filename_by_commit: HashMap<String, String> = HashMap::new();
    // Where the next record's source line must land to extend the open hunk.
    let mut next_source_line: u32 = 0;

    let mut lines = out.lines();
    while let Some(line) = lines.next() {
        let Some(rec) = parse_porcelain_record_header(line) else {
            continue;
        };
        // Consume the rest of the record: header fields up to the content
        // line, which is the first one starting with a tab.
        let mut filename = None;
        for header in lines.by_ref() {
            if header.starts_with('\t') {
                break;
            }
            if let Some(name) = header.strip_prefix("filename ") {
                filename = Some(name.to_string());
            }
        }
        if let Some(name) = filename {
            filename_by_commit.insert(rec.commit_id.clone(), name);
        }
        // A rename shows up as a `filename` that differs from the blamed
        // path; the null id (uncommitted lines) never carries one.
        let source_path = filename_by_commit
            .get(&rec.commit_id)
            .filter(|n| n.as_str() != path)
            .cloned();

        let start_line = rec.final_line.saturating_sub(1);
        let extends_open_hunk = match hunks.last() {
            Some(last) => {
                last.commit_id == rec.commit_id
                    && last.source_path == source_path
                    && last.start_line + last.len == start_line
                    && next_source_line == rec.source_line
            }
            None => false,
        };
        if extends_open_hunk {
            if let Some(last) = hunks.last_mut() {
                last.len += 1;
            }
        } else {
            hunks.push(RawBlameHunk {
                start_line,
                len: 1,
                commit_id: rec.commit_id.clone(),
                source_path,
            });
        }
        next_source_line = rec.source_line.saturating_add(1);
    }
    hunks
}

/// Attach commit metadata to the raw hunks in ONE pass over the DISTINCT
/// commit ids — the client must never need a per-entry RPC, which is the
/// whole reason the join happens engine-side. redb COMMIT_META first (keyed
/// by immutable object id, so a hit cannot go stale), gix decode for the
/// misses, written back in a single transaction.
fn join_blame_metadata(
    repo: &gix::Repository,
    cache: &crate::cache::Cache,
    hunks: &[RawBlameHunk],
) -> anyhow::Result<Vec<BlameEntry>> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for h in hunks {
        // A file whose lines come from two or three commits is the common
        // case, so dedupe before decoding anything — O(1) per hunk rather
        // than a linear scan of `ids`.
        if h.commit_id != NULL_COMMIT_ID && seen.insert(h.commit_id.as_str()) {
            ids.push(h.commit_id.clone());
        }
    }

    let cached = cache.get_commit_metas(&ids)?;
    let mut metas: HashMap<String, BlameCommit> = HashMap::with_capacity(ids.len());
    let mut misses: Vec<(String, CachedCommit)> = Vec::new();
    for id in &ids {
        // `committer_time: None` marks an entry written before that field
        // existed (or by a reader that does not decode the committer) — a
        // PARTIAL hit, so fall through and refresh it instead of answering 0.
        if let Some(meta) = cached.get(id) {
            if let Some(committer_time) = meta.committer_time {
                metas.insert(
                    id.clone(),
                    BlameCommit {
                        id: id.clone(),
                        author_name: meta.author_name.clone(),
                        author_email: meta.author_email.clone(),
                        author_time: meta.author_time,
                        committer_time,
                        summary: meta.message.clone(),
                    },
                );
                continue;
            }
        }
        match decode_blame_commit(repo, id) {
            Ok((commit, cached_commit)) => {
                metas.insert(id.clone(), commit);
                misses.push((id.clone(), cached_commit));
            }
            // Shallow clones and partial filters legitimately lack the
            // object: keep the attribution (the client links on the id) with
            // placeholder metadata rather than failing the whole read.
            Err(e) => warn!(commit = %id, error = %e, "blame: commit metadata unavailable"),
        }
    }
    if let Err(e) = cache.put_commit_metas(&misses) {
        warn!(error = %e, "failed to cache blame commit metadata");
    }

    // Uncommitted lines: both implementations report the null id, so the
    // metadata is synthesized engine-side to keep the two paths identical.
    let uncommitted = uncommitted_blame_commit();

    Ok(hunks
        .iter()
        .map(|h| BlameEntry {
            start_line: h.start_line,
            len: h.len,
            commit: if h.commit_id == NULL_COMMIT_ID {
                uncommitted.clone()
            } else {
                metas.get(&h.commit_id).cloned().unwrap_or(BlameCommit {
                    id: h.commit_id.clone(),
                    author_name: "Unknown".to_string(),
                    author_email: String::new(),
                    author_time: 0,
                    committer_time: 0,
                    summary: String::new(),
                })
            },
            source_path: h.source_path.clone(),
        })
        .collect())
}

/// Metadata for the null id, i.e. lines that are not committed yet.
/// Synthesized rather than decoded (the null id is not an object), and
/// identical no matter which implementation produced the hunk: git would
/// otherwise say "External file (--contents)" with the current timestamp for
/// a dirty buffer and gix would say nothing at all, so the gutter text would
/// depend on which backend happened to run.
fn uncommitted_blame_commit() -> BlameCommit {
    BlameCommit {
        id: NULL_COMMIT_ID.to_string(),
        author_name: "Not Committed Yet".to_string(),
        author_email: String::new(),
        author_time: 0,
        committer_time: 0,
        summary: "Uncommitted changes".to_string(),
    }
}

/// Decode one commit into its wire metadata plus the cache record to store.
/// `message` is the SUBJECT only, matching what the graph reader caches in
/// `CachedCommit::message` (so both readers share entries) and what blame
/// needs for the gutter/hover.
fn decode_blame_commit(
    repo: &gix::Repository,
    id: &str,
) -> anyhow::Result<(BlameCommit, CachedCommit)> {
    let oid = gix::ObjectId::from_hex(id.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid commit id {id:?}: {e}"))?;
    let commit = repo
        .find_object(oid)
        .map_err(|e| anyhow::anyhow!("commit {id} not found: {e}"))?
        .try_into_commit()
        .map_err(|e| anyhow::anyhow!("{id} is not a commit: {e}"))?;
    let commit_ref = commit.decode()?;
    let (author_name, author_email, author_time) = owned_signature(commit_ref.author()?);
    let (_, _, committer_time) = owned_signature(commit_ref.committer()?);
    let summary = String::from_utf8_lossy(&commit_ref.message_summary()).to_string();
    let parent_ids: Vec<String> = commit_ref.parents().map(|p| p.to_string()).collect();

    Ok((
        BlameCommit {
            id: id.to_string(),
            author_name: author_name.clone(),
            author_email: author_email.clone(),
            author_time,
            committer_time,
            summary: summary.clone(),
        },
        CachedCommit {
            message: summary,
            author_name,
            author_email,
            author_time,
            parent_ids,
            committer_time: Some(committer_time),
        },
    ))
}

// ---------------------------------------------------------------------------
// Interactive rebase reads (getRebasePlan / getRebaseStatus)
// ---------------------------------------------------------------------------

/// `getRebasePlan`: the commit chain a "rebase onto X" would replay, in
/// replay order (oldest → newest). Read-only snapshot; the real freshness
/// gate is startRebase's validation (the todo id set must equal the
/// recomputed chain exactly at EXECUTION time), so this read intentionally
/// carries no epoch and tolerates a concurrently moving repository.
pub fn read_rebase_plan(
    session: &Session,
    onto: &str,
    branch: Option<&str>,
) -> anyhow::Result<bamboo_protocol::GetRebasePlanResult> {
    validate_ref_arg(onto)
        .map_err(|e| BambooError::invalid_argument(format!("getRebasePlan: {e}")))?;
    let branch = match branch {
        Some(b) => {
            validate_ref_arg(b)
                .map_err(|e| BambooError::invalid_argument(format!("getRebasePlan: {e}")))?;
            if b == "HEAD" {
                // `branch: null` is the current-branch form; an explicit
                // "HEAD" would silently resolve against detached HEAD.
                return Err(BambooError::invalid_argument(
                    "getRebasePlan: branch must name a local branch; pass branch: null \
                     for the current branch",
                )
                .into());
            }
            Some(b.to_string())
        }
        None => None,
    };

    let git_dir = crate::gitdir::resolve_git_dir(&session.repo_path);
    let branch_name = match branch {
        Some(b) => b,
        None => current_branch_name(&git_dir, "getRebasePlan")?,
    };

    let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
    let onto_id = resolve_rev_commit(&repo, onto, "getRebasePlan: onto")?;
    // A missing/unresolvable branch is a client-input problem, not a git
    // failure: INVALID_ARGUMENT so the client can prompt.
    let branch_id = resolve_rev_commit(
        &repo,
        &format!("refs/heads/{branch_name}"),
        "getRebasePlan: branch",
    )
    .map_err(|_| {
        BambooError::invalid_argument(format!("getRebasePlan: branch {branch_name:?} not found"))
    })?;

    let Some((base, chain)) = rebase_chain(&repo, onto_id, branch_id)? else {
        return Err(BambooError::invalid_argument(
            "getRebasePlan: onto and branch share no common ancestor",
        )
        .into());
    };

    let commits = chain
        .into_iter()
        .map(|c| bamboo_protocol::RebasePlanCommit {
            id: c.id,
            subject: c.subject,
            author: c.author,
            time: c.time,
        })
        .collect();
    Ok(bamboo_protocol::GetRebasePlanResult {
        onto: onto.to_string(),
        upstream: base.to_string(),
        branch: branch_name,
        commits,
    })
}

/// `getRebaseStatus`: direct mapping of the on-disk rebase state. Read-only
/// and session-gated like every other read; detects externally-started
/// rebases too (there is no engine-side in-progress state to compare with).
pub fn read_rebase_status(
    session: &Session,
) -> anyhow::Result<bamboo_protocol::GetRebaseStatusResult> {
    Ok(read_rebase_state(&session.repo_path, &session.git_dir))
}
