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
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };

    git(&["init", "-q", "-b", "main"])?;
    // Windows git defaults to core.autocrlf=true, which would rewrite LF to
    // CRLF on every `git checkout` the discard tests perform — making the
    // restored content platform-dependent. Pin the repo to no conversion
    // so the tests assert byte-identical content everywhere.
    git(&["config", "core.autocrlf", "false"])?;
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
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
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
) -> anyhow::Result<std::sync::Arc<bamboo_engine::session::Session>> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    bamboo_engine::session::Session::open(dir.path(), tx).await
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

    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
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
async fn graph_page_survives_annotated_tag() -> anyhow::Result<()> {
    // Regression: annotated tag refs point at tag objects, not commits.
    // The rev walk (and refs) must peel them, otherwise reading the graph
    // fails with "Expected object of kind commit but got tag".
    let dir = test_repo()?;
    let out = Command::new("git")
        .args(["tag", "-a", "v1.0", "-m", "release 1.0"])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let session = open_session(&dir).await?;
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 3);

    // The tag ref must peel to the commit it annotates (the newest one),
    // so the webview can key its tag chip by commit id.
    let refs = bamboo_engine::gix_read::read_refs(&session)?;
    let tag = refs
        .iter()
        .find(|r| r.kind == bamboo_protocol::RefKind::Tag && r.name == "v1.0")
        .expect("v1.0 tag ref should be listed");
    assert_eq!(tag.target, page.commits[0].id);
    assert!(page.commits.iter().any(|c| c.id == tag.target));
    Ok(())
}

#[tokio::test]
async fn graph_page_epoch_mismatch() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 999, // stale
    };
    let err =
        bamboo_engine::gix_read::read_graph_page(&session, &viewport).expect_err("should fail");
    let wb_err = err
        .downcast_ref::<bamboo_protocol::BambooError>()
        .expect("should be BambooError");
    assert_eq!(wb_err.code, bamboo_protocol::BambooError::EPOCH_MISMATCH);
    Ok(())
}

#[tokio::test]
async fn graph_page_pagination() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let viewport = bamboo_protocol::GraphViewport {
        offset: 1,
        limit: 1,
        anchor_commit: None,
        epoch: 0, // 0 = skip check
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
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

    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let has_untracked = items
        .iter()
        .any(|i| i.path == "d.txt" && i.status == bamboo_protocol::StatusCode::Untracked);
    let has_modified = items
        .iter()
        .any(|i| i.path == "a.txt" && i.status == bamboo_protocol::StatusCode::Modified);
    assert!(has_untracked, "expected untracked d.txt, got {:?}", items);
    assert!(has_modified, "expected modified a.txt, got {:?}", items);

    // Path filter.
    let filtered = bamboo_engine::gix_read::read_status(&session, Some(&["d.txt".into()]))?;
    assert!(filtered.iter().all(|i| i.path.starts_with("d.txt")));
    assert!(!filtered.is_empty());
    Ok(())
}

#[tokio::test]
async fn refs_read_includes_branches() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let refs = bamboo_engine::gix_read::read_refs(&session)?;
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
    let undo2 = bamboo_engine::undo::UndoEngine::new(dir.path(), &dir.path().join(".git"))?;
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;

    // Epoch must have bumped (the write queue bump plus possible fs
    // watcher bumps from the git add writing .git/index).
    assert!(session.current_epoch() >= 2);

    settle().await;
    // The file should now appear as staged (tree-index change).
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let staged = items
        .iter()
        .any(|i| i.path == "new.txt" && i.status == bamboo_protocol::StatusCode::Added && i.staged);
    assert!(staged, "expected staged new.txt, got {:?}", items);
    // Worktree changes must be flagged as unstaged.
    let unstaged = items.iter().any(|i| i.path == "a.txt" && !i.staged);
    assert!(unstaged, "expected unstaged a.txt, got {:?}", items);

    // An undo snapshot and audit entry should exist.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let audit = std::fs::read_to_string(dir.path().join(".git").join("bamboo").join("audit.log"))?;
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "fourth".to_string(),
            amend: false,
        })
        .await?;

    settle().await;
    // Graph should now contain 4 commits.
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    let entries = session.undo.list_transactions(10)?;
    let stage_tx = entries[0].id;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "fourth".to_string(),
            amend: false,
        })
        .await?;

    settle().await;
    // 4 commits now.
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 4);

    // Undo the commit (the latest transaction).
    let entries = session.undo.list_transactions(10)?;
    let commit_tx = entries[0].id;
    let outcome = session.undo.undo(commit_tx)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));

    settle().await;
    // Graph back to 3 commits, main restored to "third".
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 3);
    assert!(page.commits[0].message.contains("third"));

    // The stage transaction is still on the stack; undoing it (a failed-less
    // no-op for refs) pops it.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, stage_tx);
    let outcome = session.undo.undo(stage_tx)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
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
        matches!(result, Err(bamboo_engine::undo::UndoError::Blocked { .. })),
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
    bamboo_engine::server::read_message(&mut reader)
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
    assert!(read_message(b"Content-Length: 5\r\n".to_vec())
        .unwrap()
        .is_none());
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
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    let anchor = page.commits[1].id.clone(); // "second"

    // Requested offset 0, but the anchor pins the page at its index.
    let vp = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 2,
        anchor_commit: Some(anchor.clone()),
        epoch: 0,
    };
    let anchored = bamboo_engine::gix_read::read_graph_page(&session, &vp)?;
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

    let anchored_again = bamboo_engine::gix_read::read_graph_page(&session, &vp)?;
    assert_eq!(anchored_again.commits[0].message, "second");

    // Unknown anchor: the requested offset is kept.
    let vp = bamboo_protocol::GraphViewport {
        offset: 1,
        limit: 1,
        anchor_commit: Some("0000000000000000000000000000000000000000".to_string()),
        epoch: 0,
    };
    let fallback = bamboo_engine::gix_read::read_graph_page(&session, &vp)?;
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

    let items = bamboo_engine::gix_read::read_status(&session, Some(&["dir".to_string()]))?;
    // "dir" matches the untracked directory entry (gix may collapse it to
    // "dir" or report "dir/x.txt"), but NOT the sibling "dirx.txt".
    assert!(!items.is_empty());
    assert!(items
        .iter()
        .all(|i| i.path == "dir" || i.path.starts_with("dir/")));

    // Exact file matches still work (use a non-directory sibling).
    let items = bamboo_engine::gix_read::read_status(&session, Some(&["dirx.txt".to_string()]))?;
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i.path == "dirx.txt"));

    // A prefix that is not a component boundary must not match.
    let items = bamboo_engine::gix_read::read_status(&session, Some(&["dirx".to_string()]))?;
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::SwitchBranch {
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
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "secret.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "SECRET MESSAGE".to_string(),
            amend: false,
        })
        .await?;

    let git_wb = dir.path().join(".git").join("bamboo");
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

    let content = bamboo_engine::gix_read::read_blob(&session, "HEAD", "a.txt")?;
    assert_eq!(content.trim(), "a");

    // Missing path resolves to empty content, not an error.
    let missing = bamboo_engine::gix_read::read_blob(&session, "HEAD", "no/such/file.txt")?;
    assert_eq!(missing, "");

    // Option-like revisions are rejected defensively (no shell involved).
    assert!(bamboo_engine::gix_read::read_blob(&session, "--upload-pack=x", "a.txt").is_err());
    Ok(())
}

