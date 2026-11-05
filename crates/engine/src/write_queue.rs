use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use bamboo_protocol::{
    BambooError, ConflictKind, ConflictResolution, ForceMode, GetRebaseStatusResult, PullStrategy,
    RebaseResult, RebaseTodoEntry,
};

use crate::gix_read::{parse_unified_diff, RawFileDiff, RawHunk, DIFF_FLAGS};
use crate::rebase::{
    self, build_validated_todo, classify_stop_reason, is_in_progress, write_sidecars,
};
use crate::session::{EngineEvent, Session};
use crate::undo::{UndoError, UndoOutcome, UndoSummary, UndoErrorKind};

#[derive(Debug, Clone)]
pub enum WriteCommand {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    /// Discard worktree changes: tracked paths are restored from the index
    /// (`git checkout --`), untracked-only paths are deleted (`git clean -f`).
    /// Deliberately IRREVERSIBLE (user-confirmed upstream); does not create
    /// an undo transaction (routed directly in `run_write_queue`, see
    /// there).
    Discard { paths: Vec<String> },
    /// Resolve one conflicted path by taking a side: `ours`/`theirs` take
    /// that side's blob (`git checkout --ours/--theirs` + `git add`),
    /// `keep_ours`/`keep_theirs` keep what that side did — for
    /// delete/modify conflicts that means keeping the file or confirming
    /// the deletion. UNDO DECISION: bypasses the undo journal — like
    /// Discard, this writes worktree file contents the undo `Snapshot`
    /// does not capture; restoring only the index would leave index and
    /// worktree inconsistent (see the ResolveConflict arm in
    /// `run_write_queue`). The conflict classification is re-done fresh
    /// at execution time, so a stale client snapshot can never drive the
    /// wrong resolution.
    ResolveConflict {
        path: String,
        resolution: ConflictResolution,
    },
    Commit { message: String, amend: bool },
    /// Hunk-level staging: apply the hunks of `path` that intersect the
    /// given NEW-side (worktree) line ranges onto the CURRENT index blob
    /// (`git hash-object -w --stdin` + `git update-index --cacheinfo`),
    /// never `git apply --cached` — a client patch can hit "patch does
    /// not apply" once its context drifts, while engine-side blob
    /// construction on a freshly computed diff cannot. The ranges are
    /// re-validated against that fresh diff: no intersecting hunk means
    /// INVALID_ARGUMENT (the diff the user acted on is stale).
    /// UNDO DECISION: bypasses the undo journal — the undo `Snapshot`
    /// captures the index as a tree (`index_tree`), but a range-staged
    /// index is a synthetic blob sequence with no snapshot semantics
    /// (like Discard, restoring it would need content snapshots the
    /// journal does not take); it is trivially redoable by re-staging.
    /// See the StageRanges arm in `run_write_queue`.
    StageRanges { path: String, ranges: Vec<(u32, u32)> },
    /// Hunk-level unstaging, symmetric to StageRanges: revert the staged
    /// hunks of `path` that intersect the given NEW-side (index) line
    /// ranges by rebuilding the index blob as "HEAD + remaining staged
    /// hunks". Same freshness re-validation, same UNDO bypass rationale.
    UnstageRanges { path: String, ranges: Vec<(u32, u32)> },
    CreateBranch { name: String, base: Option<String> },
    DeleteBranch { name: String, force: bool },
    /// UNDO DECISION: journaled (moves HEAD + refs; snapshot-recoverable).
    SwitchBranch { name: String, auto_stash: bool },
    /// UNDO DECISION: bypasses the undo journal — remote-tracking refs
    /// mirror external truth, so an "undo of fetch" is meaningless (see
    /// the Fetch arm in `run_write_queue`).
    Fetch { remote: Option<String> },
    /// UNDO DECISION: journaled (moves HEAD/refs and, with auto-stash,
    /// the stash stack — all captured by `Snapshot`, incl. `stash_ref`).
    Pull {
        remote: Option<String>,
        branch: Option<String>,
        strategy: PullStrategy,
        auto_stash: bool,
    },
    /// UNDO DECISION: journaled (moves the remote branch + upstream
    /// config; refs are snapshotted, config-only changes are a noop-undo).
    Push {
        remote: String,
        branch: String,
        force: ForceMode,
        set_upstream: bool,
    },
    /// UNDO DECISION: journaled (moves refs/stash and the index; the
    /// `Snapshot` carries `stash_ref` + `index_tree` for restore).
    StashPush {
        message: Option<String>,
        include_untracked: bool,
        staged_only: bool,
    },
    /// UNDO DECISION: journaled (moves refs/stash, the index and the
    /// worktree-adjacent index state). On conflict the stash is retained
    /// by git and STASH_CONFLICT is returned — the undo transaction is
    /// committed as failed, which is accurate.
    StashPop {
        stash_ref: Option<String>,
        apply: bool,
        reinstate_index: bool,
    },
    /// UNDO DECISION: journaled (moves refs/stash).
    StashDrop { stash_ref: Option<String> },
    /// UNDO DECISION: journaled — a rebase rewrites branch refs and
    /// HEAD, exactly what `Snapshot` captures. The pre-snapshot is
    /// additionally annotated with `pre_rebase_tip`/`rebase_branch` (see
    /// `run_write`) because mid-rebase resets overwrite `ORIG_HEAD`,
    /// which git documents as unreliable; undo restores from the
    /// journal, never ORIG_HEAD. When the rebase stops (conflict), the
    /// journaled post snapshot is the stopped state; continue/skip
    /// refresh it when the rebase finishes and abort restores it, so the
    /// whole rebase stays undoable as one unit.
    StartRebase {
        onto: String,
        branch: Option<String>,
        todo: Vec<RebaseTodoEntry>,
    },
    /// UNDO DECISION: bypasses a NEW journal entry — continue/skip
    /// advance the ALREADY-JOURNALED StartRebase transaction instead of
    /// starting a new one. The post snapshot of that tx is refreshed
    /// when the rebase finishes (`refresh_rebase_tx`) so the undo safety
    /// check tracks the real outcome and the pre-state restore (abort +
    /// reset) stays reachable. A stop is normal semantics; the result
    /// carries the stop reason/progress, not an error.
    RebaseContinue,
    /// UNDO DECISION: as RebaseContinue (drops the stopped commit and
    /// advances the same journaled rebase transaction).
    RebaseSkip,
    /// UNDO DECISION: bypasses a new journal entry — abort IS a
    /// recovery operation (it restores the pre-rebase state recorded in
    /// the StartRebase tx's pre-snapshot); the tx's post snapshot is
    /// refreshed to the restored state so the undo stack stays
    /// consistent.
    RebaseAbort,
    /// Cascade rollback. Routed through the same single-consumer queue as
    /// every other mutation so undo can never interleave with a write.
    Undo { transaction_id: u64 },
}

impl WriteCommand {
    /// Compact, redacted operation summary used for the undo journal and
    /// the audit log. Never embeds commit messages or file paths — only
    /// shapes and counts — so journal/audit files never leak user content.
    fn op_summary(&self) -> String {
        match self {
            WriteCommand::Stage(paths) => format!("Stage({} paths)", paths.len()),
            WriteCommand::Unstage(paths) => format!("Unstage({} paths)", paths.len()),
            WriteCommand::Discard { .. } => "Discard changes".to_string(),
            WriteCommand::ResolveConflict { resolution, .. } => {
                format!("ResolveConflict({:?})", resolution)
            }
            WriteCommand::Commit { amend, .. } => {
                if *amend {
                    "Commit(amend)".to_string()
                } else {
                    "Commit".to_string()
                }
            }
            WriteCommand::StageRanges { ranges, .. } => {
                format!("StageRanges({} range(s))", ranges.len())
            }
            WriteCommand::UnstageRanges { ranges, .. } => {
                format!("UnstageRanges({} range(s))", ranges.len())
            }
            WriteCommand::CreateBranch { .. } => "CreateBranch".to_string(),
            WriteCommand::DeleteBranch { force, .. } => {
                if *force {
                    "DeleteBranch(force)".to_string()
                } else {
                    "DeleteBranch".to_string()
                }
            }
            WriteCommand::SwitchBranch { auto_stash, .. } => {
                if *auto_stash {
                    "SwitchBranch(auto-stash)".to_string()
                } else {
                    "SwitchBranch".to_string()
                }
            }
            WriteCommand::Fetch { .. } => "Fetch".to_string(),
            WriteCommand::Pull { strategy, auto_stash, .. } => match (*auto_stash, strategy) {
                (false, PullStrategy::Default) => "Pull".to_string(),
                (true, PullStrategy::Default) => "Pull(auto-stash)".to_string(),
                (false, s) => format!("Pull({s:?})"),
                (true, s) => format!("Pull(auto-stash, {s:?})"),
            },
            WriteCommand::Push { force, .. } => match force {
                ForceMode::None => "Push".to_string(),
                ForceMode::Lease => "Push(lease)".to_string(),
                ForceMode::Force => "Push(force)".to_string(),
            },
            WriteCommand::StashPush { include_untracked, staged_only, .. } => {
                match (*include_untracked, *staged_only) {
                    (false, false) => "StashPush".to_string(),
                    (true, false) => "StashPush(untracked)".to_string(),
                    (false, true) => "StashPush(staged)".to_string(),
                    (true, true) => "StashPush(untracked, staged)".to_string(),
                }
            }
            WriteCommand::StashPop { apply, .. } => {
                if *apply {
                    "StashApply".to_string()
                } else {
                    "StashPop".to_string()
                }
            }
            WriteCommand::StashDrop { .. } => "StashDrop".to_string(),
            WriteCommand::StartRebase { todo, .. } => {
                format!("StartRebase({} entries)", todo.len())
            }
            WriteCommand::RebaseContinue => "RebaseContinue".to_string(),
            WriteCommand::RebaseSkip => "RebaseSkip".to_string(),
            WriteCommand::RebaseAbort => "RebaseAbort".to_string(),
            WriteCommand::Undo { .. } => "Undo".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WriteResult {
    Ok(String),
    /// Structured interactive-rebase outcome. A STOP is a normal
    /// outcome, not an error (the conflict is in the worktree, not the
    /// command); real failures use `Err` like every other command.
    Rebase(RebaseResult),
    /// Failure carrying a structured protocol error (code + message) so
    /// the server can surface STASH_CONFLICT / PUSH_REJECTED /
    /// AUTH_REQUIRED to the client instead of a flat git-error string.
    Err(BambooError),
    /// Structured undo outcome (carries a clone-able error projection so
    /// the server can map `UNDO_BLOCKED` / `NotFound` to protocol codes).
    Undo(Result<UndoSummary, UndoErrorKind>),
}

/// Validate a positional path argument before it reaches git. Arguments are
/// passed as argv (no shell), but git itself is an option parser: a path
/// like `-A` or `--all` would be interpreted as a flag.
fn validate_path_arg(p: &str) -> anyhow::Result<()> {
    if p.is_empty() {
        anyhow::bail!("path argument must not be empty");
    }
    if p.starts_with('-') {
        anyhow::bail!("path argument may not begin with '-': {p:?}");
    }
    if p.contains('\0') || p.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("path argument contains control characters");
    }
    Ok(())
}

/// Validate a branch/ref/remote name (mirrors the essentials of
/// `git check-ref-format`). Rejects option-like and malformed names before
/// they reach git as positional arguments.
pub(crate) fn validate_ref_arg(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("ref name must not be empty");
    }
    if name.starts_with('-') || name.starts_with('/') || name.ends_with('/') {
        anyhow::bail!("invalid ref name: {name:?}");
    }
    if name.len() > 512
        || name.contains("..")
        || name.contains("//")
        || name.contains('@')
        || name.ends_with(".lock")
        || name.ends_with('.')
    {
        anyhow::bail!("invalid ref name: {name:?}");
    }
    for ch in name.chars() {
        if ch.is_ascii_control() || " ~^:?*[\\".contains(ch) {
            anyhow::bail!("invalid character {ch:?} in ref name {name:?}");
        }
    }
    for comp in name.split('/') {
        if comp.is_empty() || comp.starts_with('.') || comp.ends_with('.') {
            anyhow::bail!("invalid ref component in {name:?}");
        }
    }
    Ok(())
}

/// Push refspecs may legitimately contain `:` (e.g. `src:dst`); validate the
/// injection-relevant properties only.
pub(crate) fn validate_pushspec(spec: &str) -> anyhow::Result<()> {
    if spec.is_empty() || spec.starts_with('-') || spec.starts_with('+') {
        anyhow::bail!("invalid push refspec: {spec:?}");
    }
    // A refspec with an empty source (`:dst`) DELETES the remote branch
    // and an empty destination (`src:`) is a no-op — both bypass the
    // ForceMode confirmation tiers entirely (`git push origin :main`
    // destroys the remote branch with `force: "none"`), so refuse them
    // outright. Deleting a remote branch, if ever supported, must be an
    // explicit RPC with its own confirmation.
    if spec.starts_with(':') || spec.ends_with(':') {
        anyhow::bail!("invalid push refspec (remote branch deletion is not supported): {spec:?}");
    }
    if spec.chars().any(|c| c.is_ascii_control() || c.is_whitespace()) {
        anyhow::bail!("invalid push refspec: {spec:?}");
    }
    Ok(())
}

/// Stash refs are limited to the strict `stash@{N}` form (digits only) so a
/// caller can never smuggle options, rev ranges or other syntax into what
/// is always a positional argument to `git stash pop/apply/drop`.
fn validate_stash_ref(s: &str) -> anyhow::Result<()> {
    let digits = s
        .strip_prefix("stash@{")
        .and_then(|rest| rest.strip_suffix('}'));
    // At most 9 digits: no real stash stack has a billion entries, and the
    // bound keeps arbitrarily long digit strings out of the queue/argv.
    match digits {
        Some(d) if !d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        _ => anyhow::bail!("stash ref must be of the form stash@{{N}}: {s:?}"),
    }
}

/// Stash messages are the value of `-m`, never parsed as options, but are
/// still bounded and control-character-free (a single line, no NUL/newline
/// injection into journal-adjacent output strings).
const MAX_STASH_MESSAGE_LEN: usize = 500;

fn validate_stash_message(message: &str) -> anyhow::Result<()> {
    if message.is_empty() {
        anyhow::bail!("stash message must not be empty");
    }
    if message.len() > MAX_STASH_MESSAGE_LEN {
        anyhow::bail!("stash message exceeds {MAX_STASH_MESSAGE_LEN} bytes");
    }
    if message.contains('\0') || message.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("stash message contains control characters");
    }
    Ok(())
}

