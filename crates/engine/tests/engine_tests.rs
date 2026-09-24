use std::process::Command;

/// Create a temp git repo with some commits and branches for testing.
fn test_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()?;
        if !out.status.success() {
            anyhow::bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };

    git(&["init", "-q", "-b", "main"])?;
    // Use distinct, increasing commit timestamps so newest-first ordering is deterministic.
    let commit_at = |msg: &str, date: &str| -> anyhow::Result<()> {
        let out = Command::new("git")
            .arg("commit")
            .arg("-q")
            .arg("-m")
            .arg(msg)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()?;
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    };
    std::fs::write(dir.path().join("a.txt"), "a\n")?;
    git(&["add", "."])?;
    commit_at("first", "2026-01-01T00:00:00Z")?;
    std::fs::write(dir.path().join("b.txt"), "b\n")?;
    git(&["add", "."])?;
    commit_at("second", "2026-01-02T00:00:00Z")?;
    git(&["branch", "feature"])?;
    std::fs::write(dir.path().join("c.txt"), "c\n")?;
    git(&["add", "."])?;
    commit_at("third", "2026-01-03T00:00:00Z")?;

    Ok(dir)
}

/// Open a session with a live (but unconsumed) notification channel.
async fn open_session(
    dir: &tempfile::TempDir,
) -> anyhow::Result<std::sync::Arc<git_workbench_engine::session::Session>> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    git_workbench_engine::session::Session::open(dir.path(), tx).await
}

#[tokio::test]
async fn session_open_bumps_epoch() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;
    assert_eq!(session.current_epoch(), 1);
    let e = session.bump_epoch();
    assert_eq!(e, 2);
    assert_eq!(session.current_epoch(), 2);
    Ok(())
}

#[tokio::test]
async fn graph_page_reads_commits() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let viewport = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 3);
    assert_eq!(page.epoch, 1);
    assert!(!page.has_more);

    // Newest first.
    assert!(page.commits[0].message.contains("third"));
    assert!(page.commits[1].message.contains("second"));
    assert!(page.commits[2].message.contains("first"));

    // Parent links.
    assert_eq!(page.commits[0].parent_ids.len(), 1);
    assert_eq!(page.commits[0].parent_ids[0], page.commits[1].id);
    assert!(page.commits[2].parent_ids.is_empty());

    // Metadata.
    assert_eq!(page.commits[0].author_name, "Test");
    assert_eq!(page.commits[0].author_email, "test@example.com");
    Ok(())
}

#[tokio::test]
async fn graph_page_epoch_mismatch() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let viewport = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 999, // stale
    };
    let err = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)
        .expect_err("should fail");
    let wb_err = err
        .downcast_ref::<git_workbench_protocol::WorkbenchError>()
        .expect("should be WorkbenchError");
    assert_eq!(wb_err.code, git_workbench_protocol::WorkbenchError::EPOCH_MISMATCH);
    Ok(())
}

#[tokio::test]
async fn graph_page_pagination() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let viewport = git_workbench_protocol::GraphViewport {
        offset: 1,
        limit: 1,
        anchor_commit: None,
        epoch: 0, // 0 = skip check
    };
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 1);
    assert!(page.commits[0].message.contains("second"));
    assert!(page.has_more);
    Ok(())
}

#[tokio::test]
async fn status_reads_untracked_and_staged() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Create an untracked file.
    std::fs::write(dir.path().join("d.txt"), "d\n")?;
    // Modify a tracked file.
    std::fs::write(dir.path().join("a.txt"), "modified\n")?;

    let items = git_workbench_engine::gix_read::read_status(&session, None)?;
    let has_untracked = items
        .iter()
        .any(|i| i.path == "d.txt" && i.status == git_workbench_protocol::StatusCode::Untracked);
    let has_modified = items
        .iter()
        .any(|i| i.path == "a.txt" && i.status == git_workbench_protocol::StatusCode::Modified);
    assert!(has_untracked, "expected untracked d.txt, got {:?}", items);
    assert!(has_modified, "expected modified a.txt, got {:?}", items);

    // Path filter.
    let filtered = git_workbench_engine::gix_read::read_status(&session, Some(&["d.txt".into()]))?;
    assert!(filtered.iter().all(|i| i.path.starts_with("d.txt")));
    assert!(!filtered.is_empty());
    Ok(())
}

#[tokio::test]
async fn refs_read_includes_branches() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let refs = git_workbench_engine::gix_read::read_refs(&session)?;
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"feature"));
    assert!(refs.iter().all(|r| !r.target.is_empty()));
    Ok(())
}