// ---------------------------------------------------------------------------
// Discard (worktree-only, irreversible, no undo transaction)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discard_tracked_restores_from_index_staged_content_survives() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Stage a new version of a.txt, then dirty the worktree further:
    // index = "staged", worktree = "dirty", HEAD = "a".
    std::fs::write(dir.path().join("a.txt"), "staged\n")?;
    let out = Command::new("git")
        .args(["add", "--", "a.txt"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(dir.path().join("a.txt"), "worktree-dirty\n")?;

    let epoch_before = session.current_epoch();
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["a.txt".to_string()],
        })
        .await?;

    // Restored from the INDEX, not from HEAD: staged content survives.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt"))?,
        "staged\n"
    );
    // Epoch bumped by the write queue (plus any watcher bumps).
    assert!(session.current_epoch() > epoch_before);

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    // Only the staged (index vs HEAD) change remains; no worktree change.
    assert!(
        items.iter().any(|i| i.path == "a.txt"
            && i.staged
            && i.status == bamboo_protocol::StatusCode::Modified),
        "expected staged-only a.txt, got {:?}",
        items
    );
    assert!(
        items.iter().all(|i| !(i.path == "a.txt" && !i.staged)),
        "expected no worktree change for a.txt, got {:?}",
        items
    );

    // Discard deliberately creates no undo transaction...
    assert!(session.undo.list_transactions(10)?.is_empty());
    // ...but is still audit-logged (redacted: no paths).
    let audit = std::fs::read_to_string(dir.path().join(".git").join("bamboo").join("audit.log"))?;
    assert!(audit.contains("\"operation\":\"Discard changes\""));
    assert!(!audit.contains("a.txt"));
    Ok(())
}

#[tokio::test]
async fn discard_removes_untracked_files() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("untracked.txt"), "u\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["untracked.txt".to_string()],
        })
        .await?;

    // Untracked-only paths are deleted, not restored.
    assert!(!dir.path().join("untracked.txt").exists());

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items.iter().all(|i| i.path != "untracked.txt"),
        "expected untracked.txt gone, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn discard_mixed_paths_in_single_call() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("a.txt"), "dirty\n")?; // tracked modification
    std::fs::write(dir.path().join("new.txt"), "untracked\n")?; // untracked

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["a.txt".to_string(), "new.txt".to_string()],
        })
        .await?;

    // Tracked restored (index == HEAD here → original content), untracked gone.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "a\n");
    assert!(!dir.path().join("new.txt").exists());

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items
            .iter()
            .all(|i| i.path != "a.txt" && i.path != "new.txt"),
        "expected both paths clean, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn discard_rejects_conflicted_files() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Build a real conflict: divergent branches both touching a.txt.
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    };
    git(&["checkout", "-q", "-b", "side"])?;
    std::fs::write(dir.path().join("a.txt"), "side\n")?;
    git(&["add", "--", "a.txt"])?;
    git(&["commit", "-q", "-m", "side"])?;
    git(&["checkout", "-q", "main"])?;
    std::fs::write(dir.path().join("a.txt"), "main\n")?;
    git(&["add", "--", "a.txt"])?;
    git(&["commit", "-q", "-m", "main"])?;
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(dir.path())
        .output()?;
    assert!(!merge.status.success(), "expected merge to conflict");

    // Discard of a conflicted path is refused, with the file named.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["a.txt".to_string()],
        })
        .await
        .expect_err("discard of a conflicted file must fail");
    let msg = format!("{err}");
    assert!(msg.contains("a.txt"), "error should name the file: {msg}");
    assert!(
        msg.to_lowercase().contains("conflict"),
        "error should mention conflicts: {msg}"
    );

    // The conflicted worktree content is untouched (still has markers).
    assert!(std::fs::read_to_string(dir.path().join("a.txt"))?.contains("<<<<<<<"));
    Ok(())
}

#[tokio::test]
async fn discard_removes_collapsed_untracked_directory() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // The Commit view lists a collapsed untracked directory by its name
    // ("newdir"), while `git status --porcelain` reports it as "newdir/".
    std::fs::create_dir_all(dir.path().join("newdir"))?;
    std::fs::write(dir.path().join("newdir").join("x.txt"), "x\n")?;

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["newdir".to_string()],
        })
        .await?;

    // The whole collapsed directory is removed (not a silent no-op).
    assert!(!dir.path().join("newdir").exists());

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items.iter().all(|i| !i.path.starts_with("newdir")),
        "expected newdir gone, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn discard_deletes_intent_to_add_file() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // `git add -N`: the index holds an empty-blob placeholder, so the file
    // must be DELETED (index entry + worktree), not restored from the
    // index (which would truncate it to zero bytes).
    std::fs::write(dir.path().join("ita.txt"), "content\n")?;
    let out = Command::new("git")
        .args(["add", "-N", "--", "ita.txt"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["ita.txt".to_string()],
        })
        .await?;

    assert!(!dir.path().join("ita.txt").exists());

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items.iter().all(|i| i.path != "ita.txt"),
        "expected ita.txt fully gone (index and worktree), got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn discard_treats_glob_named_paths_literally() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // A file literally named with a glob metacharacter must be discarded
    // as that exact file — never re-interpreted as a glob matching the
    // sibling. Brackets are used instead of `*`/`?` because those are
    // reserved characters in Windows filenames (os error 123) and this
    // test must run on the Windows CI matrix; `g[1].txt` as a glob would
    // still match the sibling `g1.txt`, so the literal-only guarantee is
    // exercised just the same.
    std::fs::write(dir.path().join("g[1].txt"), "bracket\n")?;
    std::fs::write(dir.path().join("g1.txt"), "one\n")?;

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["g[1].txt".to_string()],
        })
        .await?;

    assert!(!dir.path().join("g[1].txt").exists());
    assert!(dir.path().join("g1.txt").exists(), "sibling must survive");
    Ok(())
}

#[tokio::test]
async fn discard_rejects_unchanged_and_directory_shaped_paths() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // A path with no reported change is an explicit error — a destructive
    // RPC must never silently no-op.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["a.txt".to_string()], // clean, no change to discard
        })
        .await
        .expect_err("discarding an unchanged path must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("no changes to discard"),
        "error should say what happened: {msg}"
    );
    assert!(msg.contains("a.txt"), "error should name the path: {msg}");
    // The clean file is untouched.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "a\n");

    // Directory-shaped pathspecs (trailing slash) are rejected: they could
    // expand to every file under the directory, beyond what was confirmed.
    std::fs::create_dir_all(dir.path().join("somedir"))?;
    std::fs::write(dir.path().join("somedir").join("f.txt"), "f\n")?;
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["somedir/".to_string()],
        })
        .await
        .expect_err("directory-shaped discard path must fail");
    assert!(
        format!("{err}").contains("not directories"),
        "error should reject the directory shape: {err}"
    );
    assert!(dir.path().join("somedir").join("f.txt").exists());
    Ok(())
}