/// The queue task holds only a Weak reference to the session so a replaced
/// session (re-initialize) is dropped rather than leaked; the queue exits
/// once the upgrade fails. Writes remain strictly serialized through this
/// single consumer.
pub async fn run_write_queue(
    mut rx: mpsc::Receiver<(WriteCommand, tokio::sync::oneshot::Sender<WriteResult>)>,
    session: Weak<Session>,
) {
    while let Some((cmd, reply)) = rx.recv().await {
        let Some(session) = session.upgrade() else {
            // Session replaced/closed: drop the pending reply (its oneshot
            // receiver will observe the error).
            info!("session gone; write queue shutting down");
            break;
        };
        let epoch = session.current_epoch();
        let desc = cmd.op_summary();
        info!(cmd = %desc, epoch, "executing write command");

        let (result, tx_id): (WriteResult, u64) = match &cmd {
            WriteCommand::Undo { transaction_id } => {
                run_undo(Arc::clone(&session), *transaction_id).await
            }
            // Discard bypasses the undo journal entirely: it only touches
            // worktree file contents, which the undo snapshot does not
            // capture (it compares HEAD/refs/index, see undo.rs `Snapshot`),
            // so it can neither be undone nor block a pre-existing
            // transaction's undo. It still runs serialized on this queue,
            // bumps the epoch on success, and is audit-logged — only the
            // tx begin/commit is skipped. This is the architectural landing
            // point of "option A" (irreversible discard, confirmed upstream).
            WriteCommand::Discard { paths } => {
                let result = discard_changes(&session, paths).await;
                match result {
                    Ok(output) => (WriteResult::Ok(output), 0),
                    Err(e) => (WriteResult::Err(BambooError::git_error(e.to_string())), 0),
                }
            }
            // ResolveConflict bypasses the undo journal (same reasoning as
            // Discard, see the WriteCommand::ResolveConflict declaration):
            // the resolution writes worktree file contents, which the undo
            // snapshot does not capture — a snapshot-only index restore would
            // leave index and worktree inconsistent. It still runs
            // serialized on this queue, bumps the epoch on success, and is
            // audit-logged — only the undo tx begin/commit is skipped.
            WriteCommand::ResolveConflict { path, resolution } => {
                let result = resolve_conflict(&session, path, *resolution).await;
                match result {
                    Ok(output) => (WriteResult::Ok(output), 0),
                    Err(e) => {
                        let mapped = match e.downcast::<BambooError>() {
                            Ok(structured) => structured,
                            Err(e) => BambooError::git_error(e.to_string()),
                        };
                        (WriteResult::Err(mapped), 0)
                    }
                }
            }
            // Fetch bypasses the undo journal: remote-tracking refs
            // (refs/remotes/*) are a mirror of EXTERNAL truth. An "undo"
            // would rewind that mirror to a stale view of the remote —
            // meaningless to the user — and the post-snapshot ref
            // comparison would then make every later fetch trip the
            // external-change detection of older transactions
            // (UNDO_BLOCKED), blocking undos that are still perfectly
            // valid. Like Discard it still runs serialized on this queue,
            // bumps the epoch on success, and is audit-logged — only the
            // undo tx begin/commit is skipped.
            WriteCommand::Fetch { .. } => {
                let result = execute_command(&session, &cmd).await;
                match result {
                    Ok(output) => (WriteResult::Ok(output), 0),
                    Err(e) => (WriteResult::Err(BambooError::git_error(e.to_string())), 0),
                }
            }
            // StageRanges/UnstageRanges bypass the undo journal: the undo
            // snapshot captures the index as a tree id, but range staging
            // produces synthetic index blobs whose intermediate states no
            // snapshot can represent — an "undo" could only restore the
            // pre-stage index tree, which the whole-file Unstage command
            // already covers, and staging is trivially redoable. The ranges
            // are re-validated against a fresh diff at execution time, so a
            // stale client snapshot can never drive a wrong blob. Like
            // Discard it still runs serialized on this queue, bumps the
            // epoch on success, and is audit-logged — only the undo tx
            // begin/commit is skipped. Structured errors (INVALID_ARGUMENT
            // for stale ranges) reach the RPC surface intact.
            WriteCommand::StageRanges { path, ranges } => {
                let result = stage_ranges(&session, path, ranges).await;
                match result {
                    Ok(output) => (WriteResult::Ok(output), 0),
                    Err(e) => {
                        let mapped = match e.downcast::<BambooError>() {
                            Ok(structured) => structured,
                            Err(e) => BambooError::git_error(e.to_string()),
                        };
                        (WriteResult::Err(mapped), 0)
                    }
                }
            }
            WriteCommand::UnstageRanges { path, ranges } => {
                let result = unstage_ranges(&session, path, ranges).await;
                match result {
                    Ok(output) => (WriteResult::Ok(output), 0),
                    Err(e) => {
                        let mapped = match e.downcast::<BambooError>() {
                            Ok(structured) => structured,
                            Err(e) => BambooError::git_error(e.to_string()),
                        };
                        (WriteResult::Err(mapped), 0)
                    }
                }
            }
            // RebaseContinue/RebaseSkip/RebaseAbort bypass the undo
            // journal (they advance/abort the ALREADY-JOURNALED
            // StartRebase transaction — see the WriteCommand declarations
            // and `rebase_step`). They still run serialized on this queue,
            // bump the epoch on success, and are audit-logged; the bound
            // transaction's post snapshot is refreshed when the rebase
            // finishes/aborts so later undos are neither blocked nor
            // desynced. In-flight dedup: the queue's single-consumer
            // serialization IS the per-command in-flight guard — a
            // double-clicked continue simply queues behind the first, and
            // the second then observes "no rebase in progress", which
            // classifies as idempotent success (see rebase_step).
            WriteCommand::RebaseContinue
            | WriteCommand::RebaseSkip
            | WriteCommand::RebaseAbort => {
                let result = rebase_step(&session, &cmd).await;
                match result {
                    Ok(r) => (WriteResult::Rebase(r), 0),
                    Err(e) => {
                        let mapped = match e.downcast::<BambooError>() {
                            Ok(structured) => structured,
                            Err(e) => BambooError::git_error(e.to_string()),
                        };
                        (WriteResult::Err(mapped), 0)
                    }
                }
            }
            _ => run_write(&session, &cmd).await,
        };

        let success = matches!(
            result,
            WriteResult::Ok(_) | WriteResult::Rebase(_) | WriteResult::Undo(Ok(_))
        );
        // A rebase command may have moved HEAD/refs even when it FAILED
        // (git's abort/continue can get partway through before erroring),
        // so cache invalidation for those is UNCONDITIONAL — clients must
        // never sit on a stale epoch after a failed rebase step.
        let is_rebase_cmd = matches!(
            cmd,
            WriteCommand::StartRebase { .. }
                | WriteCommand::RebaseContinue
                | WriteCommand::RebaseSkip
                | WriteCommand::RebaseAbort
        );

        // Append audit log entry (best-effort).
        if let Err(e) = session.undo.append_audit_log(&desc, epoch, &result, tx_id) {
            error!(error = %e, "failed to write audit log");
        }

        if success || is_rebase_cmd {
            // Bump the epoch synchronously so clients can immediately
            // refresh with a valid epoch. (The fs watcher bumps it again
            // when it observes the .git writes — double bumps are fine,
            // the epoch is just an invalidation counter.)
            let new_epoch = session.bump_epoch();
            if matches!(cmd, WriteCommand::Undo { .. }) {
                session.emit(EngineEvent::GraphInvalidated { epoch: new_epoch });
            }
            if is_rebase_cmd {
                // A rebase moves HEAD (detached mid-rebase, back on the
                // branch at the end) and rewrites the branch ref, so the
                // graph and head views must refresh. Emitted directly
                // with the just-bumped epoch instead of waiting for the
                // fs watcher batch, so the RPC response is always
                // preceded by its notifications.
                let new_head = std::fs::read_to_string(session.git_dir.join("HEAD"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                session.emit(EngineEvent::HeadChanged { new_head, epoch: new_epoch });
                session.emit(EngineEvent::GraphInvalidated { epoch: new_epoch });
            }
            if success {
                match &result {
                    WriteResult::Ok(output) => {
                        info!(output = %output, "write command succeeded");
                    }
                    WriteResult::Rebase(r) => {
                        info!(stopped = r.stopped, "rebase step succeeded");
                    }
                    _ => {}
                }
            }
        } else {
            warn!(error = ?result, "write command failed");
        }

        let _ = reply.send(result);
    }

    info!("write queue shutdown");
}

/// Execute a normal (non-undo) write: pre-snapshot → git → post-snapshot.
/// Returns the result and the undo transaction id (0 on begin-failure).
async fn run_write(session: &Session, cmd: &WriteCommand) -> (WriteResult, u64) {
    let epoch = session.current_epoch();
    let desc = cmd.op_summary();

    // Allocate the undo transaction (pre-write snapshot).
    let tx_id = match session.undo.begin(&desc, epoch) {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "failed to begin undo transaction");
            return (
                WriteResult::Err(BambooError::git_error(format!(
                    "undo snapshot failed: {e}"
                ))),
                0,
            );
        }
    };

    // StartRebase carries extra undo metadata: the authoritative
    // pre-rebase branch tip (`ORIG_HEAD` is overwritten by mid-rebase
    // resets, git documents it as unreliable) and the branch name, used
    // by `restore_snapshot` to `git rebase --abort` + hard-reset the
    // worktree. The annotation derives both from the just-written pre
    // snapshot — refs are still pre-state because this queue is
    // single-consumer — and binds the tx so continue/skip/abort can
    // refresh its post snapshot when the rebase finishes.
    let mut prev_rebase_tx = None;
    if let WriteCommand::StartRebase { branch, .. } = cmd {
        if let Err(e) = session.undo.annotate_rebase(tx_id, branch.as_deref()) {
            error!(error = %e, "failed to annotate undo snapshot for rebase");
            let _ = session.undo.commit(tx_id, &desc, epoch, false);
            return (
                WriteResult::Err(BambooError::git_error(format!(
                    "failed to annotate undo snapshot: {e}"
                ))),
                tx_id,
            );
        }
        // Preserve the in-flight rebase's tx binding: if THIS start fails
        // (validation, in-progress guard, git error), the binding of the
        // still-running rebase must survive — its post snapshot still
        // needs refreshing when THAT rebase finishes.
        prev_rebase_tx = session.take_rebase_tx();
        session.set_rebase_tx(tx_id);
    }

    // The rebase flow returns a STRUCTURED result (stops are normal
    // outcomes); every other command produces a plain output string.
    let result: anyhow::Result<CmdOutcome> = match cmd {
        WriteCommand::StartRebase { onto, branch, todo } => {
            start_rebase_flow(session, onto, branch.as_deref(), todo.clone())
                .await
                .map(CmdOutcome::Rebase)
        }
        _ => execute_command(session, cmd).await.map(CmdOutcome::Output),
    };

    // Finalize the undo transaction (post-write snapshot + journal entry).
    let success = result.is_ok();
    if let Err(e) = session.undo.commit(tx_id, &desc, epoch, success) {
        error!(error = %e, "failed to finalize undo transaction");
        if success {
            // The repository was mutated but the operation is not undoable;
            // never report plain success for that.
            return (
                WriteResult::Err(BambooError::git_error(format!(
                    "git operation succeeded but is NOT undoable (journal write failed): {e}"
                ))),
                tx_id,
            );
        }
    }

    match result {
        Ok(CmdOutcome::Output(output)) => (WriteResult::Ok(output), tx_id),
        Ok(CmdOutcome::Rebase(r)) => {
            // The rebase finished completely in this one command (no
            // stop): release the tx binding — run_write's commit above
            // already captured the final post snapshot.
            if !r.stopped {
                session.take_rebase_tx();
            }
            (WriteResult::Rebase(r), tx_id)
        }
        // Structured protocol errors (STASH_CONFLICT, PUSH_REJECTED,
        // AUTH_REQUIRED) survive; everything else degrades to a plain
        // git error carrying git's stderr.
        Err(e) => {
            // A failed startRebase never left a rebase in progress — the
            // validation and the in-progress guard happen before any
            // subprocess — so the tx binding must not survive; if there
            // was an EARLIER rebase in flight, restore ITS binding.
            if matches!(cmd, WriteCommand::StartRebase { .. }) {
                session.take_rebase_tx();
                if let Some(prev) = prev_rebase_tx {
                    session.set_rebase_tx(prev);
                }
            }
            let mapped = match e.downcast::<BambooError>() {
                Ok(structured) => structured,
                Err(e) => BambooError::git_error(e.to_string()),
            };
            (WriteResult::Err(mapped), tx_id)
        }
    }
}

