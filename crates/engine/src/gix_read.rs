use anyhow::Context;
use std::collections::HashMap;

use tracing::{debug, info, warn};

use git_workbench_protocol::{Commit, GraphPage, GraphViewport, Ref, RefKind, StatusCode, StatusItem, WorkbenchError};

use crate::cache::CachedCommit;
use crate::session::Session;

const PRELOAD_MARGIN: u32 = 500;

pub fn read_graph_page(
    session: &Session,
    viewport: &GraphViewport,
) -> anyhow::Result<GraphPage> {
    let current_epoch = session.current_epoch();
    if viewport.epoch != 0 && viewport.epoch != current_epoch {
        return Err(WorkbenchError::epoch_mismatch(
            current_epoch,
            viewport.epoch,
        )
        .into());
    }

    // Poison-tolerant lock recovery: a panicked request handler must not
    // take down the whole connection.
    let repo = session
        .gix_repo
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let cache = session
        .cache
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Invalidate lane cache if epoch changed.
    let _ = cache.invalidate_on_epoch_change(current_epoch)?;

    // Collect all ref targets plus HEAD as tips.
    let mut tips = Vec::new();
    for reference in repo.references()?.all()? {
        let reference = reference.map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(id) = reference.target().try_id() {
            tips.push(id.to_owned());
        }
    }
    if let Ok(head_commit) = repo.head_commit() {
        tips.push(head_commit.id);
    }

    if tips.is_empty() {
        return Ok(GraphPage {
            commits: Vec::new(),
            total_approx: 0,
            has_more: false,
            epoch: current_epoch,
            anchor_commit: viewport.anchor_commit.clone(),
        });
    }

    // Walk commits newest-first with commit graph acceleration. This pass
    // only enumerates object ids (cheap via the commit-graph); commit
    // metadata is resolved afterwards, cache-first.
    let walk = repo
        .rev_walk(tips)
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .use_commit_graph(true)
        .all()?;

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
        return Err(WorkbenchError::epoch_mismatch(
            session.current_epoch(),
            current_epoch,
        )
        .into());
    }

    // Resolve commit metadata: serve hits from the COMMIT_META cache
    // (keyed by immutable object id, so entries never go stale) and decode
    // only the misses, writing them back in one batch.
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

        let commit_obj = repo.find_object(oid)?;
        let commit = commit_obj.try_into_commit()?;
        let commit_ref = commit.decode()?;

        let message = String::from_utf8_lossy(&commit_ref.message_summary()).to_string();
        let author = commit_ref.author()?;
        let author_name = String::from_utf8_lossy(author.name.as_ref()).to_string();
        let author_email = String::from_utf8_lossy(author.email.as_ref()).to_string();
        let author_time = author.time().map(|t| t.seconds).unwrap_or(0);
        let parent_ids: Vec<String> = commit_ref
            .parents()
            .map(|id| id.to_string())
            .collect();

        misses.push((
            id.clone(),
            CachedCommit {
                message: message.clone(),
                author_name: author_name.clone(),
                author_email: author_email.clone(),
                author_time,
                parent_ids: parent_ids.clone(),
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

    let total_approx = all_commits.len() as u64;
    // NOTE: `all_commits` is walked from row 0 (the rev walk enumerates
    // newest-first from the top), so its length is already a lower bound of
    // the true total — never add `viewport.offset` here.

    // Build children map for lane assignment.
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for commit in &all_commits {
        for parent_id in &commit.parent_ids {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(commit.id.clone());
        }
    }

    // Assign lanes using a sweep-line algorithm, walking newest → oldest.
    // lanes[i] = Some(commit_id) means lane i is occupied by commit_id's line.
    // Lanes are u16: with pathological graphs (> 65 535 concurrent lanes)
    // the assignment collapses instead of panicking — extra commits fall
    // back to lane 0 (see below).
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut commit_lanes: HashMap<String, u16> = HashMap::new();

    for commit in &mut all_commits {
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

    // Persist lanes + the epoch they are valid for in ONE redb transaction.
    let lane_entries: Vec<(String, u16)> = all_commits
        .iter()
        .map(|c| (c.id.clone(), c.lane))
        .collect();
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
        return Err(WorkbenchError::epoch_mismatch(
            session.current_epoch(),
            current_epoch,
        )
        .into());
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
    })
}

pub fn read_status(
    session: &Session,
    paths: Option<&[String]>,
) -> anyhow::Result<Vec<StatusItem>> {
    let current_epoch = session.current_epoch();
    let repo = session
        .gix_repo
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let platform = repo.status(gix::progress::Discard)?;

    // gix status: empty pattern list matches everything.
    let patterns: Vec<gix::bstr::BString> = Vec::new();
    let iter = platform.into_iter(patterns)?;

    let mut items = Vec::new();
    for entry in iter {
        let entry = entry.map_err(|e| anyhow::anyhow!("{e}"))?;

        let item = match entry {
            gix::status::Item::TreeIndex(change) => {
                map_tree_index_change(change).map(|mut i| {
                    i.staged = true;
                    i
                })
            }
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
        return Err(WorkbenchError::epoch_mismatch(
            session.current_epoch(),
            current_epoch,
        )
        .into());
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

fn map_tree_index_change(change: gix::diff::index::Change) -> Option<StatusItem> {
    use gix::diff::index::Change as C;

    match change {
        C::Addition { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Added,
            old_path: None,
            staged: false,
        }),
        C::Deletion { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Deleted,
            old_path: None,
            staged: false,
        }),
        C::Modification { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Modified,
            old_path: None,
            staged: false,
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
            let code = match status {
                ES::Conflict { .. } => StatusCode::Conflict,
                ES::Change(change) => match change {
                    gix::status::plumbing::index_as_worktree::Change::Removed => {
                        StatusCode::Deleted
                    }
                    _ => StatusCode::Modified,
                },
                ES::IntentToAdd => StatusCode::Added,
                ES::NeedsUpdate(_) => return None,
            };
            Some(StatusItem {
                path: rela_path.to_string(),
                status: code,
                old_path: None,
                staged: false,
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
            })
        }
    }
}

pub fn read_refs(session: &Session) -> anyhow::Result<Vec<Ref>> {
    let repo = session
        .gix_repo
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut refs = Vec::new();

    for reference in repo.references()?.all()? {
        let reference = reference.map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = reference.name().as_bstr().to_string();
        let target = reference
            .target()
            .try_id()
            .map(|id| id.to_owned().to_string())
            .unwrap_or_default();

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

    let repo = session
        .gix_repo
        .lock()
        .unwrap_or_else(|e| e.into_inner());

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

/// Read HEAD for the session's repository: the commit id (empty on an
/// unborn branch) plus the short branch name when HEAD is symbolic.
/// Uses the resolved git dir, so linked worktrees work.
pub fn read_head(session: &Session) -> anyhow::Result<git_workbench_protocol::GetHeadResult> {
    let git_dir = crate::gitdir::resolve_git_dir(&session.repo_path);
    let raw = std::fs::read_to_string(git_dir.join("HEAD"))
        .with_context(|| format!("failed to read {}", git_dir.join("HEAD").display()))?;
    let raw = raw.trim().to_string();
    let branch = raw.strip_prefix("ref: refs/heads/").map(str::to_string);

    let repo = session
        .gix_repo
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Unborn HEAD (fresh repo): no commit yet, but the branch name is valid.
    let head = repo
        .head_commit()
        .map(|c| c.id.to_string())
        .unwrap_or_default();

    Ok(git_workbench_protocol::GetHeadResult { head, branch })
}