#[tokio::test]
async fn discard_after_tx_does_not_block_prior_undo() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // 1. Create a real undoable transaction: stage a new file.
    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let stage_tx = entries[0].id;

    // 2. Discard an unrelated worktree change.
    std::fs::write(dir.path().join("a.txt"), "dirty\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Discard {
            paths: vec!["a.txt".to_string()],
        })
        .await?;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "a\n");

    // Still exactly one transaction (discard adds none).
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, stage_tx);

    // 3. The pre-discard transaction is still undoable: the snapshot is
    //    unaffected by worktree-only changes.
    let outcome = session.undo.undo(stage_tx)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));

    settle().await;
    // The stage was rolled back: new.txt is untracked again.
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let untracked = items.iter().any(|i| {
        i.path == "new.txt" && i.status == bamboo_protocol::StatusCode::Untracked && !i.staged
    });
    assert!(
        untracked,
        "expected untracked new.txt after undo, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn head_read_reports_branch_and_commit() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let head = bamboo_engine::gix_read::read_head(&session)?;
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!(
        head.head.len(),
        40,
        "expected a full sha, got {:?}",
        head.head
    );

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
    let detached = bamboo_engine::gix_read::read_head(&session)?;
    assert_eq!(detached.branch, None);
    assert_eq!(detached.head, sha);
    Ok(())
}

// ---------------------------------------------------------------------------
// Commit detail (read-only RPC for the commit detail view)
// ---------------------------------------------------------------------------

/// Build a commit and return its sha.
fn gc(dir: &tempfile::TempDir, msg: &str) -> String {
    let out = Command::new("git")
        .arg("commit")
        .arg("-q")
        .arg("--allow-empty")
        .arg("-m")
        .arg(msg)
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_DATE", "2026-02-01T00:00:00Z")
        .output()
        .expect("git commit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(
        &Command::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .current_dir(dir.path())
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .trim()
    .to_string()
}

#[tokio::test]
async fn commit_detail_multiline_message_and_committer() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let sha = gc(
        &dir,
        "subject line\n\nbody paragraph one\nbody paragraph two",
    );
    let detail = bamboo_engine::gix_read::read_commit_detail(&session, &sha)?;

    assert_eq!(detail.id, sha);
    // Full multi-line message preserved verbatim, newline included.
    assert_eq!(
        detail.message,
        "subject line\n\nbody paragraph one\nbody paragraph two\n"
    );
    assert_eq!(detail.author_name, "Test");
    assert_eq!(detail.author_email, "test@example.com");
    assert_eq!(detail.committer_name, "Committer");
    assert_eq!(detail.committer_email, "committer@example.com");
    assert_eq!(detail.committer_time, 1769904000); // 2026-02-01T00:00:00Z
                                                   // Not a root commit: at least one parent.
    assert!(!detail.parent_ids.is_empty());
    Ok(())
}

#[tokio::test]
async fn commit_detail_root_commit_uses_empty_tree_baseline() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    let root = String::from_utf8_lossy(
        &Command::new("git")
            .arg("rev-list")
            .arg("--max-parents=0")
            .arg("HEAD")
            .current_dir(dir.path())
            .output()?
            .stdout,
    )
    .trim()
    .to_string();

    let detail = bamboo_engine::gix_read::read_commit_detail(&session, &root)?;
    assert!(detail.parent_ids.is_empty());
    // The root commit of test_repo() adds a.txt, b.txt, c.txt (via three
    // sequential commits only the FIRST adds a.txt; root adds just a.txt).
    let paths: Vec<&str> = detail.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["a.txt"]);
    assert_eq!(detail.files[0].status, "A");
    assert_eq!(detail.files[0].additions, 1); // "a\n" = one line
    Ok(())
}

#[tokio::test]
async fn commit_detail_merge_commit_first_parent_diff() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // feature branched after "second" (a.txt, b.txt). Add a file there,
    // then merge it into main (which by then has "third" => c.txt).
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["checkout", "-q", "feature"]);
    std::fs::write(dir.path().join("feat.txt"), "feature\n")?;
    git(&["add", "."]);
    let feature_sha = gc(&dir, "feature work");
    git(&["checkout", "-q", "main"]);
    let merge_sha = {
        let out = Command::new("git")
            .arg("merge")
            .arg("-q")
            .arg("--no-ff")
            .arg("-m")
            .arg("merge feature")
            .arg("feature")
            .current_dir(dir.path())
            .output()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(
            &Command::new("git")
                .arg("rev-parse")
                .arg("HEAD")
                .current_dir(dir.path())
                .output()?
                .stdout,
        )
        .trim()
        .to_string()
    };
    assert_eq!(feature_sha.len(), 40);

    let detail = bamboo_engine::gix_read::read_commit_detail(&session, &merge_sha)?;
    assert_eq!(detail.parent_ids.len(), 2, "merge commit has two parents");
    // Diff is against the FIRST parent (main side): the merge brings in
    // feat.txt as an addition; c.txt (added on main before the merge) is
    // unchanged vs. the first parent and must not appear.
    let paths: Vec<&str> = detail.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["feat.txt"]);
    assert_eq!(detail.files[0].status, "A");
    Ok(())
}

#[tokio::test]
async fn commit_detail_rename_and_modify_counts() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Pure rename: R status with old_path.
    git_rename_helper(&dir)?;
    let sha = String::from_utf8_lossy(
        &Command::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .current_dir(dir.path())
            .output()?
            .stdout,
    )
    .trim()
    .to_string();

    let detail = bamboo_engine::gix_read::read_commit_detail(&session, &sha)?;
    assert_eq!(detail.files.len(), 1);
    let f = &detail.files[0];
    assert_eq!(f.status, "R");
    assert_eq!(f.old_path.as_deref(), Some("a.txt"));
    assert_eq!(f.path, "b-renamed.txt");
    assert_eq!(f.additions, 0);
    assert_eq!(f.deletions, 0);
    Ok(())
}