#[tokio::test]
async fn undo_snapshot_and_list() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let id = session.undo.begin("stage files", 1)?;
    assert!(id >= 1);
    session.undo.commit(id, "stage files", 1, true)?;

    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].description, "stage files");
    assert_eq!(entries[0].epoch_at_creation, 1);

    // Transactions are persisted across engine restarts.
    let undo2 = git_workbench_engine::undo::UndoEngine::new(
        dir.path(),
        &dir.path().join(".git"),
    )?;
    let entries2 = undo2.list_transactions(10)?;
    assert_eq!(entries2.len(), 1);
    Ok(())
}

#[tokio::test]
async fn write_queue_stage_and_epoch_bump() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    std::fs::write(dir.path().join("a.txt"), "modified\n")?;

    // Stage the new file through the write queue.
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;

    // Epoch must have bumped (the write queue bump plus possible fs
    // watcher bumps from the git add writing .git/index).
    assert!(session.current_epoch() >= 2);

    settle().await;
    // The file should now appear as staged (tree-index change).
    let items = git_workbench_engine::gix_read::read_status(&session, None)?;
    let staged = items
        .iter()
        .any(|i| i.path == "new.txt" && i.status == git_workbench_protocol::StatusCode::Added && i.staged);
    assert!(staged, "expected staged new.txt, got {:?}", items);
    // Worktree changes must be flagged as unstaged.
    let unstaged = items
        .iter()
        .any(|i| i.path == "a.txt" && !i.staged);
    assert!(unstaged, "expected unstaged a.txt, got {:?}", items);

    // An undo snapshot and audit entry should exist.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let audit = std::fs::read_to_string(
        dir.path().join(".git").join("git-workbench").join("audit.log"),
    )?;
    assert!(audit.contains("\"status\":\"ok\""));
    Ok(())
}

/// Wait for the fs watcher to finish processing the .git writes of a
/// recent mutation, so subsequent reads don't race the epoch bump
/// (reads re-check the epoch after walking and would otherwise return a
/// transient EPOCH_MISMATCH).
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn write_queue_commit_and_graph_refresh() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Commit {
            message: "fourth".to_string(),
            amend: false,
        })
        .await?;

    settle().await;
    // Graph should now contain 4 commits.
    let viewport = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 4);
    assert!(page.commits[0].message.contains("fourth"));
    Ok(())
}