/// The execution result of one journaled write: a plain output string or
/// a structured rebase outcome.
enum CmdOutcome {
    Output(String),
    Rebase(RebaseResult),
}

/// Execute an undo. Runs the (synchronous, git-shelling) undo engine on a
/// blocking thread — still serialized behind this single-consumer queue.
async fn run_undo(session: Arc<Session>, transaction_id: u64) -> (WriteResult, u64) {
    let undo_session = std::sync::Arc::clone(&session);
    let result = tokio::task::spawn_blocking(move || undo_session.undo.undo(transaction_id)).await;
    let result: Result<UndoOutcome, UndoError> = match result {
        Ok(r) => r,
        Err(e) => Err(UndoError::Io(anyhow::anyhow!("undo task failed: {e}"))),
    };
    // The undo may have pruned the bound rebase transaction (a cascade
    // rollback past it — which also aborted any in-progress rebase via
    // restore_snapshot); release the binding only when that happened, so
    // a still-journaled rebase keeps its refresh binding.
    if let Some(rebase_tx) = session.current_rebase_tx() {
        if !session.undo.has_transaction(rebase_tx) {
            session.take_rebase_tx();
        }
    }
    match result {
        Ok(UndoOutcome::Restored { restored_refs }) => (
            WriteResult::Undo(Ok(UndoSummary { restored_refs, noop: false })),
            transaction_id,
        ),
        Ok(UndoOutcome::Noop) => (
            WriteResult::Undo(Ok(UndoSummary { restored_refs: 0, noop: true })),
            transaction_id,
        ),
        Err(e) => (WriteResult::Undo(Err(e.kind())), transaction_id),
    }
}

async fn execute_command(session: &Session, cmd: &WriteCommand) -> anyhow::Result<String> {
    let repo_path = &session.repo_path;
    // Multi-step or post-processing flows (branch switch with auto-stash,
    // network pull/push, stash conflict classification) dispatch to
    // dedicated functions — see each for its undo/validation semantics.
    // Everything else builds an argv inline and runs exactly once.
    let argv: Vec<String> = match cmd {
        WriteCommand::Undo { .. } => {
            unreachable!("undo is handled by run_undo, not execute_command")
        }
        WriteCommand::Discard { .. } => {
            unreachable!("discard is routed in run_write_queue")
        }
        WriteCommand::ResolveConflict { .. } => {
            unreachable!("resolveConflict is routed in run_write_queue")
        }
        WriteCommand::StageRanges { .. } | WriteCommand::UnstageRanges { .. } => {
            unreachable!("stageRanges/unstageRanges are routed in run_write_queue")
        }
        WriteCommand::StartRebase { .. }
        | WriteCommand::RebaseContinue
        | WriteCommand::RebaseSkip
        | WriteCommand::RebaseAbort => {
            unreachable!("rebase commands are routed in run_write/run_write_queue")
        }
        WriteCommand::SwitchBranch { name, auto_stash } => {
            return switch_branch(session, name, *auto_stash).await;
        }
        WriteCommand::Pull { remote, branch, strategy, auto_stash } => {
            return run_pull(session, remote, branch, *strategy, *auto_stash).await;
        }
        WriteCommand::Push { remote, branch, force, set_upstream } => {
            return run_push(session, remote, branch, *force, *set_upstream).await;
        }
        WriteCommand::StashPush { message, include_untracked, staged_only } => {
            return stash_push(
                repo_path,
                message.as_deref(),
                *include_untracked,
                *staged_only,
            )
            .await;
        }
        WriteCommand::StashPop { stash_ref, apply, reinstate_index } => {
            return stash_pop(
                repo_path,
                stash_ref.as_deref(),
                *apply,
                *reinstate_index,
            )
            .await;
        }
        WriteCommand::StashDrop { stash_ref } => {
            if let Some(stash_ref) = stash_ref {
                validate_stash_ref(stash_ref)?;
            }
            let mut argv: Vec<String> = vec!["stash".to_string(), "drop".to_string()];
            if let Some(stash_ref) = stash_ref {
                argv.push(stash_ref.clone());
            }
            argv
        }
        WriteCommand::Stage(paths) => {
            for p in paths {
                validate_path_arg(p)?;
            }
            let mut argv: Vec<String> = vec!["add".to_string(), "--".to_string()];
            argv.extend(literal_pathspecs(paths));
            argv
        }
        WriteCommand::Unstage(paths) => {
            for p in paths {
                validate_path_arg(p)?;
            }
            let mut argv: Vec<String> =
                vec!["reset".to_string(), "HEAD".to_string(), "--".to_string()];
            argv.extend(literal_pathspecs(paths));
            argv
        }
        WriteCommand::Commit { message, amend } => {
            // Commit guard: refuse to commit while unmerged index entries
            // exist — git itself would refuse (or, for `--amend` on a
            // conflicted merge state, produce a commit with conflict
            // markers). Engine-side, so the protection does not depend on
            // any frontend interception.
            assert_no_unmerged_paths(&session.repo_path).await?;
            // The message is the value of `-m`, never parsed as an option.
            if message.contains('\0') {
                anyhow::bail!("commit message contains NUL");
            }
            let mut argv: Vec<String> = vec!["commit".to_string()];
            if *amend {
                argv.push("--amend".to_string());
                argv.push("--no-edit".to_string());
            } else {
                argv.push("-m".to_string());
                argv.push(message.clone());
            }
            argv
        }
        WriteCommand::CreateBranch { name, base } => {
            validate_ref_arg(name)?;
            if let Some(base) = base {
                // Base may be a branch name or a sha.
                validate_ref_arg(base)?;
            }
            let mut argv: Vec<String> = vec!["branch".to_string(), name.clone()];
            if let Some(base) = base {
                argv.push(base.clone());
            }
            argv
        }
        WriteCommand::DeleteBranch { name, force } => {
            validate_ref_arg(name)?;
            vec![
                "branch".to_string(),
                if *force { "-D" } else { "-d" }.to_string(),
                name.clone(),
            ]
        }
        WriteCommand::Fetch { remote } => {
            let mut argv: Vec<String> = vec!["fetch".to_string()];
            match remote {
                // None = "refresh every remote" (graph-client mental
                // model): `--all`. A single-remote fetch is expressed by
                // passing the remote name explicitly.
                None => argv.push("--all".to_string()),
                Some(remote) => {
                    validate_ref_arg(remote)?;
                    argv.push(remote.clone());
                }
            }
            argv
        }
    };

    // Fetch is the one inline command that talks to the network — it gets
    // the network treatment (wall-clock timeout, non-interactive auth).
    let (ok, stdout, stderr) = if matches!(cmd, WriteCommand::Fetch { .. }) {
        run_git_network(repo_path, &argv).await?
    } else {
        run_git_raw(repo_path, &argv).await?
    };

    if !ok {
        anyhow::bail!("git command failed: {}", redact_credentials(&stderr).trim());
    }
    // Redacted: git output may echo remote URLs, which can embed
    // credentials — that must never reach the RPC client.
    Ok(redact_credentials(&format!("{stdout}{stderr}")).trim().to_string())
}

/// Common construction for every git subprocess the write queue spawns:
/// argv only (never a shell), captured output, stdin closed, and
/// credential prompts disabled — git must never block the
/// single-consumer write queue waiting for terminal input
/// (`GIT_TERMINAL_PROMPT=0` makes auth fail fast so it can classify as
/// AUTH_REQUIRED; `classify_push_error` matches exactly that outcome).
/// `kill_on_drop` backs both the network timeout and queue shutdown.
fn base_git_command(repo_path: &std::path::Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}

/// Run one git argv (no paths) and return (success, stdout, stderr) —
/// used by flows that must classify the failure text themselves
/// (push rejection, stash conflicts).
async fn run_git_raw(
    repo_path: &std::path::Path,
    argv: &[String],
) -> anyhow::Result<(bool, String, String)> {
    let result = base_git_command(repo_path).args(argv).output().await?;
    Ok((
        result.status.success(),
        String::from_utf8_lossy(&result.stdout).into_owned(),
        String::from_utf8_lossy(&result.stderr).into_owned(),
    ))
}