fn git_rename_helper(dir: &tempfile::TempDir) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("mv")
        .arg("a.txt")
        .arg("b-renamed.txt")
        .current_dir(dir.path())
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new("git")
        .args(["commit", "-q", "-m", "rename a.txt"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn commit_detail_rejects_invalid_and_unknown_sha() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Malformed ids (wrong length, uppercase, option-like) are rejected.
    for bad in ["short", "1234", &"A".repeat(40), "--upload-pack=x", "HEAD"] {
        assert!(
            bamboo_engine::gix_read::read_commit_detail(&session, bad).is_err(),
            "expected error for {bad:?}"
        );
    }

    // Well-formed but unknown sha.
    let unknown = "0".repeat(40);
    assert!(bamboo_engine::gix_read::read_commit_detail(&session, &unknown).is_err());
    Ok(())
}

// ---------------------------------------------------------------------------
// Network ops: push/pull/fetch, remotes, branch status, stash RPCs
// ---------------------------------------------------------------------------

use bamboo_protocol::{BambooError, ForceMode, PullStrategy};
use bamboo_engine::write_queue::WriteCommand;

/// Run git in `dir`, failing the test with stderr on a non-zero exit.
/// Author/committer envs are pinned so helper commits never depend on the
/// machine's global git config.
fn gitq(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// A work repo (test_repo history) wired to a bare `origin` with the
/// initial main pushed (upstream deliberately NOT configured, so tests
/// can exercise `set_upstream` themselves).
fn test_repo_with_origin() -> anyhow::Result<(tempfile::TempDir, tempfile::TempDir)> {
    let work = test_repo()?;
    let origin = tempfile::tempdir()?;
    gitq(origin.path(), &["init", "--bare", "-q", "-b", "main"])?;
    gitq(
        work.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    )?;
    gitq(work.path(), &["push", "-q", "origin", "main"])?;
    Ok((work, origin))
}

/// Advance origin/main by one commit made in a throwaway clone — WITHOUT
/// touching the work repo, so its remote-tracking refs go stale (the
/// precondition for push-rejection and force-with-lease tests).
fn advance_origin(
    origin: &tempfile::TempDir,
    msg: &str,
    filename: &str,
) -> anyhow::Result<String> {
    let clone = tempfile::tempdir()?;
    gitq(
        clone.path(),
        &["clone", "-q", origin.path().to_str().unwrap(), "."],
    )?;
    std::fs::write(clone.path().join(filename), format!("{msg}\n"))?;
    gitq(clone.path(), &["add", "."])?;
    gitq(clone.path(), &["commit", "-q", "-m", msg])?;
    gitq(clone.path(), &["push", "-q", "origin", "HEAD:refs/heads/main"])?;
    Ok(gitq(clone.path(), &["rev-parse", "HEAD"])?.trim().to_string())
}

fn push_cmd(remote: &str, branch: &str, force: ForceMode, set_upstream: bool) -> WriteCommand {
    WriteCommand::Push {
        remote: remote.to_string(),
        branch: branch.to_string(),
        force,
        set_upstream,
    }
}

#[tokio::test]
async fn push_force_none_rejected_then_force_succeeds() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Remote moves ahead without our knowledge; we also have a local
    // commit, so a plain push is non-fast-forward.
    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    // force: none → structured PUSH_REJECTED carrying the remote branch.
    let err = session
        .enqueue_write(push_cmd("origin", "main", ForceMode::None, false))
        .await
        .expect_err("plain non-ff push must be rejected");
    assert_eq!(err.code, BambooError::PUSH_REJECTED);
    assert!(
        err.message.contains("origin/main"),
        "error should carry the remote branch: {}",
        err.message
    );

    // force: force (--force) → overwrite; push updates tracking refs.
    let local_sha = gitq(work.path(), &["rev-parse", "HEAD"])?.trim().to_string();
    session
        .enqueue_write(push_cmd("origin", "main", ForceMode::Force, false))
        .await?;
    assert_eq!(
        gitq(work.path(), &["rev-parse", "origin/main"])?.trim(),
        local_sha
    );
    assert_ne!(local_sha, remote_sha);
    Ok(())
}

#[tokio::test]
async fn push_force_with_lease_semantics() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Remote advances while our tracking ref is stale → the lease must
    // refuse (stale info), even though a plain --force would succeed.
    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    let err = session
        .enqueue_write(push_cmd("origin", "main", ForceMode::Lease, false))
        .await
        .expect_err("lease push with stale tracking must be rejected");
    assert_eq!(err.code, BambooError::PUSH_REJECTED);

    // After a fetch the tracking ref matches the remote, so the lease is
    // satisfied and the still-non-fast-forward push goes through —
    // proving the flag is neither absent (that was rejected above) nor
    // an unconditional --force.
    session
        .enqueue_write(WriteCommand::Fetch {
            remote: Some("origin".to_string()),
        })
        .await?;
    let local_sha = gitq(work.path(), &["rev-parse", "HEAD"])?.trim().to_string();
    session
        .enqueue_write(push_cmd("origin", "main", ForceMode::Lease, false))
        .await?;
    assert_eq!(
        gitq(work.path(), &["rev-parse", "origin/main"])?.trim(),
        local_sha
    );
    assert_ne!(local_sha, remote_sha);
    Ok(())
}

#[tokio::test]
async fn push_set_upstream_and_returns_real_output() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // No upstream configured yet (setup deliberately skipped -u).
    assert!(gitq(work.path(), &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_err());

    // The push response is the real command output (not a null), ...
    let output = session
        .enqueue_write(push_cmd("origin", "main", ForceMode::None, true))
        .await?;
    assert!(!output.is_empty(), "push output should be non-empty");

    // ...and -u wired up the tracking branch.
    assert_eq!(
        gitq(work.path(), &["rev-parse", "--abbrev-ref", "@{upstream}"])?.trim(),
        "origin/main"
    );
    Ok(())
}

#[tokio::test]
async fn push_injection_rejected() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Option-like remotes/branch specs never reach git as arguments.
    let err = session
        .enqueue_write(push_cmd("--upload-pack=x", "main", ForceMode::None, false))
        .await
        .expect_err("option-like remote must be rejected");
    assert_eq!(err.code, BambooError::GIT_ERROR);
    assert!(
        format!("{err}").contains("invalid ref name")
            || format!("{err}").contains("remote"),
        "unexpected error: {err}"
    );
    let err = session
        .enqueue_write(push_cmd("origin", "-o=evil", ForceMode::None, false))
        .await
        .expect_err("option-like branch must be rejected");
    assert!(
        format!("{err}").contains("invalid push refspec"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn pull_ff_only_fast_forwards() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;
    session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::FfOnly,
            auto_stash: false,
        })
        .await?;

    assert_eq!(
        gitq(work.path(), &["rev-parse", "HEAD"])?.trim(),
        remote_sha
    );
    // A fast-forward leaves no merge commit.
    assert!(gitq(work.path(), &["log", "--merges", "--oneline"])?
        .trim()
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn pull_merge_strategy_creates_merge_commit() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::Merge,
            auto_stash: false,
        })
        .await?;

    // --no-rebase: a true merge commit with two parents.
    assert!(gitq(work.path(), &["rev-parse", "--verify", "HEAD^2"]).is_ok());
    assert!(!gitq(work.path(), &["log", "--merges", "--oneline"])?
        .trim()
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn pull_rebase_strategy_keeps_history_linear() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::Rebase,
            auto_stash: false,
        })
        .await?;

    // The local commit was replayed on top of the remote head: HEAD's
    // parent IS the remote commit and no merge commit exists.
    assert_eq!(
        gitq(work.path(), &["rev-parse", "HEAD^"])?.trim(),
        remote_sha
    );
    assert_ne!(gitq(work.path(), &["rev-parse", "HEAD"])?.trim(), remote_sha);
    assert!(gitq(work.path(), &["log", "--merges", "--oneline"])?
        .trim()
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn pull_default_appends_no_flag_and_honors_git_config() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // The user's pull.rebase config decides the strategy: `Default` must
    // append no flag, so the pull rebases (a --no-rebase or --ff-only flag
    // would instead produce a merge commit or fail).
    gitq(work.path(), &["config", "pull.rebase", "true"])?;
    advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::Default,
            auto_stash: false,
        })
        .await?;

    assert!(gitq(work.path(), &["log", "--merges", "--oneline"])?
        .trim()
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn pull_auto_stash_carries_dirty_worktree() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("a.txt"), "dirty\n")?;

    let output = session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::Default,
            auto_stash: true,
        })
        .await?;
    assert!(output.contains("stashed"), "unexpected output: {output}");

    // The pull fast-forwarded AND the dirty change came back.
    assert_eq!(
        gitq(work.path(), &["rev-parse", "HEAD"])?.trim(),
        remote_sha
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join("a.txt"))?,
        "dirty\n"
    );
    // Nothing left on the stash stack.
    assert!(gitq(work.path(), &["stash", "list"])?.trim().is_empty());
    Ok(())
}

