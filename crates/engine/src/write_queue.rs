use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::session::{EngineEvent, Session};

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
}

#[derive(Debug, Clone)]
pub enum WriteResult {
    Ok(String),
    Err(String),
}

pub async fn run_write_queue(
    mut rx: mpsc::Receiver<(WriteCommand, tokio::sync::oneshot::Sender<WriteResult>)>,
    session: Arc<Session>,
) {
    while let Some((cmd, reply)) = rx.recv().await {
        let epoch = session.current_epoch();
        let desc = format!("{:?}", cmd);
        info!(cmd = %desc, epoch, "executing write command");

        // Allocate the undo transaction (pre-write snapshot).
        let tx_id = match session.undo.begin(&desc, epoch) {
            Ok(id) => id,
            Err(e) => {
                error!(error = %e, "failed to begin undo transaction");
                let _ = reply.send(WriteResult::Err(format!(
                    "undo snapshot failed: {e}"
                )));
                continue;
            }
        };

        let result = execute_command(&session, &cmd).await;

        // Finalize the undo transaction (post-write snapshot + journal entry).
        let success = result.is_ok();
        if let Err(e) = session.undo.commit(tx_id, &desc, epoch, success) {
            error!(error = %e, "failed to finalize undo transaction");
        }

        match &result {
            Ok(output) => {
                // Bump the epoch synchronously so clients can immediately
                // refresh with a valid epoch. (The fs watcher bumps it again
                // when it observes the .git writes — double bumps are fine,
                // the epoch is just an invalidation counter.)
                session.bump_epoch();
                info!(tx_id, output = %output, "write command succeeded");
                let _ = reply.send(WriteResult::Ok(output.clone()));
            }
            Err(e) => {
                warn!(tx_id, error = %e, "write command failed");
                let _ = reply.send(WriteResult::Err(format!("{e}")));
            }
        }

        // Append audit log entry.
        let audit_result = match &result {
            Ok(output) => WriteResult::Ok(output.clone()),
            Err(e) => WriteResult::Err(format!("{e}")),
        };
        if let Err(e) = session.undo.append_audit_log(&desc, epoch, &audit_result, tx_id) {
            error!(error = %e, "failed to write audit log");
        }
    }

    info!("write queue shutdown");
}

async fn execute_command(session: &Session, cmd: &WriteCommand) -> anyhow::Result<String> {
    let repo_path = &session.repo_path;
    let mut output = match cmd {
        WriteCommand::Stage(paths) => {
            let mut c = Command::new("git");
            c.arg("add").args(paths).current_dir(repo_path);
            c
        }
        WriteCommand::Unstage(paths) => {
            let mut c = Command::new("git");
            c.arg("reset").arg("HEAD").args(paths).current_dir(repo_path);
            c
        }
        WriteCommand::Commit { message, amend } => {
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
            let mut c = Command::new("git");
            c.arg("branch").arg(name).current_dir(repo_path);
            if let Some(base) = base {
                c.arg(base);
            }
            c
        }
        WriteCommand::DeleteBranch { name, force } => {
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
        WriteCommand::SwitchBranch { name, auto_stash } => {
            let mut c = Command::new("git");
            c.arg("switch").arg(name).current_dir(repo_path);
            if *auto_stash {
                c.arg("--merge");
            }
            c
        }
        WriteCommand::Fetch { remote } => {
            let mut c = Command::new("git");
            c.arg("fetch").current_dir(repo_path);
            if let Some(remote) = remote {
                c.arg(remote);
            }
            c
        }
        WriteCommand::Pull { remote, branch } => {
            let mut c = Command::new("git");
            c.arg("pull").current_dir(repo_path);
            if let Some(remote) = remote {
                c.arg(remote);
                if let Some(branch) = branch {
                    c.arg(branch);
                }
            }
            c
        }
        WriteCommand::Push { remote, branch, force } => {
            let mut c = Command::new("git");
            c.arg("push").current_dir(repo_path);
            if *force {
                c.arg("--force-with-lease");
            }
            c.arg(remote).arg(branch);
            c
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

/// Emit a synthetic IndexChanged/RefsChanged event (used after undo restores
/// refs via direct CLI calls, since the fs watcher also covers it, this is
/// mostly a safety net).
#[allow(dead_code)]
pub fn emit_post_undo_events(session: &Session) {
    let epoch = session.bump_epoch();
    session.emit(EngineEvent::GraphInvalidated { epoch });
}
