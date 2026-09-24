use std::process::Stdio;
use std::sync::{Arc, Weak};

use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::session::{EngineEvent, Session};
use crate::undo::{UndoError, UndoOutcome, UndoSummary, UndoErrorKind};

#[derive(Debug, Clone)]
pub enum WriteCommand {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    Commit { message: String, amend: bool },
    CreateBranch { name: String, base: Option<String> },
    DeleteBranch { name: String, force: bool },
    SwitchBranch { name: String, auto_stash: bool },
    Fetch { remote: Option<String> },
    Pull { remote: Option<String>, branch: Option<String> },
    Push { remote: String, branch: String, force: bool },
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
            WriteCommand::Pull { .. } => "Pull".to_string(),
            WriteCommand::Push { force, .. } => {
                if *force {
                    "Push(force)".to_string()
                } else {
                    "Push".to_string()
                }
            }
            WriteCommand::Undo { .. } => "Undo".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WriteResult {
    Ok(String),
    Err(String),
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
fn validate_ref_arg(name: &str) -> anyhow::Result<()> {
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
fn validate_pushspec(spec: &str) -> anyhow::Result<()> {
    if spec.is_empty() || spec.starts_with('-') || spec.starts_with('+') {
        anyhow::bail!("invalid push refspec: {spec:?}");
    }
    if spec.chars().any(|c| c.is_ascii_control() || c.is_whitespace()) {
        anyhow::bail!("invalid push refspec: {spec:?}");
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
            return (WriteResult::Err(format!("undo snapshot failed: {e}")), 0);
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
                WriteResult::Err(format!(
                    "git operation succeeded but is NOT undoable (journal write failed): {e}"
                )),
                tx_id,
            );
        }
    }

    match result {
        Ok(output) => (WriteResult::Ok(output), tx_id),
        Err(e) => (WriteResult::Err(format!("{e}")), tx_id),
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
    // SwitchBranch is a multi-step flow (auto-stash), handled separately.
    if let WriteCommand::SwitchBranch { name, auto_stash } = cmd {
        return switch_branch(session, name, *auto_stash).await;
    }

    let repo_path = &session.repo_path;
    let mut output = match cmd {
        WriteCommand::Stage(paths) => {
            for p in paths {
                validate_path_arg(p)?;
            }
            let mut c = Command::new("git");
            // `--` terminates option parsing: paths can never be read as flags.
            c.arg("add").arg("--").args(paths).current_dir(repo_path);
            c
        }
        WriteCommand::Unstage(paths) => {
            for p in paths {
                validate_path_arg(p)?;
            }
            let mut c = Command::new("git");
            c.arg("reset").arg("HEAD").arg("--").args(paths).current_dir(repo_path);
            c
        }
        WriteCommand::Commit { message, amend } => {
            // The message is the value of `-m`, never parsed as an option.
            if message.contains('\0') {
                anyhow::bail!("commit message contains NUL");
            }
            let mut c = Command::new("git");
            c.arg("commit").current_dir(repo_path);
            if *amend {
                c.arg("--amend").arg("--no-edit");
            } else {
                c.arg("-m").arg(message);
            }
            c
        }
        WriteCommand::CreateBranch { name, base } => {
            validate_ref_arg(name)?;
            if let Some(base) = base {
                // Base may be a branch name or a sha.
                validate_ref_arg(base)?;
            }
            let mut c = Command::new("git");
            c.arg("branch").arg(name).current_dir(repo_path);
            if let Some(base) = base {
                c.arg(base);
            }
            c
        }
        WriteCommand::DeleteBranch { name, force } => {
            validate_ref_arg(name)?;
            let mut c = Command::new("git");
            c.arg("branch");
            if *force {
                c.arg("-D");
            } else {
                c.arg("-d");
            }
            c.arg(name).current_dir(repo_path);
            c
        }
        WriteCommand::SwitchBranch { .. } => {
            unreachable!("switchBranch is handled by switch_branch")
        }
        WriteCommand::Fetch { remote } => {
            let mut c = Command::new("git");
            c.arg("fetch").current_dir(repo_path);
            if let Some(remote) = remote {
                validate_ref_arg(remote)?;
                c.arg(remote);
            }
            c
        }
        WriteCommand::Pull { remote, branch } => {
            let mut c = Command::new("git");
            c.arg("pull").current_dir(repo_path);
            if let Some(remote) = remote {
                validate_ref_arg(remote)?;
                c.arg(remote);
                if let Some(branch) = branch {
                    validate_ref_arg(branch)?;
                    c.arg(branch);
                }
            }
            c
        }
        WriteCommand::Push { remote, branch, force } => {
            validate_ref_arg(remote)?;
            validate_pushspec(branch)?;
            let mut c = Command::new("git");
            c.arg("push").current_dir(repo_path);
            if *force {
                c.arg("--force-with-lease");
            }
            c.arg(remote).arg(branch);
            c
        }
        WriteCommand::Undo { .. } => {
            unreachable!("undo is handled by run_undo, not execute_command")
        }
    };

    let result = output
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    if !result.status.success() {
        anyhow::bail!("git command failed: {stderr}");
    }

    Ok(format!("{stdout}{stderr}").trim().to_string())
}

/// Run one git subcommand, returning combined stdout+stderr on success and
/// an error carrying stderr on failure.
async fn run_git_checked(repo_path: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
    let result = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    if !result.status.success() {
        anyhow::bail!("git {:?} failed: {}", args, stderr.trim());
    }
    Ok(format!("{stdout}{stderr}").trim().to_string())
}

/// Switch branches with real auto-stash semantics:
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
    let mut output = String::new();
    let mut stashed = false;

    if auto_stash {
        let dirty = run_git_checked(&repo_path, &["status", "--porcelain"]).await?;
        if !dirty.trim().is_empty() {
            run_git_checked(&repo_path, &["stash", "push", "--include-untracked"])
                .await
                .map_err(|e| anyhow::anyhow!("auto-stash failed; switch aborted: {e}"))?;
            stashed = true;
            output.push_str("stashed local changes; ");
        }
    }

    match run_git_checked(&repo_path, &["switch", name]).await {
        Ok(out) => {
            if !out.is_empty() {
                output.push_str(&out);
            }
        }
        Err(e) => {
            // The switch failed; put the stash back so the user's changes
            // are not stranded (best-effort).
            if stashed {
                if let Err(pop_err) = run_git_checked(&repo_path, &["stash", "pop"]).await {
                    warn!(error = %pop_err, "failed to restore stash after failed switch");
                }
            }
            return Err(e);
        }
    }

    if stashed {
        match run_git_checked(&repo_path, &["stash", "pop"]).await {
            Ok(out) => {
                if !out.is_empty() {
                    output.push('\n');
                    output.push_str(&out);
                }
            }
            Err(e) => {
                anyhow::bail!(
                    "switch to branch {name:?} succeeded, but restoring the stashed changes \
                     failed: {e}\nThe changes are retained in the stash; run `git stash pop` manually."
                );
            }
        }
    }

    Ok(output.trim().to_string())
}