#[tokio::test]
async fn fetch_none_fetches_all_remotes_and_bypasses_undo_journal() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // A second remote (bare clone of the work repo, distinct history tip
    // via the origin advance below).
    let other = tempfile::tempdir()?;
    gitq(
        work.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
    )?;
    gitq(
        work.path(),
        &["remote", "add", "other", other.path().to_str().unwrap()],
    )?;
    let remote_sha = advance_origin(&origin, "remote work", "remote.txt")?;

    // remote: None → `git fetch --all`: every remote is refreshed.
    session
        .enqueue_write(WriteCommand::Fetch { remote: None })
        .await?;
    assert_eq!(
        gitq(work.path(), &["rev-parse", "origin/main"])?.trim(),
        remote_sha
    );
    // The second remote was fetched too (a single-remote fetch would
    // never create refs/remotes/other/*).
    assert!(gitq(work.path(), &["rev-parse", "--verify", "refs/remotes/other/main"]).is_ok());

    // Fetch deliberately creates NO undo transaction (bypass, see
    // run_write_queue): the journal is still empty after the fetch.
    assert!(session.undo.list_transactions(10)?.is_empty());
    // ...but it IS audit-logged.
    let audit = std::fs::read_to_string(work.path().join(".git").join("bamboo").join("audit.log"))?;
    assert!(audit.contains("\"operation\":\"Fetch\""));
    Ok(())
}

#[tokio::test]
async fn get_remotes_reports_names_urls_and_default() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    let other = tempfile::tempdir()?;
    gitq(
        work.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
    )?;
    gitq(
        work.path(),
        &["remote", "add", "other", other.path().to_str().unwrap()],
    )?;

    let remotes = bamboo_engine::gix_read::read_remotes(&session)?;
    assert_eq!(remotes.len(), 2);
    let origin = remotes.iter().find(|r| r.name == "origin").unwrap();
    assert!(origin.url.is_some());
    // No remote.default config → the first remote in list order is the
    // default ("git remote" sorts alphabetically: origin < other).
    assert!(origin.default);
    let other = remotes.iter().find(|r| r.name == "other").unwrap();
    assert!(other.url.is_some());
    assert!(!other.default);

    // A remote.default config moves the flag.
    gitq(work.path(), &["config", "remote.default", "other"])?;
    let remotes = bamboo_engine::gix_read::read_remotes(&session)?;
    assert!(remotes.iter().find(|r| r.name == "other").unwrap().default);
    assert!(!remotes.iter().find(|r| r.name == "origin").unwrap().default);
    Ok(())
}

#[tokio::test]
async fn branch_status_upstream_ahead_behind() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // No upstream yet (setup pushes without -u).
    let st = bamboo_engine::gix_read::read_branch_status(&session, None)?;
    assert_eq!(st.branch, "main");
    assert_eq!(st.upstream, None);
    assert_eq!((st.ahead, st.behind), (0, 0));

    // Push with -u through the engine wires the upstream up.
    session
        .enqueue_write(push_cmd("origin", "main", ForceMode::None, true))
        .await?;
    let st = bamboo_engine::gix_read::read_branch_status(&session, None)?;
    assert_eq!(st.upstream.as_deref(), Some("origin/main"));
    assert_eq!((st.ahead, st.behind), (0, 0));

    // A local commit → ahead 1.
    gitq(work.path(), &["commit", "-q", "--allow-empty", "-m", "local ahead"])?;
    let st = bamboo_engine::gix_read::read_branch_status(&session, None)?;
    assert_eq!((st.ahead, st.behind), (1, 0));

    // Remote advances + engine fetch → behind 1 as well.
    advance_origin(&origin, "remote work", "remote.txt")?;
    session
        .enqueue_write(WriteCommand::Fetch {
            remote: Some("origin".to_string()),
        })
        .await?;
    let st = bamboo_engine::gix_read::read_branch_status(&session, None)?;
    assert_eq!((st.ahead, st.behind), (1, 1));

    // An explicitly-named branch without upstream reports null.
    let st = bamboo_engine::gix_read::read_branch_status(&session, Some("feature"))?;
    assert_eq!(st.branch, "feature");
    assert_eq!(st.upstream, None);
    assert_eq!((st.ahead, st.behind), (0, 0));

    // Detached HEAD on the default (current) branch is INVALID_ARGUMENT
    // so clients can disable the affected buttons.
    let sha = gitq(work.path(), &["rev-parse", "HEAD"])?.trim().to_string();
    gitq(work.path(), &["checkout", "-q", &sha])?;
    let err = bamboo_engine::gix_read::read_branch_status(&session, None)
        .expect_err("detached HEAD must be an error");
    assert_eq!(
        err.downcast_ref::<BambooError>().unwrap().code,
        BambooError::INVALID_ARGUMENT
    );
    Ok(())
}