/// Wall-clock bound for network git commands (push/pull/fetch). A
/// stalled remote must never block the single-consumer write queue —
/// and with it every later write, including undo — indefinitely; on
/// expiry the child is killed (kill_on_drop) and the command fails with
/// a structured git error.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(600);

/// `run_git_raw` with the network timeout applied (push/pull/fetch).
async fn run_git_network(
    repo_path: &std::path::Path,
    argv: &[String],
) -> anyhow::Result<(bool, String, String)> {
    let subcommand = argv.first().cloned().unwrap_or_default();
    match tokio::time::timeout(NETWORK_TIMEOUT, run_git_raw(repo_path, argv)).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "git {subcommand} timed out after {} seconds (network command killed)",
            NETWORK_TIMEOUT.as_secs()
        ),
    }
}

/// Strip credentials embedded in URLs from git output before it can
/// reach an RPC error message or result: git echoes remote URLs
/// (`fatal: Authentication failed for 'https://user:token@host/repo/'`)
/// and URLs frequently embed userinfo. Replaces the `user:token@`
/// component with `***@`. Scans the whole string (multiple URLs, URLs
/// inside sentences); text without `://` passes through untouched.
pub(crate) fn redact_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut consumed = 0usize;
    while let Some(rel) = text[consumed..].find("://") {
        let auth_start = consumed + rel + 3;
        out.push_str(&text[consumed..auth_start]);
        let tail = &text[auth_start..];
        // The authority ends at the first path separator, quote, closer
        // or whitespace — whichever comes first.
        let auth_end = tail
            .find(|c: char| {
                c == '/' || c.is_whitespace() || matches!(c, '\'' | '"' | ')' | '>' | '<')
            })
            .unwrap_or(tail.len());
        let authority = &tail[..auth_end];
        if let Some(at) = authority.rfind('@') {
            out.push_str("***@");
            out.push_str(&authority[at + 1..]);
        } else {
            out.push_str(authority);
        }
        consumed = auth_start + auth_end;
    }
    out.push_str(&text[consumed..]);
    out
}

/// Run `main_op` with auto-stash semantics (shared by `pull` and
/// `switchBranch`): when the worktree is dirty, stash (including
/// untracked) → run the operation → pop the stash back. On operation
/// failure the stash is popped back best-effort (the operation may
/// itself have left the worktree conflicted); when even that fails the
/// error says the changes are stranded in the stash. On success a
/// failed pop reports that the operation DID succeed and the changes
/// are retained in the stash — preserving any structured error code
/// (e.g. STASH_CONFLICT) so the client can route to its stash-conflict
/// UI.
async fn run_with_auto_stash<F, Fut>(
    repo_path: &std::path::Path,
    op_desc: &str,
    auto_stash: bool,
    main_op: F,
) -> anyhow::Result<String>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = anyhow::Result<String>> + Send,
{
    let mut output = String::new();
    let mut stashed = false;

    if auto_stash {
        let dirty = run_git_checked(repo_path, &["status", "--porcelain"], &[]).await?;
        if !dirty.trim().is_empty() {
            // Shared stash primitive: `stash push --include-untracked`.
            stash_push(repo_path, None, true, false)
                .await
                .map_err(|e| anyhow::anyhow!("auto-stash failed; {op_desc} aborted: {e}"))?;
            stashed = true;
            output.push_str("stashed local changes; ");
        }
    }

    match main_op().await {
        Ok(out) => {
            if !out.is_empty() {
                output.push_str(&out);
            }
        }
        Err(e) => {
            // The operation failed; put the stash back so the user's
            // changes are not stranded (best-effort — the operation may
            // itself have left the worktree conflicted, in which case
            // popping can fail too).
            if stashed {
                if let Err(pop_err) = stash_pop(repo_path, None, false, false).await {
                    warn!(error = %pop_err, "failed to restore stash after failed {op_desc}");
                    return Err(anyhow::anyhow!(
                        "{e}\nAdditionally, restoring the stashed changes failed: {pop_err}\n\
                         The changes are retained in the stash; run `git stash pop` manually."
                    ));
                }
            }
            return Err(e);
        }
    }

    if stashed {
        match stash_pop(repo_path, None, false, false).await {
            Ok(out) => {
                if !out.is_empty() {
                    output.push('\n');
                    output.push_str(&out);
                }
            }
            Err(e) => {
                // Preserve a structured code (STASH_CONFLICT) for the
                // client's conflict UI; everything else degrades to a
                // plain error — both carry the full explanation.
                let (code, inner) = match e.downcast_ref::<BambooError>() {
                    Some(b) => (Some(b.code), b.message.clone()),
                    None => (None, e.to_string()),
                };
                let msg = format!(
                    "{op_desc} succeeded, but restoring the stashed changes failed: {inner}\n\
                     The changes are retained in the stash; run `git stash pop` manually."
                );
                return match code {
                    Some(code) => Err(BambooError { code, message: msg }.into()),
                    None => Err(anyhow::anyhow!(msg)),
                };
            }
        }
    }

    Ok(output.trim().to_string())
}

/// Push with the three force tiers, optional `-u`, and structured error
/// classification. The engine never confirms force itself — a force push
/// is only reachable after the client's confirmation modal.
async fn run_push(
    session: &Session,
    remote: &str,
    branch: &str,
    force: ForceMode,
    set_upstream: bool,
) -> anyhow::Result<String> {
    validate_ref_arg(remote)?;
    validate_pushspec(branch)?;

    let mut argv: Vec<String> = vec!["push".to_string()];
    match force {
        // Bare lease (no expected value): weaker than a precise-value
        // lease, but precise values would need an extra remote-ref read;
        // the confirm modal + configurable force mode cover the residual
        // risk (documented trade-off, see the phase-1 plan).
        ForceMode::Lease => argv.push("--force-with-lease".to_string()),
        ForceMode::Force => argv.push("--force".to_string()),
        ForceMode::None => {}
    }
    if set_upstream {
        argv.push("--set-upstream".to_string());
    }
    argv.push(remote.to_string());
    argv.push(branch.to_string());

    let (ok, stdout, stderr) = run_git_network(&session.repo_path, &argv).await?;
    if !ok {
        return Err(classify_push_error(remote, branch, &stdout, &stderr).into());
    }
    // Redacted: push output echoes the remote URL, which can embed
    // credentials.
    Ok(redact_credentials(format!("{stdout}{stderr}").trim()))
}

/// Map a failed push's output onto the structured protocol errors the
/// client can act on (PUSH_REJECTED → offer pull; AUTH_REQUIRED → guide
/// terminal auth). Anything unrecognized stays a plain git error. The
/// engine deliberately does NOT orchestrate recovery (pull→push chains
/// are client-side interaction).
fn classify_push_error(remote: &str, branch: &str, stdout: &str, stderr: &str) -> BambooError {
    // Redacted before embedding: git echoes remote URLs in auth/transport
    // failures, and URLs can embed credentials.
    let combined = redact_credentials(&format!("{stdout}{stderr}"));
    // Case-insensitive matching: git's casing varies across transports.
    let lower = combined.to_lowercase();
    // Rejections: "! [rejected] main -> main (fetch first)" (non-ff),
    // "(non-fast-forward)", "(stale info)" (force-with-lease). Carry the
    // remote branch so the client can name it in the recovery UI.
    if lower.contains("[rejected")
        || lower.contains("rejected (fetch first)")
        || lower.contains("(non-fast-forward)")
    {
        return BambooError::push_rejected(format!(
            "push to {remote}/{branch} was rejected (remote has newer commits, \
             or the force-with-lease protection tripped): {}",
            combined.trim()
        ));
    }
    if lower.contains("could not read username")
        || lower.contains("authentication failed")
        || lower.contains("terminal prompts disabled")
        // SSH and 403-style failures must classify as auth too, not as
        // generic git errors the client cannot guide the user through.
        || lower.contains("permission denied")
        || lower.contains("host key verification failed")
        || lower.contains("access denied")
        || lower.contains("invalid username or password")
    {
        return BambooError::auth_required(format!(
            "authentication required for {remote}: {}",
            combined.trim()
        ));
    }
    BambooError::git_error(combined.trim().to_string())
}

