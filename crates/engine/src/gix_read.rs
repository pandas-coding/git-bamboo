use std::collections::HashMap;

use tracing::{debug, info, warn};

use git_workbench_protocol::{Commit, GraphPage, GraphViewport, Ref, RefKind, StatusCode, StatusItem};

use crate::session::Session;

const PRELOAD_MARGIN: u32 = 500;

pub fn read_graph_page(
    session: &Session,
    viewport: &GraphViewport,
) -> anyhow::Result<GraphPage> {
    let current_epoch = session.current_epoch();
    if viewport.epoch != 0 && viewport.epoch != current_epoch {
        return Err(git_workbench_protocol::WorkbenchError::epoch_mismatch(
            current_epoch,
            viewport.epoch,
        )
        .into());
    }

    let repo = session.gix_repo.lock().unwrap();
    let cache = session.cache.lock().unwrap();

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

    // Walk commits newest-first with commit graph acceleration.
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

    let mut all_commits: Vec<Commit> = Vec::with_capacity((end.min(10_000)) as usize);

    for (idx, info) in walk.enumerate() {
        let info = info?;
        if idx >= end as usize {
            break;
        }

        let id = info.id.to_string();

        // Commit details require an object lookup.
        let commit_obj = repo.find_object(info.id)?;
        let commit = commit_obj.try_into_commit()?;
        let commit_ref = commit.decode()?;

        let message = String::from_utf8_lossy(&commit_ref.message_summary()).to_string();
        let author = commit_ref.author()?;
        let author_name = String::from_utf8_lossy(author.name.as_ref()).to_string();
        let author_email = String::from_utf8_lossy(author.email.as_ref()).to_string();
        let author_time = author
            .time()
            .map(|t| t.seconds as i64)
            .unwrap_or(0);

        let parent_ids: Vec<String> = commit_ref
            .parents()
            .map(|id| id.to_string())
            .collect();

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

    let total_approx = all_commits.len() as u64 + viewport.offset;

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
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut commit_lanes: HashMap<String, u8> = HashMap::new();

    for commit in &mut all_commits {
        let commit_children = children.get(&commit.id).cloned().unwrap_or_default();

        let lane = match commit_children.len() {
            0 => {
                // Tip: find lowest free lane.
                let mut found = None;
                for (i, slot) in lanes.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(commit.id.clone());
                        found = Some(i as u8);
                        break;
                    }
                }
                match found {
                    Some(l) => l,
                    None => {
                        let l = lanes.len() as u8;
                        lanes.push(Some(commit.id.clone()));
                        l
                    }
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
                let mut child_lanes: Vec<u8> = commit_children
                    .iter()
                    .filter_map(|cid| commit_lanes.get(cid).copied())
                    .collect();
                child_lanes.sort_unstable();
                if child_lanes.is_empty() {
                    // Children not yet assigned (shouldn't happen in topological order).
                    let l = lanes.len() as u8;
                    lanes.push(Some(commit.id.clone()));
                    l
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

    // Store lanes in cache.
    for commit in &all_commits {
        if let Err(e) = cache.put_lane(&commit.id, commit.lane, current_epoch) {
            warn!(error = %e, "failed to cache lane");
        }
    }

    // Slice to viewport.
    let start = viewport.offset as usize;
    let has_more = start + (viewport.limit as usize) < all_commits.len();
    let commits: Vec<Commit> = all_commits
        .into_iter()
        .skip(start)
        .take(viewport.limit as usize)
        .collect();

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
    let repo = session.gix_repo.lock().unwrap();

    let platform = repo.status(gix::progress::Discard)?;

    // gix status: empty pattern list matches everything.
    let patterns: Vec<gix::bstr::BString> = Vec::new();
    let iter = platform.into_iter(patterns)?;

    let mut items = Vec::new();
    for entry in iter {
        let entry = entry.map_err(|e| anyhow::anyhow!("{e}"))?;

        let item = match entry {
            gix::status::Item::TreeIndex(change) => map_tree_index_change(change),
            gix::status::Item::IndexWorktree(iw) => map_index_worktree_item(iw),
        };

        if let Some(status_item) = item {
            // Filter by requested paths.
            if let Some(filter) = paths {
                if !filter.iter().any(|p| status_item.path.starts_with(p.as_str())) {
                    continue;
                }
            }
            items.push(status_item);
        }
    }

    debug!(count = items.len(), "status read");
    Ok(items)
}

fn map_tree_index_change(change: gix::diff::index::Change) -> Option<StatusItem> {
    use gix::diff::index::Change as C;

    match change {
        C::Addition { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Added,
            old_path: None,
        }),
        C::Deletion { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Deleted,
            old_path: None,
        }),
        C::Modification { location, .. } => Some(StatusItem {
            path: location.to_string(),
            status: StatusCode::Modified,
            old_path: None,
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
            })
        }
    }
}

pub fn read_refs(session: &Session) -> anyhow::Result<Vec<Ref>> {
    let repo = session.gix_repo.lock().unwrap();
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