#[tokio::test]
async fn stash_push_list_pop_roundtrip() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("a.txt"), "stashed\n")?;
    std::fs::write(dir.path().join("u.txt"), "untracked\n")?;

    let out = session
        .enqueue_write(WriteCommand::StashPush {
            message: Some("my message".to_string()),
            include_untracked: true,
            staged_only: false,
        })
        .await?;
    assert!(out.contains("Saved"), "unexpected stash output: {out}");

    // The worktree is clean again (tracked restored, untracked gone).
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "a\n");
    assert!(!dir.path().join("u.txt").exists());

    // stashList exposes the entry with ref/message/date.
    let entries = bamboo_engine::gix_read::read_stash_list(&session)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].stash_ref, "stash@{0}");
    assert!(
        entries[0].message.contains("my message"),
        "unexpected subject: {:?}",
        entries[0].message
    );
    assert!(!entries[0].date.is_empty());

    // Pop restores everything (untracked included) and drops the entry.
    session
        .enqueue_write(WriteCommand::StashPop {
            stash_ref: None,
            apply: false,
            reinstate_index: false,
        })
        .await?;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt"))?,
        "stashed\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt"))?,
        "untracked\n"
    );
    assert!(bamboo_engine::gix_read::read_stash_list(&session)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn stash_apply_reinstates_index_split() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // index = "v2", worktree = "v3": the stash records BOTH states.
    std::fs::write(dir.path().join("a.txt"), "v2\n")?;
    gitq(dir.path(), &["add", "--", "a.txt"])?;
    std::fs::write(dir.path().join("a.txt"), "v3\n")?;
    session
        .enqueue_write(WriteCommand::StashPush {
            message: None,
            include_untracked: false,
            staged_only: false,
        })
        .await?;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "a\n");

    // apply --index restores the staged/worktree split as it was.
    session
        .enqueue_write(WriteCommand::StashPop {
            stash_ref: None,
            apply: true,
            reinstate_index: true,
        })
        .await?;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "v3\n");

    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items.iter().any(|i| i.path == "a.txt" && i.staged
            && i.status == bamboo_protocol::StatusCode::Modified),
        "expected staged a.txt (index = v2), got {:?}",
        items
    );
    assert!(
        items.iter().any(|i| i.path == "a.txt" && !i.staged),
        "expected unstaged a.txt (worktree = v3), got {:?}",
        items
    );
    // apply (unlike pop) keeps the entry on the stack.
    assert_eq!(bamboo_engine::gix_read::read_stash_list(&session)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn stash_drop_removes_entry() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("a.txt"), "to be stashed\n")?;
    session
        .enqueue_write(WriteCommand::StashPush {
            message: None,
            include_untracked: false,
            staged_only: false,
        })
        .await?;
    assert_eq!(bamboo_engine::gix_read::read_stash_list(&session)?.len(), 1);

    let out = session
        .enqueue_write(WriteCommand::StashDrop {
            stash_ref: Some("stash@{0}".to_string()),
        })
        .await?;
    assert!(out.contains("Dropped"), "unexpected drop output: {out}");
    assert!(bamboo_engine::gix_read::read_stash_list(&session)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn stash_pop_conflict_returns_structured_code_and_retains_entry() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Stash a change to a.txt, then stage a conflicting change to the
    // same file, so the pop cannot apply cleanly.
    std::fs::write(dir.path().join("a.txt"), "stash side\n")?;
    session
        .enqueue_write(WriteCommand::StashPush {
            message: None,
            include_untracked: false,
            staged_only: false,
        })
        .await?;
    std::fs::write(dir.path().join("a.txt"), "worktree side\n")?;
    gitq(dir.path(), &["add", "--", "a.txt"])?;

    let err = session
        .enqueue_write(WriteCommand::StashPop {
            stash_ref: None,
            apply: false,
            reinstate_index: false,
        })
        .await
        .expect_err("conflicting stash pop must fail");
    assert_eq!(err.code, BambooError::STASH_CONFLICT);
    assert!(
        err.message.contains("retained"),
        "error must state the stash was retained: {}",
        err.message
    );

    // git did NOT drop the stash entry.
    assert_eq!(bamboo_engine::gix_read::read_stash_list(&session)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn stash_validates_ref_and_message() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    // Stash refs are limited to the strict stash@{N} form (and a
    // bounded number of digits — a multi-megabyte digit string must not
    // flow into the queue/argv).
    for bad in [
        "--evil",
        "stash@{x}",
        "stash@{}",
        "HEAD~1",
        "stash@{0} extra",
        "stash@{1234567890}",
    ] {
        let err = match session
            .enqueue_write(WriteCommand::StashPop {
                stash_ref: Some(bad.to_string()),
                apply: false,
                reinstate_index: false,
            })
            .await
        {
            Ok(output) => panic!("stash ref {bad:?} must be rejected, got success: {output}"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").contains("stash@{N}"),
            "bad ref {bad:?} should be rejected with form error: {err}"
        );
    }

    // Messages: no control characters, bounded length.
    let err = session
        .enqueue_write(WriteCommand::StashPush {
            message: Some("line\nbreak".to_string()),
            include_untracked: false,
            staged_only: false,
        })
        .await
        .expect_err("control chars must be rejected");
    assert!(format!("{err}").contains("control characters"));

    let err = session
        .enqueue_write(WriteCommand::StashPush {
            message: Some("x".repeat(501)),
            include_untracked: false,
            staged_only: false,
        })
        .await
        .expect_err("overlong message must be rejected");
    assert!(format!("{err}").contains("exceeds"));

    // Nothing was stashed along the way.
    assert!(bamboo_engine::gix_read::read_stash_list(&session)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn legacy_bool_force_parses_as_lease_at_the_engine_boundary() -> anyhow::Result<()> {
    // The wire-level roundtrip (bool true → lease) lives in the protocol
    // tests; this pins the engine-side consequence: a legacy `force: true`
    // client gets lease semantics (rejected on stale tracking, not an
    // unconditional overwrite).
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;

    // force: true (legacy bool) deserializes to Lease.
    let req: bamboo_protocol::Push = serde_json::from_value(serde_json::json!({
        "remote": "origin",
        "branch": "main",
        "force": true
    }))?;
    assert_eq!(req.force, ForceMode::Lease);

    let err = session
        .enqueue_write(push_cmd("origin", "main", req.force, false))
        .await
        .expect_err("legacy-bool force (lease) with stale tracking must be rejected");
    assert_eq!(err.code, BambooError::PUSH_REJECTED);
    // The remote was NOT overwritten (that would be --force semantics).
    assert_ne!(
        gitq(work.path(), &["rev-parse", "origin/main"])?.trim(),
        gitq(work.path(), &["rev-parse", "HEAD"])?.trim()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Regression tests: review fixes (queue-blocking safeguards, refspec
// deletion, pull semantics, credential redaction, stash/branch-status
// validation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_rejects_remote_branch_deletion_refspecs() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // `:dst` deletes the remote branch and `src:` is a no-op — both would
    // bypass the ForceMode confirmation tiers entirely (`git push origin
    // :main` destroys the remote branch with force: "none"), so they are
    // rejected before git ever runs.
    for spec in [":refs/heads/main", "refs/heads/main:"] {
        let err = session
            .enqueue_write(push_cmd("origin", spec, ForceMode::None, false))
            .await
            .expect_err("deletion-shaped refspec must be rejected");
        assert!(
            format!("{err}").contains("invalid push refspec"),
            "unexpected error: {err}"
        );
    }

    // The remote branch was untouched.
    assert!(
        gitq(origin.path(), &["rev-parse", "--verify", "refs/heads/main"]).is_ok(),
        "remote branch must survive"
    );
    Ok(())
}

#[tokio::test]
async fn pull_branch_without_remote_is_invalid_argument() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // `git pull` would silently ignore the branch and pull the current
    // branch's upstream instead — the RPC must fail rather than succeed
    // against the wrong thing.
    let err = session
        .enqueue_write(WriteCommand::Pull {
            remote: None,
            branch: Some("main".to_string()),
            strategy: PullStrategy::Default,
            auto_stash: false,
        })
        .await
        .expect_err("branch without remote must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    Ok(())
}

#[tokio::test]
async fn pull_ff_only_rejection_restores_auto_stash() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Divergent histories: --ff-only must refuse to merge.
    advance_origin(&origin, "remote work", "remote.txt")?;
    std::fs::write(work.path().join("local.txt"), "local\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local work"])?;
    let pre_pull_head = gitq(work.path(), &["rev-parse", "HEAD"])?.trim().to_string();

    // Dirty worktree gets auto-stashed before the pull.
    std::fs::write(work.path().join("a.txt"), "dirty\n")?;

    let err = session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::FfOnly,
            auto_stash: true,
        })
        .await
        .expect_err("ff-only pull on divergent branches must fail");
    assert_eq!(err.code, BambooError::GIT_ERROR);
    assert!(
        err.message.contains("git pull failed"),
        "unexpected error: {}",
        err.message
    );

    // The auto-stash was popped back: the dirty change is restored and
    // nothing is stranded on the stash stack.
    assert_eq!(std::fs::read_to_string(work.path().join("a.txt"))?, "dirty\n");
    assert!(gitq(work.path(), &["stash", "list"])?.trim().is_empty());
    // The failed pull did not move HEAD.
    assert_eq!(
        gitq(work.path(), &["rev-parse", "HEAD"])?.trim(),
        pre_pull_head
    );
    Ok(())
}

#[tokio::test]
async fn pull_merge_conflict_returns_error_and_queue_survives() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Both sides modify a.txt differently → the merge cannot auto-resolve.
    advance_origin(&origin, "remote change", "a.txt")?;
    std::fs::write(work.path().join("a.txt"), "local change\n")?;
    gitq(work.path(), &["add", "."])?;
    gitq(work.path(), &["commit", "-q", "-m", "local change"])?;

    let err = session
        .enqueue_write(WriteCommand::Pull {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            strategy: PullStrategy::Merge,
            auto_stash: false,
        })
        .await
        .expect_err("conflicting pull must fail, not hang the queue");
    assert_eq!(err.code, BambooError::GIT_ERROR);

    // Conflict markers in the worktree, as the conflict flow expects.
    let content = std::fs::read_to_string(work.path().join("a.txt"))?;
    assert!(content.contains("<<<<<<<"), "unexpected content: {content}");

    // The queue is alive: a subsequent write still executes.
    session
        .enqueue_write(WriteCommand::Stage(vec!["a.txt".to_string()]))
        .await?;
    Ok(())
}

#[tokio::test]
async fn branch_status_rejects_unknown_and_head_branch_names() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // An unknown branch must be an explicit INVALID_ARGUMENT (not an
    // indistinguishable "no upstream, 0/0" result), and an explicit
    // "HEAD" would silently resolve against detached HEAD.
    for bad in ["no-such-branch", "HEAD"] {
        let err = bamboo_engine::gix_read::read_branch_status(&session, Some(bad))
            .expect_err("invalid branch must be an error");
        let e = err.downcast_ref::<BambooError>().unwrap();
        assert_eq!(e.code, BambooError::INVALID_ARGUMENT, "{bad}: {e}");
    }
    Ok(())
}

