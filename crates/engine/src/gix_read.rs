use anyhow::Context;
use std::collections::HashMap;

use tracing::{debug, info, warn};

use bamboo_protocol::{
    BambooError, BranchStatus, Commit, GraphPage, GraphViewport, Ref, RefKind, RemoteInfo,
    StashEntry, StatusCode, StatusItem,
};

use crate::cache::CachedCommit;
use crate::session::Session;
use crate::write_queue::{redact_credentials, validate_ref_arg};

const PRELOAD_MARGIN: u32 = 500;

pub fn read_graph_page(session: &Session, viewport: &GraphViewport) -> anyhow::Result<GraphPage> {
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

    // Collect all ref targets plus HEAD as tips. Refs that point at
    // annotated tags (refs/tags/*) hold a *tag object* id, and the rev walk
    // does not peel tips itself — feeding it a tag id fails with "Expected
    // object of kind commit but got tag". Peel every ref to the commit it
    // ultimately references instead; refs that don't resolve to a commit
    // (e.g. refs/notes trees) are skipped rather than failing the page.
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
        return Err(BambooError::epoch_mismatch(session.current_epoch(), current_epoch).into());
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
        let parent_ids: Vec<String> = commit_ref.parents().map(|id| id.to_string()).collect();

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
    let lane_entries: Vec<(String, u16)> =
        all_commits.iter().map(|c| (c.id.clone(), c.lane)).collect();
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
    })
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
fn git_output(repo_path: &std::path::Path, args: &[&str]) -> anyhow::Result<(bool, String, String)> {
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
            let (ok, _, _) =
                git_output(repo_path, &["show-ref", "--verify", &format!("refs/heads/{b}")])?;
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
        &["rev-parse", "--abbrev-ref", &format!("{branch}@{{upstream}}")],
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