/// Pull with strategy flags and auto-stash. `Default` appends no flag so
/// the user's `pull.rebase` / `pull.ff` git config decides (engine never
/// overrides un-asked). Auto-stash: push dirty worktree → pull → pop;
/// on pop failure the changes are retained in the stash and the error
/// says so explicitly.
async fn run_pull(
    session: &Session,
    remote: &Option<String>,
    branch: &Option<String>,
    strategy: PullStrategy,
    auto_stash: bool,
) -> anyhow::Result<String> {
    if let Some(remote) = remote {
        validate_ref_arg(remote)?;
    }
    if let Some(branch) = branch {
        // A branch without a remote would be silently ignored by
        // `git pull` (which then pulls the current branch's upstream) —
        // reject it instead of succeeding against the wrong thing.
        if remote.is_none() {
            return Err(BambooError::invalid_argument(
                "pull: branch requires a remote; to pull the current branch \
                 from its upstream, omit both",
            )
            .into());
        }
        validate_ref_arg(branch)?;
    }

    let mut argv: Vec<String> = vec!["pull".to_string()];
    match strategy {
        PullStrategy::Default => {}
        PullStrategy::Merge => argv.push("--no-rebase".to_string()),
        PullStrategy::Rebase => argv.push("--rebase".to_string()),
        PullStrategy::FfOnly => argv.push("--ff-only".to_string()),
    }
    if let Some(remote) = remote {
        argv.push(remote.clone());
        if let Some(branch) = branch {
            argv.push(branch.clone());
        }
    }

    // Auto-stash (dirty check → stash → pull → pop-back) is shared with
    // switchBranch — see run_with_auto_stash.
    let repo_path = session.repo_path.clone();
    let repo_path_for_op = repo_path.clone();
    run_with_auto_stash(
        &repo_path,
        "pull",
        auto_stash,
        move || async move {
            let (ok, stdout, stderr) = run_git_network(&repo_path_for_op, &argv).await?;
            if !ok {
                anyhow::bail!(
                    "git pull failed: {}",
                    redact_credentials(&format!("{stdout}{stderr}")).trim()
                );
            }
            // Redacted: pull output echoes the remote URL, which can
            // embed credentials.
            Ok(redact_credentials(format!("{stdout}{stderr}").trim()))
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Hunk-level staging (stageRanges/unstageRanges, phase 5)
// ---------------------------------------------------------------------------

/// Upper bound for line-range endpoints (a diff of a real text file never
/// exceeds this; the bound keeps absurd numbers out of the hunk math).
const MAX_LINE_RANGE: u32 = 10_000_000;

/// Upper bound for the number of ranges per request (one CodeLens click
/// sends exactly one; anything larger is a malformed client).
const MAX_RANGE_COUNT: usize = 64;

/// Engine-side range validation — the host clamps as a UX nicety, but the
/// engine never trusts it: 1-based, ordered, bounded.
fn validate_line_ranges(ranges: &[(u32, u32)]) -> Result<(), BambooError> {
    if ranges.is_empty() || ranges.len() > MAX_RANGE_COUNT {
        return Err(BambooError::invalid_argument(format!(
            "stageRanges/unstageRanges: 1..{MAX_RANGE_COUNT} ranges required"
        )));
    }
    for (start, end) in ranges {
        if *start < 1 || start > end || *end > MAX_LINE_RANGE {
            return Err(BambooError::invalid_argument(format!(
                "invalid line range {start}..{end} (1-based, start <= end, \u{2264} {MAX_LINE_RANGE})"
            )));
        }
    }
    Ok(())
}

/// Hunk selection: a hunk is selected when any requested range overlaps
/// the hunk's new-side interval (see `RawHunk::new_side_interval` for the
/// shared host/engine formula). Whole-hunk granularity — partial overlap
/// stages the whole hunk; per-line granularity is a later phase that will
/// reuse this exact intersection.
fn select_hunks<'a>(hunks: &'a [RawHunk], ranges: &[(u32, u32)]) -> Vec<&'a RawHunk> {
    hunks
        .iter()
        .filter(|h| {
            let (hunk_start, hunk_end) = h.new_side_interval();
            ranges
                .iter()
                .any(|(rs, re)| (*rs as u64) <= hunk_end && (*re as u64) >= hunk_start)
        })
        .collect()
}

/// Reject unmerged paths before any hunk math: `git diff` renders them as
/// combined diffs (`@@@`, conflict markers) that the unified parser must
/// never be asked to apply.
async fn assert_path_not_conflicted(session: &Session, path: &str) -> Result<(), BambooError> {
    let out = run_git_checked(
        &session.repo_path,
        &["status", "--porcelain", "-z"],
        &[path.to_string()],
    )
    .await
    .map_err(|e| BambooError::git_error(e.to_string()))?;
    if parse_porcelain_z(&out)
        .iter()
        .any(|(xy, _)| porcelain_conflict_kind(xy).is_some())
    {
        return Err(BambooError::invalid_argument(format!(
            "cannot stage ranges for {path:?}: the file has merge conflicts \u{2014} resolve them first"
        )));
    }
    Ok(())
}

/// The diff for one path, freshly computed by the engine at execution
/// time (the freshness basis: ranges the client derived from an older
/// diff are re-checked against THIS output before anything is written).
async fn current_path_diff(
    session: &Session,
    path: &str,
    staged: bool,
) -> anyhow::Result<RawFileDiff> {
    let mut argv: Vec<String> = vec!["diff".to_string()];
    argv.extend(DIFF_FLAGS.iter().map(|f| f.to_string()));
    if staged {
        argv.push("--cached".to_string());
    }
    argv.push("--".to_string());
    argv.push(format!(":(literal){path}"));
    let (ok, stdout, stderr) = run_git_raw_bytes(&session.repo_path, &argv).await?;
    if !ok {
        anyhow::bail!(
            "git diff failed for {path:?}: {}",
            redact_credentials(&stderr).trim()
        );
    }
    Ok(parse_unified_diff(&stdout))
}

/// Byte-exact blob content of `rev:path` (`:0` = index, `HEAD` = HEAD).
/// A path absent from that rev resolves to `None` (an empty base); other
/// failures stay errors. Bytes, not String: lossy UTF-8 would corrupt
/// non-UTF-8 content when the reconstructed blob is re-hashed.
async fn read_blob_bytes(
    repo_path: &std::path::Path,
    rev: &str,
    path: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let spec = format!("{rev}:{path}");
    let argv = ["cat-file".to_string(), "blob".to_string(), spec];
    let (ok, stdout, stderr) = run_git_raw_bytes(repo_path, &argv).await?;
    if !ok {
        // Both messages mean "absent from that revision": a path that is
        // not in HEAD at all (git's wording differs when it exists on
        // disk, e.g. a newly added file).
        if stderr.contains("does not exist")
            || stderr.contains("exists on disk, but not in")
        {
            return Ok(None);
        }
        anyhow::bail!(
            "git cat-file failed: {}",
            redact_credentials(&stderr).trim()
        );
    }
    Ok(Some(stdout))
}

/// Apply selected hunks onto `base` content, byte-exactly. Hunk old-side
/// line positions refer to the ORIGINAL base; `offset` carries the line
/// delta of previously applied hunks so later hunks land correctly.
/// Trailing-newline state is reconstructed from the `\ No newline at end
/// of file` markers: the last line of the output keeps its newline iff
/// the side it came from ended with one.
fn apply_hunks(base: &[u8], hunks: &[&RawHunk]) -> anyhow::Result<Vec<u8>> {
    // Header-vs-body sanity check: if the parser ever disagrees with git's
    // own counts, refuse rather than construct a corrupt index blob.
    for h in hunks {
        let old_side = h.lines.iter().filter(|l| l.origin != b'+').count() as u64;
        let new_side = h.lines.iter().filter(|l| l.origin != b'-').count() as u64;
        if old_side != h.old_count || new_side != h.new_count {
            anyhow::bail!(
                "parsed hunk line counts disagree with the {} header \u{2014} refusing to construct a blob",
                "diff"
            );
        }
    }

    let (base_lines, base_has_nl) = split_content_lines(base);
    let mut out: Vec<Vec<u8>> = Vec::new();
    // Whether the final output line carries a trailing newline; updated
    // as each potential last-line source emits.
    let mut trailing_nl = base_has_nl;
    let mut cursor = 0usize; // next base line still to be copied
    let mut offset: i64 = 0; // cumulative new-side line delta of applied hunks

    for h in hunks {
        let old_idx = if h.old_start == 0 {
            0
        } else {
            (h.old_start - 1) as usize
        };
        let old_len = h.old_count as usize;
        // Defensive clamps (well-formed diffs never need them): never go
        // backwards, never past the base end.
        let skip = (((old_idx as i64) + offset).max(0) as usize)
            .max(cursor)
            .min(base_lines.len());
        while cursor < skip {
            out.push(base_lines[cursor].to_vec());
            trailing_nl = base_has_nl;
            cursor += 1;
        }
        for line in &h.lines {
            if line.origin != b'-' {
                out.push(line.text.clone());
            }
        }
        let ends_at_eof = old_idx + old_len >= base_lines.len();
        if ends_at_eof {
            // The hunk's new side supplies the file's final line: git emits
            // the no-newline marker exactly when that line ends without
            // a newline.
            trailing_nl = !h.new_no_newline;
        }
        offset += h.new_count as i64 - h.old_count as i64;
        cursor = (old_idx + old_len).max(cursor).min(base_lines.len());
    }

    while cursor < base_lines.len() {
        out.push(base_lines[cursor].to_vec());
        trailing_nl = base_has_nl;
        cursor += 1;
    }

    let mut result = Vec::new();
    for (i, line) in out.iter().enumerate() {
        if i > 0 {
            result.push(b'\n');
        }
        result.extend_from_slice(line);
    }
    if trailing_nl && !out.is_empty() {
        result.push(b'\n');
    }
    Ok(result)
}

/// Split blob content into lines (no terminators) plus whether the
/// content ends with a newline. Empty content is zero lines.
fn split_content_lines(content: &[u8]) -> (Vec<&[u8]>, bool) {
    if content.is_empty() {
        return (Vec::new(), true);
    }
    let (body, has_nl) = match content.strip_suffix(b"\n") {
        Some(body) => (body, true),
        None => (content, false),
    };
    (body.split(|b| *b == b'\n').collect(), has_nl)
}

/// Index-entry mode for `update-index --cacheinfo`: the existing index
/// entry's mode first (authoritative), then the HEAD tree entry (staged
/// deletions have no index entry), then the worktree file's exec bit,
/// else the plain 100644 default.
async fn resolve_index_mode(
    repo_path: &std::path::Path,
    path: &str,
) -> anyhow::Result<String> {
    let literal = format!(":(literal){path}");
    let ls_files = run_git_raw_bytes(
        repo_path,
        &[
            "ls-files".to_string(),
            "-s".to_string(),
            "--".to_string(),
            literal.clone(),
        ],
    )
    .await?;
    if ls_files.0 {
        if let Some(mode) = ls_files
            .1
            .split(|b| *b == b'\n')
            .next()
            .and_then(|l| l.split(|b| *b == b' ').next())
            .filter(|m| !m.is_empty() && m != b"000000")
        {
            return Ok(String::from_utf8_lossy(mode).into_owned());
        }
    }
    let ls_tree = run_git_raw_bytes(
        repo_path,
        &[
            "ls-tree".to_string(),
            "HEAD".to_string(),
            "--".to_string(),
            literal,
        ],
    )
    .await?;
    if ls_tree.0 {
        if let Some(mode) = ls_tree
            .1
            .split(|b| *b == b'\n')
            .next()
            .and_then(|l| l.split(|b| *b == b' ').next())
            .filter(|m| !m.is_empty())
        {
            return Ok(String::from_utf8_lossy(mode).into_owned());
        }
    }
    let executable = repo_path
        .join(path)
        .metadata()
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false);
    Ok(if executable { "100755" } else { "100644" }.to_string())
}

/// `git hash-object -w --stdin --path <path>`: write the constructed
/// content as a blob and return its object id. `--path` makes git apply
/// the path's checkin filters, matching how the diff content was
/// produced (VS Code's staging does the same).
async fn hash_object(
    repo_path: &std::path::Path,
    path: &str,
    content: &[u8],
) -> anyhow::Result<String> {
    let mut cmd = base_git_command(repo_path);
    // base_git_command closes stdin; this one command needs it.
    cmd.stdin(Stdio::piped());
    cmd.args(["hash-object", "-w", "--stdin", "--path", path]);
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content).await?;
        // Dropping the handle closes the pipe → EOF → git hashes and exits.
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "git hash-object failed: {}",
            redact_credentials(&String::from_utf8_lossy(&out.stderr)).trim()
        );
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.len() != 40 && sha.len() != 64 {
        anyhow::bail!("unexpected git hash-object output: {sha:?}");
    }
    Ok(sha)
}

/// Point the index entry of `path` at a freshly written blob (the
/// VS Code approach: `hash-object -w` + `update-index --add --cacheinfo`),
/// or remove the entry when `remove` is set (staging a whole-file
/// deletion / unstaging everything of a newly added file).
async fn write_index_blob(
    session: &Session,
    path: &str,
    content: &[u8],
    remove: bool,
) -> anyhow::Result<()> {
    let argv: Vec<String> = if remove {
        vec![
            "update-index".to_string(),
            "--force-remove".to_string(),
            "--".to_string(),
            path.to_string(),
        ]
    } else {
        let mode = resolve_index_mode(&session.repo_path, path).await?;
        let sha = hash_object(&session.repo_path, path, content).await?;
        vec![
            "update-index".to_string(),
            "--add".to_string(),
            "--cacheinfo".to_string(),
            format!("{mode},{sha},{path}"),
        ]
    };
    let (ok, _, stderr) = run_git_raw(&session.repo_path, &argv).await?;
    if !ok {
        anyhow::bail!(
            "git update-index failed: {}",
            redact_credentials(&stderr).trim()
        );
    }
    Ok(())
}

/// `git diff` stdout as raw bytes (run_git_raw's lossy String would
/// corrupt non-UTF-8 content lines during blob reconstruction).
async fn run_git_raw_bytes(
    repo_path: &std::path::Path,
    argv: &[String],
) -> anyhow::Result<(bool, Vec<u8>, String)> {
    let result = base_git_command(repo_path).args(argv).output().await?;
    Ok((
        result.status.success(),
        result.stdout,
        String::from_utf8_lossy(&result.stderr).into_owned(),
    ))
}