#[tokio::test]
async fn get_remotes_redacts_embedded_url_credentials() -> anyhow::Result<()> {
    let (work, _origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    gitq(
        work.path(),
        &["remote", "set-url", "origin", "https://user:token@example.com/repo.git"],
    )?;
    let remotes = bamboo_engine::gix_read::read_remotes(&session)?;
    let url = remotes
        .iter()
        .find(|r| r.name == "origin")
        .unwrap()
        .url
        .as_deref()
        .expect("origin url");
    assert!(url.starts_with("https://***@"), "not redacted: {url}");
    assert!(!url.contains("user"), "not redacted: {url}");
    assert!(!url.contains("token"), "not redacted: {url}");
    assert!(
        url.ends_with("example.com/repo.git"),
        "host/path must survive: {url}"
    );
    Ok(())
}

#[tokio::test]
async fn stash_pop_explicit_ref_targets_the_named_entry() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("a.txt"), "first\n")?;
    session
        .enqueue_write(WriteCommand::StashPush {
            message: Some("first".to_string()),
            include_untracked: false,
            staged_only: false,
        })
        .await?;
    std::fs::write(dir.path().join("b.txt"), "second\n")?;
    session
        .enqueue_write(WriteCommand::StashPush {
            message: Some("second".to_string()),
            include_untracked: false,
            staged_only: false,
        })
        .await?;

    // stash@{1} is the older ("first") entry; popping it restores only
    // that entry's changes.
    session
        .enqueue_write(WriteCommand::StashPop {
            stash_ref: Some("stash@{1}".to_string()),
            apply: false,
            reinstate_index: false,
        })
        .await?;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "first\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("b.txt"))?, "b\n");

    // Only the "second" entry remains, now at stash@{0}.
    let entries = bamboo_engine::gix_read::read_stash_list(&session)?;
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].message.contains("second"),
        "unexpected subject: {:?}",
        entries[0].message
    );
    // The date is strict ISO-8601 (%cI), as the protocol doc promises.
    assert!(
        entries[0].date.contains('T') && entries[0].date.contains('+'),
        "date should be ISO-8601 with offset: {:?}",
        entries[0].date
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Conflict flow (phase 3): conflict classification, resolution, commit
// guard, and stage-revision blob reads for the merge editor.
// ---------------------------------------------------------------------------

/// Both-modified conflict on a.txt: main writes "main\n", branch `side`
/// writes "side\n", merging `side` into main conflicts (UU). Stages:
/// :1 = "a\n" (base), :2 = "main\n" (ours), :3 = "side\n" (theirs).
fn both_modified_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = test_repo()?;
    gitq(dir.path(), &["checkout", "-q", "-b", "side"])?;
    std::fs::write(dir.path().join("a.txt"), "side\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "side change"])?;
    gitq(dir.path(), &["checkout", "-q", "main"])?;
    std::fs::write(dir.path().join("a.txt"), "main\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "main change"])?;
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        !merge.status.success(),
        "expected merge to conflict, got: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    Ok(dir)
}

/// deleted-by-them conflict on b.txt: `side` deletes it, main modifies
/// it; merging `side` into main conflicts (UD).
fn deleted_by_them_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = test_repo()?;
    gitq(dir.path(), &["checkout", "-q", "-b", "side"])?;
    gitq(dir.path(), &["rm", "-q", "b.txt"])?;
    gitq(dir.path(), &["commit", "-q", "-m", "side deletes b"])?;
    gitq(dir.path(), &["checkout", "-q", "main"])?;
    std::fs::write(dir.path().join("b.txt"), "ours\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "main modifies b"])?;
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        !merge.status.success(),
        "expected merge to conflict, got: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    Ok(dir)
}

/// deleted-by-us conflict on c.txt: main deletes it, `side` modifies it;
/// merging `side` into main conflicts (DU).
fn deleted_by_us_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = test_repo()?;
    gitq(dir.path(), &["checkout", "-q", "-b", "side"])?;
    std::fs::write(dir.path().join("c.txt"), "theirs\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "side modifies c"])?;
    gitq(dir.path(), &["checkout", "-q", "main"])?;
    gitq(dir.path(), &["rm", "-q", "c.txt"])?;
    gitq(dir.path(), &["commit", "-q", "-m", "main deletes c"])?;
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        !merge.status.success(),
        "expected merge to conflict, got: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    Ok(dir)
}

#[tokio::test]
async fn status_reports_conflict_kind() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let item = items
        .iter()
        .find(|i| i.path == "a.txt")
        .expect("conflicted a.txt should be listed");
    assert_eq!(item.status, bamboo_protocol::StatusCode::Conflict);
    assert_eq!(item.conflict_kind, Some(bamboo_protocol::ConflictKind::BothModified));

    // Delete/modify conflicts classify too.
    let dir = deleted_by_them_repo()?;
    let session = open_session(&dir).await?;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let item = items
        .iter()
        .find(|i| i.path == "b.txt")
        .expect("conflicted b.txt should be listed");
    assert_eq!(item.status, bamboo_protocol::StatusCode::Conflict);
    assert_eq!(item.conflict_kind, Some(bamboo_protocol::ConflictKind::DeletedByThem));
    Ok(())
}

#[tokio::test]
async fn resolve_conflict_theirs_and_commit() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    let out = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "a.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Theirs,
        })
        .await?;
    assert!(out.contains("a.txt"), "output should name the path: {out}");

    settle().await;
    // Worktree took their blob; the resolution is staged (vs HEAD "main\n").
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "side\n");
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items
            .iter()
            .any(|i| i.path == "a.txt" && i.staged
                && i.status == bamboo_protocol::StatusCode::Modified),
        "resolved file should be staged as modified, got {:?}",
        items
    );
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));

    // With no unmerged entries left, the commit goes through.
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "merge side".to_string(),
            amend: false,
        })
        .await?;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "side\n");
    Ok(())
}