#[tokio::test]
async fn undo_restores_refs_after_commit() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    let entries = session.undo.list_transactions(10)?;
    let stage_tx = entries[0].id;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Commit {
            message: "fourth".to_string(),
            amend: false,
        })
        .await?;

    settle().await;
    // 4 commits now.
    let viewport = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 4);

    // Undo the commit (the latest transaction).
    let entries = session.undo.list_transactions(10)?;
    let commit_tx = entries[0].id;
    let outcome = session.undo.undo(commit_tx)?;
    assert!(matches!(
        outcome,
        git_workbench_engine::undo::UndoOutcome::Restored { .. }
    ));

    settle().await;
    // Graph back to 3 commits, main restored to "third".
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 3);
    assert!(page.commits[0].message.contains("third"));

    // The stage transaction is still on the stack; undoing it (a failed-less
    // no-op for refs) pops it.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, stage_tx);
    let outcome = session.undo.undo(stage_tx)?;
    assert!(matches!(outcome, git_workbench_engine::undo::UndoOutcome::Restored { .. }));
    assert!(session.undo.list_transactions(10)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn undo_blocked_on_external_change() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Our journaled write: stage + commit.
    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Commit {
            message: "fourth".to_string(),
            amend: false,
        })
        .await?;

    // External write (not journaled): move refs/heads/main.
    std::fs::write(dir.path().join("ext.txt"), "ext\n")?;
    let output = std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir.path())
        .output()?;
    assert!(output.status.success());
    let output = std::process::Command::new("git")
        .args(["commit", "-m", "external"])
        .current_dir(dir.path())
        .output()?;
    assert!(output.status.success());

    // Undo must be blocked: current refs != latest post-snapshot.
    let entries = session.undo.list_transactions(10)?;
    let commit_tx = entries[0].id;
    let result = session.undo.undo(commit_tx);
    assert!(
        matches!(
            result,
            Err(git_workbench_engine::undo::UndoError::Blocked { .. })
        ),
        "expected Blocked, got {:?}",
        result
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Framing (server)
// ---------------------------------------------------------------------------

use std::io::Cursor;

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

fn read_message(
    data: Vec<u8>,
) -> anyhow::Result<Option<(Option<serde_json::Value>, String, serde_json::Value)>> {
    let mut reader = Cursor::new(data);
    git_workbench_engine::server::read_message(&mut reader)
}

#[test]
fn framing_parses_valid_message() {
    let msg = read_message(frame(r#"{"jsonrpc":"2.0","id":1,"method":"getRefs"}"#))
        .unwrap()
        .unwrap();
    assert_eq!(msg.1, "getRefs");
    assert_eq!(msg.0, Some(serde_json::json!(1)));

    // Notifications (no id) parse with id = None.
    let msg = read_message(frame(r#"{"jsonrpc":"2.0","method":"ping"}"#))
        .unwrap()
        .unwrap();
    assert_eq!(msg.0, None);
}

#[test]
fn framing_rejects_oversized_content_length() {
    let data = format!("Content-Length: {}\r\n\r\n{{}}", 17 * 1024 * 1024);
    assert!(read_message(data.into_bytes()).is_err());
}

#[test]
fn framing_rejects_oversized_header_block() {
    // Many small headers: total block far exceeds 64 KiB.
    let mut data = Vec::new();
    for i in 0..3000 {
        data.extend_from_slice(format!("X-Header-{i}: aaaaaaaaaaaaaaaaaaaaaaaaaa\r\n").as_bytes());
    }
    data.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
    assert!(read_message(data).is_err());
}

#[test]
fn framing_rejects_overlong_header_line() {
    let long = "X-Big: ".to_string() + &"a".repeat(9 * 1024) + "\r\n";
    let data = format!("{long}Content-Length: 2\r\n\r\n{{}}");
    assert!(read_message(data.into_bytes()).is_err());
}

#[test]
fn framing_eof_mid_header_returns_none() {
    // Clean EOF: no error, no message.
    assert!(read_message(b"Content-Length: 5\r\n".to_vec()).unwrap().is_none());
    assert!(read_message(Vec::new()).unwrap().is_none());
}

#[test]
fn framing_eof_mid_body_is_error() {
    // Truncated body: the stream is desynced, so this must be an error
    // (the connection is closed by the reader loop).
    assert!(read_message(b"Content-Length: 50\r\n\r\n{}".to_vec()).is_err());
}

#[test]
fn framing_rejects_missing_content_length() {
    assert!(read_message(b"Content-Type: application/json\r\n\r\n{}".to_vec()).is_err());
}

// ---------------------------------------------------------------------------
// Durable epoch + repo fingerprint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn epoch_survives_restart_and_bumps_on_repo_change() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let s1 = open_session(&dir).await?;
    assert_eq!(s1.current_epoch(), 1);
    // redb holds an exclusive per-file lock: drop the session (and its
    // cache handle) before "restarting" the engine.
    drop(s1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Restart with no repo changes: the stored epoch is reused.
    let s2 = open_session(&dir).await?;
    assert_eq!(s2.current_epoch(), 1);
    drop(s2);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Refs move while the engine is down.
    let out = Command::new("git")
        .args(["commit", "--allow-empty", "-m", "external"])
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()?;
    assert!(out.status.success());

    // Restart: the fingerprint differs, so the cache is cleared and the
    // epoch is bumped (stored + 1) — no stale lanes can be served.
    let s3 = open_session(&dir).await?;
    assert_eq!(s3.current_epoch(), 2);
    drop(s3);
    Ok(())
}

// ---------------------------------------------------------------------------
// Anchor re-pinning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_page_anchor_repins_viewport() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Full list to find the anchor commit.
    let viewport = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = git_workbench_engine::gix_read::read_graph_page(&session, &viewport)?;
    let anchor = page.commits[1].id.clone(); // "second"

    // Requested offset 0, but the anchor pins the page at its index.
    let vp = git_workbench_protocol::GraphViewport {
        offset: 0,
        limit: 2,
        anchor_commit: Some(anchor.clone()),
        epoch: 0,
    };
    let anchored = git_workbench_engine::gix_read::read_graph_page(&session, &vp)?;
    assert_eq!(anchored.commits[0].message, "second");
    assert_eq!(anchored.commits.len(), 2);
    assert_eq!(anchored.anchor_commit.as_deref(), Some(anchor.as_str()));

    // A new commit lands on top: the anchor keeps the same commits in view
    // even though the absolute offset would have scrolled past them.
    std::fs::write(dir.path().join("e.txt"), "e\n")?;
    let out = Command::new("git")
        .args(["add", "."])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());
    let out = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(["commit", "-m", "newest"])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());
    settle().await;

    let anchored_again = git_workbench_engine::gix_read::read_graph_page(&session, &vp)?;
    assert_eq!(anchored_again.commits[0].message, "second");

    // Unknown anchor: the requested offset is kept.
    let vp = git_workbench_protocol::GraphViewport {
        offset: 1,
        limit: 1,
        anchor_commit: Some("0000000000000000000000000000000000000000".to_string()),
        epoch: 0,
    };
    let fallback = git_workbench_engine::gix_read::read_graph_page(&session, &vp)?;
    assert_eq!(fallback.commits.len(), 1);
    assert_eq!(fallback.commits[0].message, "third");
    Ok(())
}

// ---------------------------------------------------------------------------
// Component-aware path filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_path_filter_is_component_aware() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::create_dir_all(dir.path().join("dir"))?;
    std::fs::write(dir.path().join("dir").join("x.txt"), "x\n")?;
    std::fs::write(dir.path().join("dirx.txt"), "y\n")?;

    let items = git_workbench_engine::gix_read::read_status(
        &session,
        Some(&["dir".to_string()]),
    )?;
    // "dir" matches the untracked directory entry (gix may collapse it to
    // "dir" or report "dir/x.txt"), but NOT the sibling "dirx.txt".
    assert!(!items.is_empty());
    assert!(items
        .iter()
        .all(|i| i.path == "dir" || i.path.starts_with("dir/")));

    // Exact file matches still work (use a non-directory sibling).
    let items = git_workbench_engine::gix_read::read_status(
        &session,
        Some(&["dirx.txt".to_string()]),
    )?;
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i.path == "dirx.txt"));

    // A prefix that is not a component boundary must not match.
    let items = git_workbench_engine::gix_read::read_status(
        &session,
        Some(&["dirx".to_string()]),
    )?;
    assert!(items.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Real auto-stash on branch switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switch_branch_auto_stash_carries_changes() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Dirty worktree (tracked + untracked files).
    std::fs::write(dir.path().join("a.txt"), "modified\n")?;
    std::fs::write(dir.path().join("untracked.txt"), "u\n")?;

    let output = session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::SwitchBranch {
            name: "feature".to_string(),
            auto_stash: true,
        })
        .await?;
    assert!(!output.is_empty());

    // The switch happened and both changes were carried over by the stash.
    let head = std::fs::read_to_string(dir.path().join(".git").join("HEAD"))?;
    assert!(head.contains("feature"));
    assert!(std::fs::read_to_string(dir.path().join("a.txt"))?.contains("modified"));
    assert!(std::fs::read_to_string(dir.path().join("untracked.txt"))?.contains("u"));

    // Nothing left in the stash.
    let out = Command::new("git")
        .args(["stash", "list"])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Redacted op summaries in the journal/audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn journal_and_audit_redact_message_and_paths() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("secret.txt"), "s\n")?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "secret.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Commit {
            message: "SECRET MESSAGE".to_string(),
            amend: false,
        })
        .await?;

    let git_wb = dir.path().join(".git").join("git-workbench");
    let journal = std::fs::read_to_string(git_wb.join("undo").join("journal.jsonl"))?;
    assert!(journal.contains("Stage(1 paths)"));
    assert!(journal.contains("Commit"));
    assert!(!journal.contains("secret.txt"));
    assert!(!journal.contains("SECRET MESSAGE"));

    let audit = std::fs::read_to_string(git_wb.join("audit.log"))?;
    assert!(!audit.contains("secret.txt"));
    assert!(!audit.contains("SECRET MESSAGE"));
    Ok(())
}

#[tokio::test]
async fn blob_read_from_head_and_missing_path() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let content =
        git_workbench_engine::gix_read::read_blob(&session, "HEAD", "a.txt")?;
    assert_eq!(content.trim(), "a");

    // Missing path resolves to empty content, not an error.
    let missing =
        git_workbench_engine::gix_read::read_blob(&session, "HEAD", "no/such/file.txt")?;
    assert_eq!(missing, "");

    // Option-like revisions are rejected defensively (no shell involved).
    assert!(
        git_workbench_engine::gix_read::read_blob(&session, "--upload-pack=x", "a.txt").is_err()
    );
    Ok(())
}

#[tokio::test]
async fn head_read_reports_branch_and_commit() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let head = git_workbench_engine::gix_read::read_head(&session)?;
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!(head.head.len(), 40, "expected a full sha, got {:?}", head.head);

    // Detached HEAD: branch is None, sha still present.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let sha = head.head.clone();
    assert!(git(&["checkout", "-q", &sha]));
    let detached = git_workbench_engine::gix_read::read_head(&session)?;
    assert_eq!(detached.branch, None);
    assert_eq!(detached.head, sha);
    Ok(())
}