/// `stageRanges`: apply the intersecting worktree hunks of `path` onto
/// the CURRENT index blob. Basing the reconstruction on the index (not
/// HEAD) is what makes incremental staging correct: staging hunk B
/// after hunk A must extend A's staged content, not clobber it — when
/// nothing is staged the index equals HEAD, so this also IS "HEAD +
/// selected hunks" in the single-change case.
async fn stage_ranges(
    session: &Session,
    path: &str,
    ranges: &[(u32, u32)],
) -> anyhow::Result<String> {
    validate_path_arg(path)?;
    validate_line_ranges(ranges)?;
    assert_path_not_conflicted(session, path).await?;

    let diff = current_path_diff(session, path, false).await?;
    if diff.binary {
        return Err(BambooError::invalid_argument(format!(
            "{path:?} is a binary file \u{2014} hunk-level staging is not possible"
        ))
        .into());
    }
    let selected = select_hunks(&diff.hunks, ranges);
    if selected.is_empty() {
        return Err(BambooError::invalid_argument(format!(
            "no unstaged hunk of {path:?} intersects lines \u{2014} the diff may be stale; refresh and retry"
        ))
        .into());
    }

    let base = read_blob_bytes(&session.repo_path, ":0", path)
        .await?
        .unwrap_or_default();
    let content = apply_hunks(&base, &selected)?;

    // Staging the whole-file deletion of a worktree-deleted file removes
    // the index entry (an empty blob would stage an empty FILE instead).
    let worktree_exists = session.repo_path.join(path).is_file();
    let remove = !worktree_exists && content.is_empty();
    write_index_blob(session, path, &content, remove).await?;

    Ok(format!("staged {} hunk(s) of {path:?}", selected.len()))
}

/// `unstageRanges`: revert the intersecting staged hunks of `path` by
/// rebuilding the index blob as "HEAD + the staged hunks that were NOT
/// selected" — symmetric to stage_ranges' construction.
async fn unstage_ranges(
    session: &Session,
    path: &str,
    ranges: &[(u32, u32)],
) -> anyhow::Result<String> {
    validate_path_arg(path)?;
    validate_line_ranges(ranges)?;
    assert_path_not_conflicted(session, path).await?;

    let diff = current_path_diff(session, path, true).await?;
    if diff.binary {
        return Err(BambooError::invalid_argument(format!(
            "{path:?} is a binary file \u{2014} hunk-level unstaging is not possible"
        ))
        .into());
    }
    let selected = select_hunks(&diff.hunks, ranges);
    if selected.is_empty() {
        return Err(BambooError::invalid_argument(format!(
            "no staged hunk of {path:?} intersects lines \u{2014} the diff may be stale; refresh and retry"
        ))
        .into());
    }

    let head_blob = read_blob_bytes(&session.repo_path, "HEAD", path).await?;
    let base = head_blob.clone().unwrap_or_default();
    let kept: Vec<&RawHunk> = diff
        .hunks
        .iter()
        .filter(|h| !selected.iter().any(|s| std::ptr::eq(*s, *h)))
        .collect();
    let content = apply_hunks(&base, &kept)?;

    // Unstaging everything of a newly added (HEAD-absent) file removes it
    // from the index; everything else re-points the entry at the
    // reconstructed blob.
    let remove = content.is_empty() && head_blob.is_none();
    write_index_blob(session, path, &content, remove).await?;

    Ok(format!("unstaged {} hunk(s) of {path:?}", selected.len()))
}

// ---------------------------------------------------------------------------
// Stash primitives (shared by switchBranch auto-stash and the stash RPCs)
// ---------------------------------------------------------------------------

/// Stash push primitive: `git stash push [-u] [-S] [-m <msg>]`. Shared by
/// `switch_branch` (auto-stash) and the `stashPush` RPC — one
/// implementation, no copies (second consumer triggered the extraction).
async fn stash_push(
    repo_path: &std::path::Path,
    message: Option<&str>,
    include_untracked: bool,
    staged_only: bool,
) -> anyhow::Result<String> {
    if let Some(message) = message {
        validate_stash_message(message)?;
    }
    let mut argv: Vec<String> = vec!["stash".to_string(), "push".to_string()];
    if include_untracked {
        argv.push("--include-untracked".to_string());
    }
    if staged_only {
        argv.push("--staged".to_string());
    }
    if let Some(message) = message {
        argv.push("-m".to_string());
        argv.push(message.to_string());
    }
    let (ok, stdout, stderr) = run_git_raw(repo_path, &argv).await?;
    if !ok {
        anyhow::bail!(
            "git stash push failed: {}",
            redact_credentials(&format!("{stdout}{stderr}")).trim()
        );
    }
    Ok(redact_credentials(format!("{stdout}{stderr}").trim()))
}

/// Stash pop/apply primitive: `git stash pop|apply [--index] [stash@{N}]`.
/// On failure git does NOT drop the stash entry — it is retained. Merge
/// conflicts ("could not apply", CONFLICT) leave the conflicted files in
/// the worktree and surface as STASH_CONFLICT so the client can hand
/// them to the conflict flow; a "would be overwritten" failure aborts
/// BEFORE anything is applied (nothing reaches the worktree) and
/// surfaces as STASH_CONFLICT too, with wording that says nothing was
/// applied. Other failures stay plain git errors. Shared by
/// `switch_branch` (auto-stash pop-back) and the stash RPCs.
async fn stash_pop(
    repo_path: &std::path::Path,
    stash_ref: Option<&str>,
    apply: bool,
    reinstate_index: bool,
) -> anyhow::Result<String> {
    if let Some(stash_ref) = stash_ref {
        validate_stash_ref(stash_ref)?;
    }
    let mut argv: Vec<String> = vec!["stash".to_string()];
    argv.push(if apply { "apply" } else { "pop" }.to_string());
    if reinstate_index {
        argv.push("--index".to_string());
    }
    if let Some(stash_ref) = stash_ref {
        argv.push(stash_ref.to_string());
    }

    let (ok, stdout, stderr) = run_git_raw(repo_path, &argv).await?;
    if !ok {
        let combined = redact_credentials(&format!("{stdout}{stderr}"));
        let op = if apply { "apply" } else { "pop" };
        let conflicted =
            combined.contains("could not apply") || combined.contains("CONFLICT");
        let would_overwrite = combined.contains("would be overwritten");
        if conflicted || would_overwrite {
            // The wording must match what actually happened: a merge
            // conflict has already written conflicted files to the
            // worktree; the would-be-overwritten pre-check applied
            // NOTHING.
            let reason = if would_overwrite && !conflicted {
                format!(
                    "stash {op} would overwrite local changes; nothing was \
                     applied. The stash entry is retained (your changes are \
                     safe in the stash)"
                )
            } else {
                format!(
                    "stash {op} conflicted; stash retained (git does not drop \
                     the entry — your changes are safe in the stash; resolve \
                     the conflicted files in the worktree)"
                )
            };
            return Err(
                BambooError::stash_conflict(format!("{reason}: {}", combined.trim())).into(),
            );
        }
        return Err(BambooError::git_error(format!(
            "git stash {op} failed: {}",
            combined.trim()
        ))
        .into());
    }
    Ok(redact_credentials(format!("{stdout}{stderr}").trim()))
}

// Render paths as `:(literal)` pathspecs so a path that happens to
// contain glob metacharacters (`*?[]!`) or a magic prefix (`:`) is passed
// with literal semantics — git would otherwise re-interpret such a path
// as a glob/magic pathspec, letting a write act on files other than the
// ones that were classified and confirmed. Always combined with a `--`
// separator so paths can never be read as flags.
fn literal_pathspecs(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| format!(":(literal){p}")).collect()
}

/// Discard worktree changes for the given paths: tracked paths are
/// restored from the index (staged content survives), untracked-only
/// paths are deleted, intent-to-add paths (`git add -N`, index entry is
/// the empty blob) are removed from index and worktree. Classification
/// happens fresh at execution time so a stale UI snapshot can never drive
/// the wrong git subcommand — this is stronger than re-reading status
/// host-side.
async fn discard_changes(session: &Session, paths: &[String]) -> anyhow::Result<String> {
    for p in paths {
        validate_path_arg(p)?;
        // A trailing slash is a directory-shaped pathspec that could expand
        // to every file under the directory — beyond what was confirmed.
        // Untracked directories are addressed by their collapsed name
        // (e.g. "newdir", not "newdir/").
        if p.ends_with('/') {
            anyhow::bail!("discard paths must name files, not directories: {p:?}");
        }
    }

    let (tracked, untracked, intent_to_add) = classify_discard_paths(session, paths).await?;

    if !tracked.is_empty() {
        run_git_checked(&session.repo_path, &["checkout"], &tracked).await?;
    }
    if !untracked.is_empty() {
        run_git_checked(&session.repo_path, &["clean", "-f"], &untracked).await?;
    }
    if !intent_to_add.is_empty() {
        // `git clean` skips index-tracked files, so intent-to-add entries
        // need `git rm -f` (removes the empty-blob index entry and the
        // worktree file); a plain `checkout --` would truncate the file to
        // the empty index blob instead of deleting it.
        run_git_checked(&session.repo_path, &["rm", "-f"], &intent_to_add).await?;
    }

    Ok(format!(
        "discarded {} tracked and {} untracked path(s)",
        tracked.len(),
        untracked.len() + intent_to_add.len()
    ))
}

/// Classify `paths` for discard using `git status --porcelain -z`.
/// Returns (tracked, untracked, intent-to-add) lists; a path with no
/// reported change is an error (a destructive RPC must never silently
/// no-op — the caller confirmed real changes). Unmerged (conflict) states
/// are rejected — restoring one side blindly would destroy the other
/// side's changes, which B1 does not attempt.
///
/// Known narrow race: an external process can mutate the worktree between
/// this classification and the checkout/clean/rm invocations, so the git
/// commands may act on a state newer than the classification. The queue
/// serializes only the engine's own writes; shrinking this window further
/// would require locking the worktree, which the CLI-based write path
/// does not attempt.
async fn classify_discard_paths(
    session: &Session,
    paths: &[String],
) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let out = run_git_checked(
        &session.repo_path,
        &["status", "--porcelain", "-z"],
        paths,
    )
    .await?;

    let entries = parse_porcelain_z(&out);

    // Unmerged index states (both sides diverge); `AA`/`DD` also only
    // arise from unmerged index entries.
    if let Some((_, path)) = entries
        .iter()
        .find(|(xy, _)| porcelain_conflict_kind(xy).is_some())
    {
        anyhow::bail!(
            "cannot discard {path:?}: the file has merge conflicts \
             (resolve them first; discarding conflicted files is not supported)"
        );
    }

    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    let mut intent_to_add = Vec::new();
    for p in paths {
        // Untracked directories are reported with a trailing slash
        // ("?? newdir/") while callers address them by their collapsed
        // name ("newdir"), so compare after stripping it.
        let entry = entries.iter().find(|(_, path)| {
            path.as_str() == p.as_str() || path.strip_suffix('/') == Some(p.as_str())
        });
        match entry {
            // "??" = untracked (worktree only, not in the index).
            Some((xy, _)) if xy == "??" => untracked.push(p.clone()),
            // " A" / " AM" = intent-to-add (empty-blob index entry): the
            // worktree copy is the only real content, so discarding means
            // deletion, not an index restore.
            Some((xy, _)) if xy == " A" || xy == " AM" => intent_to_add.push(p.clone()),
            // Any other status line (modified, deleted, added, staged,
            // renamed, ...) means the path is tracked: `checkout --`
            // restores the worktree copy from the index.
            Some((_, _)) => tracked.push(p.clone()),
            // No reported change → refuse instead of silently no-oping a
            // destructive RPC the caller believes did something.
            None => anyhow::bail!("no changes to discard for {p:?}"),
        }
    }

    Ok((tracked, untracked, intent_to_add))
}

/// Parse `git status --porcelain -z` output into (XY, path) records.
/// -z records are NUL-terminated and unquoted; a rename/copy entry
/// carries the original path as an extra trailing field that must be
/// consumed, never parsed as an entry itself.
fn parse_porcelain_z(out: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut fields = out.split('\0');
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        if field.len() < 3 {
            continue;
        }
        let xy = &field[..2];
        let path = field[3..].to_string();
        entries.push((xy.to_string(), path));
        if xy.starts_with('R') || xy.starts_with('C') {
            // Consume the rename/copy source path field.
            let _ = fields.next();
        }
    }
    entries
}

