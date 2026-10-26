use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use bamboo_protocol::{BambooError, ForceMode, PullStrategy};

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
    Commit { message: String, amend: bool },
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
            WriteCommand::Commit { amend, .. } => {
                if *amend {
                    "Commit(amend)".to_string()
                } else {
                    "Commit".to_string()
                }
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
            WriteCommand::Undo { .. } => "Undo".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WriteResult {
    Ok(String),
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
            _ => run_write(&session, &cmd).await,
        };

        let success = matches!(result, WriteResult::Ok(_) | WriteResult::Undo(Ok(_)));

        // Append audit log entry (best-effort).
        if let Err(e) = session.undo.append_audit_log(&desc, epoch, &result, tx_id) {
            error!(error = %e, "failed to write audit log");
        }

        if success {
            // Bump the epoch synchronously so clients can immediately
            // refresh with a valid epoch. (The fs watcher bumps it again
            // when it observes the .git writes — double bumps are fine,
            // the epoch is just an invalidation counter.)
            let new_epoch = session.bump_epoch();
            if matches!(cmd, WriteCommand::Undo { .. }) {
                session.emit(EngineEvent::GraphInvalidated { epoch: new_epoch });
            }
            if let WriteResult::Ok(output) = &result {
                info!(output = %output, "write command succeeded");
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

    let result = execute_command(session, cmd).await;

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
        Ok(output) => (WriteResult::Ok(output), tx_id),
        // Structured protocol errors (STASH_CONFLICT, PUSH_REJECTED,
        // AUTH_REQUIRED) survive; everything else degrades to a plain
        // git error carrying git's stderr.
        Err(e) => {
            let mapped = match e.downcast::<BambooError>() {
                Ok(structured) => structured,
                Err(e) => BambooError::git_error(e.to_string()),
            };
            (WriteResult::Err(mapped), tx_id)
        }
    }
}

/// Execute an undo. Runs the (synchronous, git-shelling) undo engine on a
/// blocking thread — still serialized behind this single-consumer queue.
async fn run_undo(session: Arc<Session>, transaction_id: u64) -> (WriteResult, u64) {
    let result = tokio::task::spawn_blocking(move || session.undo.undo(transaction_id)).await;
    let result: Result<UndoOutcome, UndoError> = match result {
        Ok(r) => r,
        Err(e) => Err(UndoError::Io(anyhow::anyhow!("undo task failed: {e}"))),
    };
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

    // -z records are NUL-terminated and unquoted; a rename/copy entry
    // carries the original path as an extra trailing field that must be
    // consumed, never parsed as an entry itself.
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

    // Unmerged index states (both sides diverge); `AA`/`DD` also only
    // arise from unmerged index entries.
    const CONFLICT_CODES: [&str; 7] = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];
    if let Some((_, path)) = entries
        .iter()
        .find(|(xy, _)| CONFLICT_CODES.contains(&xy.as_str()))
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