#[tokio::test]
async fn resolve_conflict_ours_yields_our_content() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "a.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Ours,
        })
        .await?;
    settle().await;
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "main\n");

    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    // Our blob equals HEAD's, so after staging nothing shows as a change.
    assert!(
        !items.iter().any(|i| i.path == "a.txt"),
        "resolved-with-ours (== HEAD) should show no change, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn resolve_conflict_rejects_mismatches_and_stale_paths() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    // keepOurs on a content conflict: both sides have blobs; only
    // ours/theirs are legal.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "a.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::KeepOurs,
        })
        .await
        .expect_err("keepOurs on both-modified must fail");
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    let msg = format!("{err}");
    assert!(msg.contains("both-modified"), "should name the kind: {msg}");
    assert!(msg.contains("ours/theirs"), "should suggest ours/theirs: {msg}");

    // A non-conflicted path (unmodified since HEAD) is rejected: the
    // classification is fresh, not taken from the client snapshot.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "c.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Ours,
        })
        .await
        .expect_err("resolving a clean path must fail");
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // The conflicted worktree content is untouched by the failed attempts.
    assert!(std::fs::read_to_string(dir.path().join("a.txt"))?.contains("<<<<<<<"));
    Ok(())
}

/// both-modified conflict on `sub/f.txt` — the conflict lives in a
/// subdirectory, so a directory-shaped resolve request ("sub") can be
/// exercised: its pathspec would recursively match every file under `sub/`.
fn subdir_conflict_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = test_repo()?;
    std::fs::create_dir(dir.path().join("sub"))?;
    std::fs::write(dir.path().join("sub/f.txt"), "base\n")?;
    gitq(dir.path(), &["add", "sub"])?;
    gitq(dir.path(), &["commit", "-q", "-m", "base f"])?;
    gitq(dir.path(), &["checkout", "-q", "-b", "side"])?;
    std::fs::write(dir.path().join("sub/f.txt"), "side\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "side f"])?;
    gitq(dir.path(), &["checkout", "-q", "main"])?;
    std::fs::write(dir.path().join("sub/f.txt"), "main\n")?;
    gitq(dir.path(), &["commit", "-q", "-am", "main f"])?;
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        !merge.status.success(),
        "expected merge to conflict, got: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    Ok(dir)
}

#[tokio::test]
async fn resolve_conflict_rejects_directory_shaped_paths() -> anyhow::Result<()> {
    let dir = subdir_conflict_repo()?;
    let session = open_session(&dir).await?;

    // "sub" is a valid repo path with exactly one conflicted file under
    // it, so a pathspec-only guard would pass — and the follow-up
    // `git checkout --ours -- sub` would revert uncommitted sibling
    // changes under sub/, `git add -- sub` would stage the whole
    // directory. The exact-path classification must reject it.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "sub".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Ours,
        })
        .await
        .expect_err("directory-shaped resolve path must fail");
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // The conflict is untouched: markers still in the worktree file, and
    // the file (not the directory) still resolves normally afterwards.
    assert!(std::fs::read_to_string(dir.path().join("sub/f.txt"))?.contains("<<<<<<<"));
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "sub/f.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Theirs,
        })
        .await?;
    settle().await;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    assert_eq!(std::fs::read_to_string(dir.path().join("sub/f.txt"))?, "side\n");
    Ok(())
}

#[tokio::test]
async fn resolve_deleted_by_them_conflict() -> anyhow::Result<()> {
    let dir = deleted_by_them_repo()?;
    let session = open_session(&dir).await?;

    // ours/theirs are illegal here: one side has no blob.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "b.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::Ours,
        })
        .await
        .expect_err("ours on deleted-by-them must fail");
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // keepOurs keeps our modified file.
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "b.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::KeepOurs,
        })
        .await?;
    settle().await;
    assert_eq!(std::fs::read_to_string(dir.path().join("b.txt"))?, "ours\n");
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    // Then commit goes through.
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "merge side keeping b".to_string(),
            amend: false,
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn resolve_deleted_by_them_keep_theirs_confirms_deletion() -> anyhow::Result<()> {
    let dir = deleted_by_them_repo()?;
    let session = open_session(&dir).await?;

    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "b.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::KeepTheirs,
        })
        .await?;
    settle().await;
    assert!(!dir.path().join("b.txt").exists(), "file should be gone");
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    // The confirmed deletion is staged (HEAD still has b.txt until the
    // merge commit lands).
    assert!(
        items
            .iter()
            .any(|i| i.path == "b.txt" && i.staged && i.status == bamboo_protocol::StatusCode::Deleted),
        "deletion confirmed: b.txt should be a staged deletion, got {:?}",
        items
    );
    Ok(())
}

#[tokio::test]
async fn resolve_deleted_by_us_conflict() -> anyhow::Result<()> {
    let dir = deleted_by_us_repo()?;
    let session = open_session(&dir).await?;

    // keepOurs confirms our deletion: their worktree copy is removed.
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "c.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::KeepOurs,
        })
        .await?;
    settle().await;
    assert!(!dir.path().join("c.txt").exists(), "our deletion should win");
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items.iter().any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    assert!(!items.iter().any(|i| i.path == "c.txt"));

    // keepTheirs (on a fresh conflict) restores their version from the
    // index stage — even though the worktree copy was left by the merge.
    let dir = deleted_by_us_repo()?;
    let session = open_session(&dir).await?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::ResolveConflict {
            path: "c.txt".to_string(),
            resolution: bamboo_protocol::ConflictResolution::KeepTheirs,
        })
        .await?;
    settle().await;
    assert_eq!(std::fs::read_to_string(dir.path().join("c.txt"))?, "theirs\n");
    Ok(())
}

#[tokio::test]
async fn commit_is_blocked_while_conflicted() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "should not work".to_string(),
            amend: false,
        })
        .await
        .expect_err("commit with unmerged entries must fail");
    let msg = format!("{err}");
    assert!(msg.contains("a.txt"), "error should name the file: {msg}");
    assert!(
        msg.to_lowercase().contains("conflict"),
        "error should mention conflicts: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn read_blob_serves_conflict_stages() -> anyhow::Result<()> {
    let dir = both_modified_repo()?;
    let session = open_session(&dir).await?;

    let base = bamboo_engine::gix_read::read_blob(&session, ":1", "a.txt")?;
    let ours = bamboo_engine::gix_read::read_blob(&session, ":2", "a.txt")?;
    let theirs = bamboo_engine::gix_read::read_blob(&session, ":3", "a.txt")?;
    assert_eq!(base, "a\n");
    assert_eq!(ours, "main\n");
    assert_eq!(theirs, "side\n");

    // A missing stage (delete/modify conflicts have no :3/:2 on the
    // deleted side... deleted-by-them still stages ours at :2) resolves
    // to an empty string — that side is legitimately absent, not an
    // error, so the TS side can distinguish it from engine failures.
    let dir = deleted_by_them_repo()?;
    let session = open_session(&dir).await?;
    assert_eq!(
        bamboo_engine::gix_read::read_blob(&session, ":2", "b.txt")?,
        "ours\n",
        "ours stage should exist for deleted-by-them"
    );
    assert_eq!(
        bamboo_engine::gix_read::read_blob(&session, ":3", "b.txt")?,
        "",
        "theirs stage should be absent (empty) when they deleted the file"
    );
    assert_eq!(
        bamboo_engine::gix_read::read_blob(&session, ":2", "a.txt")?,
        "",
        "a non-conflicted path has no unmerged entries — absent, not an error"
    );
    Ok(())
}