/// Map a `git status --porcelain` XY code onto the conflict kind it
/// represents, or `None` for non-conflict codes. Unmerged codes:
/// DD both deleted, AU added by us, UD deleted by them, UA added by
/// them, DU deleted by us, AA both added, UU both modified.
fn porcelain_conflict_kind(xy: &str) -> Option<ConflictKind> {
    match xy {
        "DD" => Some(ConflictKind::BothDeleted),
        "AU" => Some(ConflictKind::AddedByUs),
        "UD" => Some(ConflictKind::DeletedByThem),
        "UA" => Some(ConflictKind::AddedByThem),
        "DU" => Some(ConflictKind::DeletedByUs),
        "AA" => Some(ConflictKind::BothAdded),
        "UU" => Some(ConflictKind::BothModified),
        _ => None,
    }
}

/// What git commands a conflict resolution boils down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictAction {
    /// `git checkout --ours -- <path>` + `git add --` (take our blob —
    /// also the "keep our file" side of a delete/modify conflict, since
    /// our blob is staged at stage 2).
    TakeOurs,
    /// `git checkout --theirs -- <path>` + `git add --` (take their blob —
    /// likewise the surviving side of a delete/modify conflict).
    TakeTheirs,
    /// Confirm the deletion: `git rm -f -- <path>` (drop all stages and
    /// any worktree copy of the other side's content).
    Remove,
}

/// The git commands a resolution maps to for a given conflict kind.
/// Content conflicts (both-modified, both-added, added-by-*) have both
/// blobs staged, so `ours`/`theirs` are `git checkout --ours/--theirs`;
/// delete/modify conflicts only have one surviving side, so the actions
/// are "keep the file" or "confirm the deletion". `keep_ours`/`keep_theirs`
/// name the side whose *change* survives, not a blob — for content
/// conflicts they are the same as `ours`/`theirs`.
fn action_for(kind: ConflictKind, resolution: ConflictResolution) -> Result<ConflictAction, BambooError> {
    use ConflictKind as K;
    let kind_name = |k: ConflictKind| match k {
        K::BothDeleted => "both-deleted",
        K::AddedByUs => "added-by-us",
        K::DeletedByThem => "deleted-by-them",
        K::AddedByThem => "added-by-them",
        K::DeletedByUs => "deleted-by-us",
        K::BothAdded => "both-added",
        K::BothModified => "both-modified",
    };
    let mismatch = || {
        let (kind_str, hint) = match kind {
            K::BothDeleted | K::DeletedByThem | K::DeletedByUs => (
                kind_name(kind),
                "a side deleted the file: use keepOurs/keepTheirs",
            ),
            _ => (
                kind_name(kind),
                "both sides have content: use ours/theirs",
            ),
        };
        BambooError::invalid_argument(format!(
            "resolution {resolution:?} is not valid for a {kind_str} conflict ({hint})"
        ))
    };
    Ok(match (kind, resolution) {
        // Content conflicts: both sides' blobs are staged.
        (K::BothModified | K::BothAdded | K::AddedByUs | K::AddedByThem, ConflictResolution::Ours) => {
            ConflictAction::TakeOurs
        }
        (K::BothModified | K::BothAdded | K::AddedByUs | K::AddedByThem, ConflictResolution::Theirs) => {
            ConflictAction::TakeTheirs
        }
        // Delete/modify: "keep" names the side whose change survives.
        // We modified, they deleted: keepOurs keeps the file (our blob is
        // staged and in the worktree); keepTheirs confirms the deletion.
        (K::DeletedByThem, ConflictResolution::KeepOurs) => ConflictAction::TakeOurs,
        (K::DeletedByThem, ConflictResolution::KeepTheirs) => ConflictAction::Remove,
        // We deleted, they modified: keepOurs confirms our deletion
        // (the worktree copy of their content goes away); keepTheirs keeps
        // their file, which is exactly the staged stage-3 blob.
        (K::DeletedByUs, ConflictResolution::KeepOurs) => ConflictAction::Remove,
        (K::DeletedByUs, ConflictResolution::KeepTheirs) => ConflictAction::TakeTheirs,
        // Both deleted: any "keep" confirms the deletion.
        (K::BothDeleted, ConflictResolution::KeepOurs | ConflictResolution::KeepTheirs) => {
            ConflictAction::Remove
        }
        // Everything else is a kind/resolution mismatch: never guess.
        _ => return Err(mismatch()),
    })
}

/// Classify `path`'s conflict kind fresh, at execution time, from
/// `git status --porcelain -z`. `None` (mapped to an INVALID_ARGUMENT
/// error) when the path is not currently conflicted — a stale client
/// snapshot must never drive a resolution.
async fn classify_conflict_path(session: &Session, path: &str) -> Result<ConflictKind, BambooError> {
    let out = run_git_checked(
        &session.repo_path,
        &["status", "--porcelain", "-z"],
        &[path.to_string()],
    )
    .await
    .map_err(|e| BambooError::git_error(e.to_string()))?;
    let entries = parse_porcelain_z(&out);
    // The pathspec filter limits records to this path — but a directory-
    // shaped request ("sub") matches every file under it recursively, so
    // require exactly one entry AND require its path to equal the request.
    // The exact match rejects directory-shaped requests outright, keeping
    // the follow-up checkout/add/rm scoped to the single conflicted file
    // (a directory pathspec would otherwise also revert/stage uncommitted
    // sibling changes under it).
    match entries.as_slice() {
        [(xy, p)] if p == path && porcelain_conflict_kind(xy).is_some() => {
            Ok(porcelain_conflict_kind(xy).expect("checked above"))
        }
        _ => Err(BambooError::invalid_argument(format!(
            "{path:?} is not a conflicted file (its state may have changed \
             since the UI snapshot — refresh and retry)"
        ))),
    }
}

/// Refuse to commit while any unmerged index entries exist (commit
/// guard; engine-side so no frontend interception is required).
async fn assert_no_unmerged_paths(repo_path: &std::path::Path) -> anyhow::Result<()> {
    let out = run_git_checked(repo_path, &["status", "--porcelain", "-z"], &[]).await?;
    if let Some((_, path)) = parse_porcelain_z(&out)
        .into_iter()
        .find(|(xy, _)| porcelain_conflict_kind(xy).is_some())
    {
        anyhow::bail!(
            "cannot commit: {path:?} has unresolved merge conflicts \
             (resolve them in the Commit view first)"
        );
    }
    Ok(())
}

/// Resolve one conflicted path: fresh classification → legality check →
/// the mapped git commands (`checkout --ours/--theirs` + `add`, or
/// `rm -f` for confirmed deletions). Runs on the write queue but
/// bypasses the undo journal (see the WriteCommand::ResolveConflict
/// declaration for the reasoning).
async fn resolve_conflict(
    session: &Session,
    path: &str,
    resolution: ConflictResolution,
) -> anyhow::Result<String> {
    validate_path_arg(path)?;
    if path.ends_with('/') {
        anyhow::bail!("conflict paths must name files, not directories: {path:?}");
    }

    let kind = classify_conflict_path(session, path).await?;
    let action = action_for(kind, resolution)?;

    let repo_path = &session.repo_path;
    let paths = [path.to_string()];
    let summary = match action {
        ConflictAction::TakeOurs => {
            run_git_checked(repo_path, &["checkout", "--ours"], &paths).await?;
            run_git_checked(repo_path, &["add"], &paths).await?;
            format!("resolved conflict in {path:?} (kept ours)")
        }
        ConflictAction::TakeTheirs => {
            run_git_checked(repo_path, &["checkout", "--theirs"], &paths).await?;
            run_git_checked(repo_path, &["add"], &paths).await?;
            format!("resolved conflict in {path:?} (kept theirs)")
        }
        ConflictAction::Remove => {
            // -f: unmerged entries (and the worktree copy of the other
            // side's content) go without a staged-content comparison.
            run_git_checked(repo_path, &["rm", "-f"], &paths).await?;
            format!("resolved conflict in {path:?} (confirmed the deletion)")
        }
    };
    Ok(summary)
}

/// Run one git subcommand, optionally with a `--`-separated path list
/// (rendered as `:(literal)` pathspecs, see `literal_pathspecs`). Returns
/// combined stdout+stderr on success and an error carrying stderr on
/// failure.
async fn run_git_checked(
    repo_path: &std::path::Path,
    args: &[&str],
    paths: &[String],
) -> anyhow::Result<String> {
    // Shared subprocess construction: non-interactive auth, closed
    // stdin, captured output (see base_git_command).
    let mut c = base_git_command(repo_path);
    c.args(args);
    if !paths.is_empty() {
        c.arg("--").args(literal_pathspecs(paths));
    }
    let result = c.output().await?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    if !result.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            redact_credentials(&stderr).trim()
        );
    }
    // NOT trimmed (and NOT redacted): porcelain `-z` records are
    // NUL-terminated and may carry significant leading whitespace
    // (" M path") — machine-parsed output must stay verbatim; callers
    // that want a display string trim themselves.
    Ok(format!("{stdout}{stderr}"))
}

/// Switch branches with real auto-stash semantics. The
/// dirty-check → stash → operate → pop-back flow is shared with `pull`
/// (see `run_with_auto_stash`); only the main operation differs.
/// 1. `git stash push --include-untracked` (only when the worktree is dirty;
///    a failure aborts the switch);
/// 2. `git switch <name>` (on failure the stash is popped back, best-effort);
/// 3. `git stash pop` — if the pop fails, the switch HAS succeeded and the
///    changes are retained in the stash; the error says so explicitly so
///    the user is not misled into thinking nothing happened.
async fn switch_branch(
    session: &Session,
    name: &str,
    auto_stash: bool,
) -> anyhow::Result<String> {
    validate_ref_arg(name)?;
    let repo_path = session.repo_path.clone();
    let repo_path_for_op = repo_path.clone();
    let name = name.to_string();
    let op_desc = format!("switch to branch {name:?}");
    run_with_auto_stash(
        &repo_path,
        &op_desc,
        auto_stash,
        move || async move {
            run_git_checked(&repo_path_for_op, &["switch", &name], &[]).await
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Interactive rebase execution (see rebase.rs for todo/state/editors)
// ---------------------------------------------------------------------------

/// Read the in-progress rebase state off the write queue's async context
/// (fs reads + possibly one rev-parse subprocess). A panic in the
/// blocking task propagates as an error rather than silently reading as
/// "no rebase in progress" (which would misclassify real stops).
async fn read_rebase_state_async(session: &Session) -> anyhow::Result<GetRebaseStatusResult> {
    let repo_path = session.repo_path.clone();
    let git_dir = session.git_dir.clone();
    tokio::task::spawn_blocking(move || rebase::read_rebase_state(&repo_path, &git_dir))
        .await
        .map_err(|e| anyhow::anyhow!("rebase state read task failed: {e}"))
}

/// Whether the index has unresolved conflict entries (`git diff --name-only
/// --diff-filter=U` is empty only when it does not).
async fn has_unmerged_paths(repo_path: &std::path::Path) -> anyhow::Result<bool> {
    let out = run_git_checked(repo_path, &["diff", "--name-only", "--diff-filter=U"], &[])
        .await?;
    Ok(!out.is_empty())
}

/// Execute `startRebase`: freshness-gate the client todo against the
/// CURRENT `upstream..branch` commit chain (a stale plan can never
/// silently drop or add commits — any deviation is INVALID_ARGUMENT
/// before any subprocess runs), write the todo + message sidecars, then
/// let `git rebase -i --no-autosquash --onto <onto> <merge-base> <branch>`
/// do the actual replay with the engine self-invoked as
/// GIT_SEQUENCE_EDITOR/GIT_EDITOR (non-interactive, no terminal).
///
/// No wall-clock timeout: a large rebase legitimately runs long, and the
/// single-consumer queue being blocked is the same trade-off the undo
/// journal already makes for every journaled write.
async fn start_rebase_flow(
    session: &Session,
    onto: &str,
    branch: Option<&str>,
    todo: Vec<RebaseTodoEntry>,
) -> anyhow::Result<RebaseResult> {
    validate_ref_arg(onto)
        .map_err(|e| BambooError::invalid_argument(format!("startRebase: {e}")))?;
    if let Some(branch) = branch {
        validate_ref_arg(branch)
            .map_err(|e| BambooError::invalid_argument(format!("startRebase: {e}")))?;
    }

    // Only one rebase at a time: git would refuse anyway ("it seems that
    // there is already a rebase-merge directory"), but a refused start
    // must not be misclassified as a stop of the NEW rebase, must not
    // journal a bogus success tx, and must not overwrite the in-flight
    // rebase's message sidecars. Engine- and externally-started rebases
    // are the same here — the state read is pure on-disk detection.
    if is_in_progress(&read_rebase_state_async(session).await?) {
        return Err(BambooError::invalid_argument(
            "startRebase: a rebase is already in progress (continue, skip or abort it first)",
        )
        .into());
    }

    // Everything up to `write_sidecars` is pure gix/fs work — a stale or
    // malformed todo leaves NO sidecar and spawns NO subprocess.
    let (base_sha, chain, branch_name, branch_tip) = {
        let repo = session.gix_repo.lock().unwrap_or_else(|e| e.into_inner());
        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => rebase::current_branch_name(&session.git_dir, "startRebase")?,
        };
        let onto_id = rebase::resolve_rev_commit(&repo, onto, "startRebase: onto")?;
        let branch_id = rebase::resolve_rev_commit(
            &repo,
            &format!("refs/heads/{branch_name}"),
            "startRebase: branch",
        )?;
        let Some((base, chain)) = rebase::rebase_chain(&repo, onto_id, branch_id)? else {
            return Err(BambooError::invalid_argument(
                "startRebase: onto and branch share no common ancestor",
            )
            .into());
        };
        (base.to_string(), chain, branch_name, branch_id.to_string())
    };

    // Nothing to replay (branch already contained in onto, or a
    // merges-only range): git's "nothing to do" refusal is a legitimate
    // completed outcome, not an error — short-circuit it without any
    // sidecar or subprocess.
    if chain.is_empty() {
        info!(onto, branch = %branch_name, "startRebase: nothing to replay");
        return Ok(RebaseResult::default());
    }

    let (lines, messages) = build_validated_todo(&todo, &chain)?;
    write_sidecars(&session.git_dir, &lines, &messages, &branch_tip)?;

    // Editors point at the engine itself (re-invoked with
    // --internal-sequence-editor/--internal-message-editor, see rebase.rs
    // and main.rs): git appends the file to edit as the final argument.
    let seq_editor = rebase::sequence_editor_value(&session.git_dir)?;
    let msg_editor = rebase::message_editor_value(&session.git_dir)?;
    let argv = [
        "rebase",
        "-i",
        "--no-autosquash",
        "--onto",
        onto,
        &base_sha,
        &branch_name,
    ];
    info!(
        onto,
        branch = %branch_name,
        upstream = %base_sha,
        entries = lines.len(),
        "starting interactive rebase"
    );
    let out = match base_git_command(&session.repo_path)
        .args(argv)
        .env("GIT_SEQUENCE_EDITOR", &seq_editor)
        .env("GIT_EDITOR", &msg_editor)
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            rebase::cleanup_sidecars(&session.git_dir);
            return Err(e.into());
        }
    };

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let result =
        classify_rebase_completion(session, out.status.success(), stderr).await;
    match result {
        Ok(result) => {
            if !result.stopped {
                rebase::cleanup_sidecars(&session.git_dir);
                // The tx binding release happens in run_write (the post
                // snapshot is captured by its commit).
            }
            Ok(result)
        }
        Err(e) => {
            // A hard failure leaves no rebase in progress (classify only
            // errors in that case): drop the sidecars so a later
            // externally-started rebase never inherits a stale run.
            rebase::cleanup_sidecars(&session.git_dir);
            Err(e)
        }
    }
}

/// Execute `rebaseContinue` / `rebaseSkip` / `rebaseAbort` on the current
/// in-progress rebase (engine- or externally-started). Journaled undo is
/// NOT re-entered here — see the WriteCommand declarations; the bound
/// StartRebase tx's post snapshot is refreshed when the rebase completes
/// so the undo safety check tracks the real outcome.
async fn rebase_step(session: &Session, cmd: &WriteCommand) -> anyhow::Result<RebaseResult> {
    let pre_state = read_rebase_state_async(session).await?;
    if !is_in_progress(&pre_state) {
        return match cmd {
            // Idempotent: a double-clicked continue (or one racing the
            // rebase's completion) sees "nothing left to continue" and
            // that IS the success state.
            WriteCommand::RebaseContinue => Ok(RebaseResult::default()),
            _ => Err(BambooError::invalid_argument("no rebase in progress").into()),
        };
    }

    let argv: Vec<&str> = match cmd {
        WriteCommand::RebaseContinue => {
            // Unresolved conflicts would make git refuse ("cannot rebase:
            // you have unstaged changes" / re-stop immediately); fail fast
            // with a structured error the client can act on.
            if has_unmerged_paths(&session.repo_path).await? {
                return Err(BambooError::invalid_argument(
                    "rebaseContinue: resolve the conflicts (and stage the resolutions) first",
                )
                .into());
            }
            vec!["rebase", "--continue"]
        }
        WriteCommand::RebaseSkip => vec!["rebase", "--skip"],
        WriteCommand::RebaseAbort => vec!["rebase", "--abort"],
        _ => unreachable!("rebase_step only handles rebase step commands"),
    };

    // GIT_EDITOR stays wired to the message sidecar on continue/skip: a
    // reword message stop that survived into a continue still needs the
    // custom message (the sidecar lookup is by stopped sha, so stale
    // entries are simply never matched).
    let msg_editor = rebase::message_editor_value(&session.git_dir)?;
    info!(op = ?argv, "advancing in-progress rebase");
    let out = base_git_command(&session.repo_path)
        .args(&argv)
        .env("GIT_EDITOR", &msg_editor)
        .output()
        .await?;
    let ok = out.status.success();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if matches!(cmd, WriteCommand::RebaseAbort) {
        if !ok {
            anyhow::bail!("git rebase --abort failed: {}", redact_credentials(&stderr).trim());
        }
        // git restored the branch ref/worktree to the pre-rebase state;
        // refresh the journaled tx's post snapshot to match so the undo
        // stack stays consistent (undo of the rebase tx becomes a clean
        // restore of the same state).
        rebase::cleanup_sidecars(&session.git_dir);
        refresh_rebase_tx(session);
        refresh_latest_tx(session);
        session.take_rebase_tx();
        return Ok(RebaseResult::default());
    }

    let result = classify_rebase_completion(session, ok, stderr.clone()).await?;
    // A continue that did not advance the rebase (same stopped sha AND
    // same progress counter) means git REFUSED it — e.g. "the previous
    // cherry-pick is now empty, possibly you want to skip it" — and
    // reporting a stop would make the misstop look like the reword-stop
    // the client knows how to handle. Distinguish it as INVALID_ARGUMENT
    // with git's stderr hint so the client can surface the real reason.
    if matches!(cmd, WriteCommand::RebaseContinue)
        && result.stopped
        && result.stopped_sha == pre_state.stopped_sha
        && result.msgnum == pre_state.msgnum
    {
        return Err(BambooError::invalid_argument(format!(
            "rebaseContinue: the rebase did not advance: {}",
            redact_credentials(&stderr).trim()
        ))
        .into());
    }
    if !result.stopped {
        rebase::cleanup_sidecars(&session.git_dir);
        // The rebase the StartRebase tx journaled is now complete: rewrite
        // that tx's post snapshot to the final state, otherwise the undo
        // safety check would compare against the stale stopped-state
        // snapshot and block every later undo. Later txs (e.g. the Stage
        // that resolved the conflict) have the same problem — refresh the
        // LATEST tx too, which covers them.
        refresh_rebase_tx(session);
        refresh_latest_tx(session);
        session.take_rebase_tx();
    }
    Ok(result)
}

/// Map one `git rebase` invocation's exit to the protocol result:
///
/// * exit 0, no on-disk rebase state → finished;
/// * any exit WITH on-disk rebase state → stopped (normal outcome;
///   reason/progress read from the state, conflicts detected from the
///   index);
/// * non-zero exit with no state → a real error, EXCEPT "No rebase in
///   progress?" which is a racing completion (idempotent success).
async fn classify_rebase_completion(
    session: &Session,
    ok: bool,
    stderr: String,
) -> anyhow::Result<RebaseResult> {
    let state = read_rebase_state_async(session).await?;
    if !is_in_progress(&state) {
        if ok {
            return Ok(RebaseResult::default());
        }
        if stderr.to_lowercase().contains("no rebase in progress") {
            return Ok(RebaseResult::default());
        }
        anyhow::bail!("git rebase failed: {}", redact_credentials(&stderr).trim());
    }
    // On-disk rebase state: stopped (also reached defensively when exit
    // was 0 — git should not exit 0 with state left behind, but if it
    // does, reporting the stop is strictly safer than reporting done).
    let has_unmerged = has_unmerged_paths(&session.repo_path).await?;
    Ok(RebaseResult {
        stopped: true,
        reason: Some(classify_stop_reason(has_unmerged)),
        stopped_sha: state.stopped_sha.clone(),
        msgnum: state.msgnum,
        end: state.end,
    })
}

/// Rewrite the journaled StartRebase transaction's post snapshot with the
/// CURRENT repository state (see rebase_step: called when the rebase
/// finishes or is aborted). Best-effort: a missing/pruned tx just warns.
fn refresh_rebase_tx(session: &Session) {
    if let Some(id) = session.current_rebase_tx() {
        if let Err(e) = session.undo.refresh_post_snapshot(id) {
            warn!(
                error = %e,
                transaction_id = id,
                "failed to refresh the rebase tx post snapshot; undo may be blocked"
            );
        }
    }
}

/// Rewrite the LATEST undo transaction's post snapshot with the current
/// state. Conflict-resolution flows (Stage/ResolveConflict during the
/// rebase stop) journal txs whose post snapshots still contain the
/// stopped-rebase state; left as-is, the undo safety check (which
/// compares against the latest tx) blocks undo after the rebase
/// completes. Best-effort: a missing journal just warns.
fn refresh_latest_tx(session: &Session) {
    if let Err(e) = session.undo.refresh_latest_post_snapshot() {
        warn!(
            error = %e,
            "failed to refresh the latest tx post snapshot; undo may be blocked"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::redact_credentials;

    #[test]
    fn redact_strips_url_userinfo() {
        assert_eq!(
            redact_credentials(
                "fatal: Authentication failed for 'https://user:token@host/repo/'"
            ),
            "fatal: Authentication failed for 'https://***@host/repo/'"
        );
        assert_eq!(
            redact_credentials("ssh://git:pat@example.com/o/r"),
            "ssh://***@example.com/o/r"
        );
    }

    #[test]
    fn redact_handles_multiple_and_clean_urls() {
        assert_eq!(redact_credentials("https://host/path"), "https://host/path");
        assert_eq!(redact_credentials("no urls here"), "no urls here");
        assert_eq!(
            redact_credentials("pull from https://a:b@x.io/1 and https://c:d@y.io/2"),
            "pull from https://***@x.io/1 and https://***@y.io/2"
        );
        // A quoted URL with no path component still gets its userinfo
        // stripped and the surrounding text survives verbatim.
        assert_eq!(
            redact_credentials("failed for 'https://tok@host' then stopped"),
            "failed for 'https://***@host' then stopped"
        );
    }
}
