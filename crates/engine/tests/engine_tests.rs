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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

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
// Graph filtering (filter as a query condition)
// ---------------------------------------------------------------------------

/// Build the filter-test repo. History (dates increase so newest-first
/// order is deterministic):
///
/// ```text
/// 1 root  "init project"        Alice <alice@corp.example>  README.md docs/guide.md src/main.rs
/// 2       "fix typo in guide"   Alice                        docs/guide.md
/// 3 feat  "feat: add engine"    Bob   <bob@corp.example>    src/engine.rs
/// 4 feat  "fix: engine crash"   Bob                          src/engine.rs
/// 5 merge "merge feature"       Alice                        (merges 4 into main)
/// ```
///
/// The branch and merge matter for two semantics: the merge's diff is
/// taken against its FIRST parent (so it shows src/engine.rs as added),
/// and lanes are assigned over the full topology, not the hit list.
fn filter_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
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
    git(&["init", "-q", "-b", "main"])?;
    git(&["config", "core.autocrlf", "false"])?;

    let commit = |msg: &str,
                  author: (&str, &str),
                  date: &str,
                  files: &[(&str, &str)]|
     -> anyhow::Result<()> {
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(full, content)?;
        }
        git(&["add", "."])?;
        let out = Command::new("git")
            .arg("commit")
            .arg("-q")
            .arg("-m")
            .arg(msg)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", author.0)
            .env("GIT_AUTHOR_EMAIL", author.1)
            .env("GIT_COMMITTER_NAME", author.0)
            .env("GIT_COMMITTER_EMAIL", author.1)
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

    let alice = ("Alice", "alice@corp.example");
    let bob = ("Bob", "bob@corp.example");
    commit(
        "init project",
        alice,
        "2026-01-01T00:00:00Z",
        &[
            ("README.md", "readme\n"),
            ("docs/guide.md", "guide\n"),
            ("src/main.rs", "fn main() {}\n"),
        ],
    )?;
    commit(
        "fix typo in guide",
        alice,
        "2026-01-02T00:00:00Z",
        &[("docs/guide.md", "guide, fixed\n")],
    )?;
    git(&["checkout", "-q", "-b", "feature"])?;
    commit(
        "feat: add engine",
        bob,
        "2026-01-03T00:00:00Z",
        &[("src/engine.rs", "engine v1\n")],
    )?;
    commit(
        "fix: engine crash",
        bob,
        "2026-01-04T00:00:00Z",
        &[("src/engine.rs", "engine v2\n")],
    )?;
    git(&["checkout", "-q", "main"])?;
    // Non-fast-forward merge (feature is two commits ahead of main).
    let out = Command::new("git")
        .args(["merge", "--no-ff", "-q", "-m", "merge feature", "feature"])
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Alice")
        .env("GIT_AUTHOR_EMAIL", "alice@corp.example")
        .env("GIT_COMMITTER_NAME", "Alice")
        .env("GIT_COMMITTER_EMAIL", "alice@corp.example")
        .env("GIT_AUTHOR_DATE", "2026-01-05T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-05T00:00:00Z")
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(dir)
}

/// Read one filtered page over `filter_repo`.
async fn filtered_page(
    session: &std::sync::Arc<bamboo_engine::session::Session>,
    filter: &bamboo_protocol::GraphFilter,
) -> anyhow::Result<bamboo_protocol::GraphPage> {
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 100,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    bamboo_engine::gix_read::read_graph_page_with_filter(session, &viewport, Some(filter))
}

fn graph_filter(
    author: Option<&str>,
    message: Option<&str>,
    path: Option<&str>,
) -> bamboo_protocol::GraphFilter {
    bamboo_protocol::GraphFilter {
        author: author.map(str::to_string),
        message: message.map(str::to_string),
        path: path.map(str::to_string),
    }
}

/// Assert the page's commit messages, in row order, exactly.
fn assert_messages(page: &bamboo_protocol::GraphPage, expected: &[&str]) {
    let actual: Vec<&str> = page.commits.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(actual, expected, "unexpected filtered rows");
}

#[tokio::test]
async fn graph_filter_message_substring_is_exact_and_ordered() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let page = filtered_page(&session, &graph_filter(None, Some("fix"), None)).await?;
    assert_messages(&page, &["fix: engine crash", "fix typo in guide"]);
    // Filtered reads report the EXACT hit count, not a lower bound.
    assert_eq!(page.total_approx, 2);
    assert!(!page.has_more);
    assert!(!page.truncated);
    // Substring, not prefix: "typo" matches mid-message.
    let page = filtered_page(&session, &graph_filter(None, Some("typo"), None)).await?;
    assert_messages(&page, &["fix typo in guide"]);
    Ok(())
}

#[tokio::test]
async fn graph_filter_author_matches_name_or_email_case_insensitively() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    // Name, case-insensitive (merge + the two Alice commits, newest first).
    let page = filtered_page(&session, &graph_filter(Some("ALICE"), None, None)).await?;
    assert_messages(
        &page,
        &["merge feature", "fix typo in guide", "init project"],
    );

    // Email substring: the local part alone matches both hosters' addresses.
    let page = filtered_page(&session, &graph_filter(Some("bob@"), None, None)).await?;
    assert_messages(&page, &["fix: engine crash", "feat: add engine"]);

    // No match: exact miss yields an empty page, not an error.
    let page = filtered_page(&session, &graph_filter(Some("nobody"), None, None)).await?;
    assert_messages(&page, &[]);
    assert_eq!(page.total_approx, 0);
    assert!(!page.has_more);
    Ok(())
}

#[tokio::test]
async fn graph_filter_path_substring_matches_changed_paths() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    // File-name substring: the two engine commits plus the merge (its diff
    // vs. the FIRST parent adds src/engine.rs).
    let page = filtered_page(&session, &graph_filter(None, None, Some("engine.rs"))).await?;
    assert_messages(
        &page,
        &["merge feature", "fix: engine crash", "feat: add engine"],
    );

    // Directory substring: includes the ROOT commit (its diff vs. the
    // empty tree adds docs/guide.md).
    let page = filtered_page(&session, &graph_filter(None, None, Some("docs/"))).await?;
    assert_messages(&page, &["fix typo in guide", "init project"]);

    // Paths are case-sensitive, unlike author/message.
    let page = filtered_page(&session, &graph_filter(None, None, Some("Docs"))).await?;
    assert_messages(&page, &[]);

    // A file only ever touched on the feature branch is reachable through
    // the merge tip.
    let page = filtered_page(&session, &graph_filter(None, None, Some("src/engine"))).await?;
    assert_eq!(page.total_approx, 3);
    Ok(())
}

#[tokio::test]
async fn graph_filter_conditions_and_together() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    // Alice + "fix": the guide fix, not Bob's engine fixes.
    let page = filtered_page(&session, &graph_filter(Some("alice"), Some("fix"), None)).await?;
    assert_messages(&page, &["fix typo in guide"]);

    // All three conditions.
    let page = filtered_page(
        &session,
        &graph_filter(Some("bob"), Some("crash"), Some("engine")),
    )
    .await?;
    assert_messages(&page, &["fix: engine crash"]);
    Ok(())
}

#[tokio::test]
async fn graph_filter_paginates_over_the_materialized_list() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let filter = graph_filter(None, None, Some("src"));
    // Hits: merge, engine crash, add engine, init project (root adds
    // src/main.rs) — newest first.
    let viewport = |offset: u64, limit: u32| bamboo_protocol::GraphViewport {
        offset,
        limit,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    let page = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport(0, 2),
        Some(&filter),
    )?;
    assert_messages(&page, &["merge feature", "fix: engine crash"]);
    assert_eq!(page.total_approx, 4);
    assert!(page.has_more);

    let page = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport(3, 2),
        Some(&filter),
    )?;
    assert_messages(&page, &["init project"]);
    assert_eq!(page.total_approx, 4);
    assert!(!page.has_more);
    Ok(())
}

#[tokio::test]
async fn graph_filter_anchor_repins_in_the_filtered_list() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let all = filtered_page(&session, &graph_filter(Some("alice"), None, None)).await?;
    assert_eq!(all.commits.len(), 3);
    // Anchor to the OLDEST Alice commit: the page starts there even though
    // the offset would start at 0.
    let anchor = &all.commits[2].id;
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 2,
        anchor_commit: Some(anchor.clone()),
        epoch: session.current_epoch(),
    };
    let page = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport,
        Some(&graph_filter(Some("alice"), None, None)),
    )?;
    assert_messages(&page, &["init project"]);
    assert_eq!(page.total_approx, 3);
    assert!(!page.has_more);

    // Unknown anchors keep the requested offset.
    let viewport = bamboo_protocol::GraphViewport {
        offset: 1,
        limit: 1,
        anchor_commit: Some("0000000000000000000000000000000000000000".to_string()),
        epoch: session.current_epoch(),
    };
    let page = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport,
        Some(&graph_filter(Some("alice"), None, None)),
    )?;
    assert_messages(&page, &["fix typo in guide"]);
    Ok(())
}

#[tokio::test]
async fn graph_filter_empty_filter_equals_unfiltered() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 100,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };
    let unfiltered = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;

    // An all-None filter is a no-op, and so is a whitespace-only one after
    // normalization (the server normalizes; the engine double-guards).
    let page = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport,
        Some(&bamboo_protocol::GraphFilter::default()),
    )?;
    assert_eq!(page, unfiltered);

    // A filtered read first (populating lanes at this epoch) must not
    // disturb the unfiltered read.
    let filtered = filtered_page(&session, &graph_filter(None, Some("merge"), None)).await?;
    assert_eq!(filtered.commits.len(), 1);
    let again = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(again.commits.len(), 5);
    Ok(())
}

#[tokio::test]
async fn graph_filter_epoch_mismatch_still_rejects() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: session.current_epoch() + 99,
    };
    let err = bamboo_engine::gix_read::read_graph_page_with_filter(
        &session,
        &viewport,
        Some(&graph_filter(None, Some("fix"), None)),
    )
    .expect_err("stale epoch must be rejected");
    assert!(err.to_string().contains("epoch"));
    Ok(())
}

#[tokio::test]
async fn graph_filter_truncation_reports_lower_bound() -> anyhow::Result<()> {
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 100,
        anchor_commit: None,
        epoch: session.current_epoch(),
    };

    // Walk cap: stop after 3 commits (merge, crash, engine) — the walk
    // never reaches the older "fix typo in guide".
    let limits = bamboo_engine::graph_filter::FilterLimits {
        max_commits: 3,
        ..Default::default()
    };
    let page = bamboo_engine::gix_read::read_graph_page_with_limits(
        &session,
        &viewport,
        Some(&graph_filter(None, Some("fix"), None)),
        &limits,
    )?;
    assert!(page.truncated);
    assert_eq!(page.total_approx, 1); // only "fix: engine crash" was reached
    assert_messages(&page, &["fix: engine crash"]);

    // Time budget: a zero budget with a path condition truncates before
    // any hit is examined.
    let limits = bamboo_engine::graph_filter::FilterLimits {
        tree_diff_time_budget: std::time::Duration::ZERO,
        ..Default::default()
    };
    let page = bamboo_engine::gix_read::read_graph_page_with_limits(
        &session,
        &viewport,
        Some(&graph_filter(None, None, Some("src"))),
        &limits,
    )?;
    assert!(page.truncated);
    assert_eq!(page.total_approx, 0);
    assert_messages(&page, &[]);
    Ok(())
}

#[tokio::test]
async fn graph_filter_switching_filters_serves_each_view() -> anyhow::Result<()> {
    // The memoized view is keyed by filter: alternating between filters
    // (the user editing the box) must serve each list correctly, and the
    // same filter twice must agree with itself.
    let dir = filter_repo()?;
    let session = open_session(&dir).await?;

    let by_message = graph_filter(None, Some("fix"), None);
    let by_author = graph_filter(Some("bob"), None, None);

    for round in 0..2 {
        let a = filtered_page(&session, &by_message).await?;
        assert_eq!(a.total_approx, 2, "message view, round {round}");
        let b = filtered_page(&session, &by_author).await?;
        assert_eq!(b.total_approx, 2, "author view, round {round}");
        let a2 = filtered_page(&session, &by_message).await?;
        assert_eq!(a2.total_approx, 2);
        assert_messages(&a2, &["fix: engine crash", "fix typo in guide"]);
    }
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
        .await?
        .expect_output();
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

use bamboo_engine::write_queue::WriteCommand;
use bamboo_protocol::{BambooError, ForceMode, PullStrategy};

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
fn advance_origin(origin: &tempfile::TempDir, msg: &str, filename: &str) -> anyhow::Result<String> {
    let clone = tempfile::tempdir()?;
    gitq(
        clone.path(),
        &["clone", "-q", origin.path().to_str().unwrap(), "."],
    )?;
    std::fs::write(clone.path().join(filename), format!("{msg}\n"))?;
    gitq(clone.path(), &["add", "."])?;
    gitq(clone.path(), &["commit", "-q", "-m", msg])?;
    gitq(
        clone.path(),
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    )?;
    Ok(gitq(clone.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
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
    let local_sha = gitq(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
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
    let local_sha = gitq(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
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
        .await?
        .expect_output();
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
        format!("{err}").contains("invalid ref name") || format!("{err}").contains("remote"),
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
    assert_ne!(
        gitq(work.path(), &["rev-parse", "HEAD"])?.trim(),
        remote_sha
    );
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
        .await?
        .expect_output();
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
    assert!(gitq(
        work.path(),
        &["rev-parse", "--verify", "refs/remotes/other/main"]
    )
    .is_ok());

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
    gitq(
        work.path(),
        &["commit", "-q", "--allow-empty", "-m", "local ahead"],
    )?;
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
    let sha = gitq(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
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
        .await?
        .expect_output();
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
        items.iter().any(|i| i.path == "a.txt"
            && i.staged
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
        .await?
        .expect_output();
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
            Ok(output) => {
                panic!("stash ref {bad:?} must be rejected, got success: {output:?}")
            }
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
    let pre_pull_head = gitq(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();

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
    assert_eq!(
        std::fs::read_to_string(work.path().join("a.txt"))?,
        "dirty\n"
    );
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
        &[
            "remote",
            "set-url",
            "origin",
            "https://user:token@example.com/repo.git",
        ],
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
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt"))?,
        "first\n"
    );
    assert_eq!(std::fs::read_to_string(dir.path().join("b.txt"))?, "b\n");

    // Only the "second" entry remains, now at stash@{0}.
    let entries = bamboo_engine::gix_read::read_stash_list(&session)?;
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].message.contains("second"),
        "unexpected subject: {:?}",
        entries[0].message
    );
    // The date is strict ISO-8601 (%cI), as the protocol doc promises. Parsed
    // rather than substring-matched, because git prints the zero offset as `Z`
    // and not `+00:00` — so which zone form appears depends on the machine's
    // timezone, and a UTC host (every CI runner) yields `...Z`. Both are
    // ISO-8601; asserting a literal '+' made this test pass only off-UTC.
    chrono::DateTime::parse_from_rfc3339(&entries[0].date).unwrap_or_else(|e| {
        panic!(
            "date should be strict ISO-8601: {:?} ({e})",
            entries[0].date
        )
    });
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
    assert_eq!(
        item.conflict_kind,
        Some(bamboo_protocol::ConflictKind::BothModified)
    );

    // Delete/modify conflicts classify too.
    let dir = deleted_by_them_repo()?;
    let session = open_session(&dir).await?;
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    let item = items
        .iter()
        .find(|i| i.path == "b.txt")
        .expect("conflicted b.txt should be listed");
    assert_eq!(item.status, bamboo_protocol::StatusCode::Conflict);
    assert_eq!(
        item.conflict_kind,
        Some(bamboo_protocol::ConflictKind::DeletedByThem)
    );
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
        .await?
        .expect_output();
    assert!(out.contains("a.txt"), "output should name the path: {out}");

    settle().await;
    // Worktree took their blob; the resolution is staged (vs HEAD "main\n").
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt"))?, "side\n");
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(
        items.iter().any(|i| i.path == "a.txt"
            && i.staged
            && i.status == bamboo_protocol::StatusCode::Modified),
        "resolved file should be staged as modified, got {:?}",
        items
    );
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));

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
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
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
    assert!(
        msg.contains("ours/theirs"),
        "should suggest ours/theirs: {msg}"
    );

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
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/f.txt"))?,
        "side\n"
    );
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
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
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
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
    // The confirmed deletion is staged (HEAD still has b.txt until the
    // merge commit lands).
    assert!(
        items.iter().any(|i| i.path == "b.txt"
            && i.staged
            && i.status == bamboo_protocol::StatusCode::Deleted),
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
    assert!(
        !dir.path().join("c.txt").exists(),
        "our deletion should win"
    );
    let items = bamboo_engine::gix_read::read_status(&session, None)?;
    assert!(!items
        .iter()
        .any(|i| i.status == bamboo_protocol::StatusCode::Conflict));
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
    assert_eq!(
        std::fs::read_to_string(dir.path().join("c.txt"))?,
        "theirs\n"
    );
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

// ---------------------------------------------------------------------------
// getDiff / stageRanges / unstageRanges (hunk-level staging, phase 5)
// ---------------------------------------------------------------------------

/// A 12-line file committed to HEAD, then dirtied by the test. Two
/// separated edits (line 2 and line 12) produce exactly two hunks under
/// `-U3`, the canonical hunk-staging shape.
fn hunk_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = test_repo()?;
    let body: String = (1..=12).map(|n| format!("line{n}\n")).collect();
    std::fs::write(dir.path().join("d.txt"), body)?;
    gitq(dir.path(), &["add", "--", "d.txt"])?;
    gitq(dir.path(), &["commit", "-q", "-m", "add d.txt"])?;
    Ok(dir)
}

/// Rewrite d.txt with `line2` → `l2-dirty` and `line12` → `l12-dirty`.
fn dirty_two_hunks(dir: &std::path::Path) -> anyhow::Result<()> {
    let mut body = String::new();
    for n in 1..=12 {
        match n {
            2 => body.push_str("l2-dirty\n"),
            12 => body.push_str("l12-dirty\n"),
            _ => body.push_str(&format!("line{n}\n")),
        }
    }
    std::fs::write(dir.join("d.txt"), body)?;
    Ok(())
}

#[tokio::test]
async fn get_diff_lists_unstaged_and_staged_hunks() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Worktree-vs-index: two hunks, correctly positioned.
    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert!(!unstaged.binary);
    assert_eq!(unstaged.hunks.len(), 2);
    let h1 = &unstaged.hunks[0];
    let h2 = &unstaged.hunks[1];
    assert_eq!((h1.old_start, h1.old_lines), (1, 5));
    assert_eq!((h1.new_start, h1.new_lines), (1, 5));
    assert_eq!((h2.old_start, h2.old_lines), (9, 4));
    assert_eq!((h2.new_start, h2.new_lines), (9, 4));
    // Hunk 1 body: context around the edit — `line1`, `-line2`,
    // `+l2-dirty`, `line3`..`line5`.
    let removed: Vec<&str> = h1
        .lines
        .iter()
        .filter(|l| l.kind == bamboo_protocol::DiffLineKind::Removed)
        .map(|l| l.text.as_str())
        .collect();
    let added: Vec<&str> = h1
        .lines
        .iter()
        .filter(|l| l.kind == bamboo_protocol::DiffLineKind::Added)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(removed, ["line2"]);
    assert_eq!(added, ["l2-dirty"]);
    // old/new line counts must equal header counts (the invariant the
    // blob reconstruction relies on).
    for h in &unstaged.hunks {
        let old_side = h
            .lines
            .iter()
            .filter(|l| l.kind != bamboo_protocol::DiffLineKind::Added)
            .count() as u32;
        let new_side = h
            .lines
            .iter()
            .filter(|l| l.kind != bamboo_protocol::DiffLineKind::Removed)
            .count() as u32;
        assert_eq!(old_side, h.old_lines, "old-side count mismatch");
        assert_eq!(new_side, h.new_lines, "new-side count mismatch");
    }

    // Nothing staged yet → staged diff is empty.
    let staged = bamboo_engine::gix_read::read_diff(&session, "d.txt", true)?;
    assert!(staged.hunks.is_empty());

    // After git add: staged diff has the two hunks, worktree diff is empty.
    gitq(dir.path(), &["add", "--", "d.txt"])?;
    let staged = bamboo_engine::gix_read::read_diff(&session, "d.txt", true)?;
    assert_eq!(staged.hunks.len(), 2);
    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert!(unstaged.hunks.is_empty());
    Ok(())
}

#[tokio::test]
async fn get_diff_flags_binary_files() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    std::fs::write(dir.path().join("d.txt"), [0u8, 159, 146, 150, 0, 1, 2])?;

    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert!(unstaged.binary);
    assert!(unstaged.hunks.is_empty());
    Ok(())
}

fn stage_cmd(path: &str, ranges: &[(u32, u32)]) -> WriteCommand {
    WriteCommand::StageRanges {
        path: path.to_string(),
        ranges: ranges.iter().map(|(s, e)| (*s, *e)).collect(),
    }
}

fn unstage_cmd(path: &str, ranges: &[(u32, u32)]) -> WriteCommand {
    WriteCommand::UnstageRanges {
        path: path.to_string(),
        ranges: ranges.iter().map(|(s, e)| (*s, *e)).collect(),
    }
}

#[tokio::test]
async fn stage_ranges_stages_one_hunk_of_two() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Hunk 1 occupies new-side lines [1,5] (edit + context); a range
    // anywhere inside selects it.
    session.enqueue_write(stage_cmd("d.txt", &[(2, 2)])).await?;

    // Worktree untouched; index holds line2 but NOT line12.
    let index = gitq(dir.path(), &["show", ":0:d.txt"])?;
    assert!(index.contains("l2-dirty"));
    assert!(!index.contains("l12-dirty"));
    let worktree = std::fs::read_to_string(dir.path().join("d.txt"))?;
    assert!(worktree.contains("l12-dirty"));
    // Status: staged modification + further unstaged modification.
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains("MM d.txt"), "status: {status}");
    Ok(())
}

#[tokio::test]
async fn stage_ranges_incremental_does_not_lose_earlier_hunks() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Stage hunk 1, then hunk 2 — the second call must EXTEND the index
    // content, not clobber it (this is why the base is the index blob,
    // not HEAD).
    session.enqueue_write(stage_cmd("d.txt", &[(2, 2)])).await?;
    session
        .enqueue_write(stage_cmd("d.txt", &[(12, 12)]))
        .await?;

    let index = gitq(dir.path(), &["show", ":0:d.txt"])?;
    assert!(index.contains("l2-dirty"));
    assert!(index.contains("l12-dirty"));
    // Everything staged now → clean against the worktree.
    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert!(unstaged.hunks.is_empty());
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains("M  d.txt"), "status: {status}");
    Ok(())
}

#[tokio::test]
async fn stage_ranges_stale_selection_is_invalid_argument() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Range 7..8 intersects neither hunk (hunk intervals [1,5] and [9,12]
    // — wait, hunk 2 covers new lines 9..12; pick a true gap: 7..8).
    let err = session
        .enqueue_write(stage_cmd("d.txt", &[(7, 8)]))
        .await
        .expect_err("a range hitting no hunk must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    // Nothing was written.
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains(" M d.txt"), "status: {status}");
    Ok(())
}

#[tokio::test]
async fn stage_ranges_rejects_bad_ranges_and_paths() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Empty range list.
    let err = session
        .enqueue_write(stage_cmd("d.txt", &[]))
        .await
        .expect_err("empty range list must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    // Zero / inverted / out-of-bounds ranges.
    for ranges in [[(0, 1)], [(5, 2)], [(1, 99_999_999)]] {
        let err = session
            .enqueue_write(stage_cmd("d.txt", &ranges))
            .await
            .expect_err("malformed range must be rejected");
        assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    }
    // Untracked path: no hunks, so selection is empty → stale rejection.
    std::fs::write(dir.path().join("new.txt"), "n\n")?;
    let err = session
        .enqueue_write(stage_cmd("new.txt", &[(1, 1)]))
        .await
        .expect_err("untracked path must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    Ok(())
}

#[tokio::test]
async fn stage_ranges_rejects_conflicted_path() -> anyhow::Result<()> {
    let (work, origin) = test_repo_with_origin()?;
    let session = open_session(&work).await?;

    // Manufacture a conflict (both sides touch a.txt differently).
    // Clone BEFORE the local commit so the histories diverge.
    let clone = tempfile::tempdir()?;
    gitq(
        clone.path(),
        &["clone", "-q", origin.path().to_str().unwrap(), "."],
    )?;
    std::fs::write(work.path().join("a.txt"), "ours\n")?;
    gitq(work.path(), &["add", "--", "a.txt"])?;
    gitq(work.path(), &["commit", "-q", "-m", "ours"])?;
    std::fs::write(clone.path().join("a.txt"), "theirs\n")?;
    gitq(clone.path(), &["commit", "-qam", "theirs"])?;
    gitq(clone.path(), &["push", "-q", "origin", "main"])?;
    let pull = Command::new("git")
        .args(["pull", "-q", "--no-rebase", "origin", "main"])
        .current_dir(work.path())
        .output()?;
    assert!(!pull.status.success(), "pull must conflict");
    let status = gitq(work.path(), &["status", "--porcelain"])?;
    assert!(status.contains("UU a.txt"), "status: {status}");

    let err = session
        .enqueue_write(stage_cmd("a.txt", &[(1, 1)]))
        .await
        .expect_err("conflicted path must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("conflict"));
    Ok(())
}

#[tokio::test]
async fn stage_ranges_stages_whole_file_deletion() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    std::fs::remove_file(dir.path().join("d.txt"))?;

    // The deletion hunk's new-side interval is [1,1] (empty new side).
    session.enqueue_write(stage_cmd("d.txt", &[(1, 1)])).await?;

    // Index entry removed (not an empty blob!), worktree still absent.
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains("D  d.txt"), "status: {status}");
    assert!(!dir.path().join("d.txt").exists());
    Ok(())
}

#[tokio::test]
async fn stage_ranges_preserves_missing_trailing_newline() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;

    // Rewrite d.txt so it ends WITHOUT a newline, changing the last line.
    let body: String = (1..=11)
        .map(|n| format!("line{n}\n"))
        .collect::<Vec<_>>()
        .join("")
        + "l12-dirty";
    std::fs::write(dir.path().join("d.txt"), body)?;

    session
        .enqueue_write(stage_cmd("d.txt", &[(12, 12)]))
        .await?;

    // The index blob must end without a trailing newline, byte-exactly.
    let index = gitq(dir.path(), &["show", ":0:d.txt"])?;
    let expected: String = (1..=11)
        .map(|n| format!("line{n}\n"))
        .collect::<Vec<_>>()
        .join("")
        + "l12-dirty";
    assert_eq!(index, expected);
    // And the worktree diff is now empty (fully staged).
    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert!(unstaged.hunks.is_empty());
    Ok(())
}

#[tokio::test]
async fn stage_ranges_preserves_executable_bit() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    gitq(dir.path(), &["update-index", "--chmod=+x", "d.txt"])?;
    gitq(dir.path(), &["commit", "-q", "-m", "chmod d.txt"])?;
    dirty_two_hunks(dir.path())?;

    session.enqueue_write(stage_cmd("d.txt", &[(2, 2)])).await?;

    // The reconstructed index entry keeps mode 100755 (via ls-files).
    let ls = gitq(dir.path(), &["ls-files", "-s", "--", "d.txt"])?;
    assert!(ls.starts_with("100755"), "ls-files: {ls}");
    Ok(())
}

#[tokio::test]
async fn unstage_ranges_reverts_one_hunk_of_two() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;
    gitq(dir.path(), &["add", "--", "d.txt"])?;

    // Unstage hunk 2 (new-side interval [9,12] in the staged diff).
    session
        .enqueue_write(unstage_cmd("d.txt", &[(12, 12)]))
        .await?;

    // Index now matches HEAD for line 12 but keeps line 2's change.
    let index = gitq(dir.path(), &["show", ":0:d.txt"])?;
    assert!(index.contains("l2-dirty"));
    assert!(index.contains("line12"));
    // The reverted change reappears as an unstaged worktree change.
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains("MM d.txt"), "status: {status}");
    let unstaged = bamboo_engine::gix_read::read_diff(&session, "d.txt", false)?;
    assert_eq!(unstaged.hunks.len(), 1);
    Ok(())
}

#[tokio::test]
async fn unstage_ranges_new_file_back_to_untracked() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    std::fs::write(dir.path().join("n.txt"), "new\n")?;
    gitq(dir.path(), &["add", "--", "n.txt"])?;

    // The whole-file hunk's interval is [1,1].
    session
        .enqueue_write(unstage_cmd("n.txt", &[(1, 1)]))
        .await?;

    // Removed from the index entirely (untracked again, file preserved).
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains("?? n.txt"), "status: {status}");
    assert_eq!(std::fs::read_to_string(dir.path().join("n.txt"))?, "new\n");
    Ok(())
}

#[tokio::test]
async fn unstage_ranges_staged_deletion_restores_index_entry() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    gitq(dir.path(), &["rm", "-q", "d.txt"])?;

    // The staged deletion hunk's new-side interval is [1,1].
    session
        .enqueue_write(unstage_cmd("d.txt", &[(1, 1)]))
        .await?;

    // Index restored to HEAD; the worktree stays deleted → unstaged " D".
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains(" D d.txt"), "status: {status}");
    assert!(
        !dir.path().join("d.txt").exists(),
        "unstage must not touch the worktree"
    );
    // Mode restored from the HEAD tree, not defaulted.
    let ls = gitq(dir.path(), &["ls-files", "-s", "--", "d.txt"])?;
    assert!(ls.starts_with("100644"), "ls-files: {ls}");
    Ok(())
}

#[tokio::test]
async fn stage_then_unstage_round_trip_leaves_index_at_head() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    dirty_two_hunks(dir.path())?;

    // Stage both hunks, then unstage both: index must equal HEAD again
    // while the worktree keeps both changes.
    session
        .enqueue_write(stage_cmd("d.txt", &[(2, 2), (12, 12)]))
        .await?;
    let staged = bamboo_engine::gix_read::read_diff(&session, "d.txt", true)?;
    assert_eq!(staged.hunks.len(), 2);
    session
        .enqueue_write(unstage_cmd("d.txt", &[(2, 2), (12, 12)]))
        .await?;

    let index = gitq(dir.path(), &["show", ":0:d.txt"])?;
    let head = gitq(dir.path(), &["show", "HEAD:d.txt"])?;
    assert_eq!(index, head);
    let status = gitq(dir.path(), &["status", "--porcelain"])?;
    assert!(status.contains(" M d.txt"), "status: {status}");
    Ok(())
}

#[tokio::test]
async fn stage_ranges_rejects_binary_files() -> anyhow::Result<()> {
    let dir = hunk_repo()?;
    let session = open_session(&dir).await?;
    std::fs::write(dir.path().join("d.txt"), [0u8, 159, 146, 150, 0, 1, 2])?;

    let err = session
        .enqueue_write(stage_cmd("d.txt", &[(1, 1)]))
        .await
        .expect_err("binary file must be rejected");
    assert_eq!(err.code, BambooError::INVALID_ARGUMENT);
    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive rebase (getRebasePlan / startRebase / status / steps / undo)
// ---------------------------------------------------------------------------

/// Rebase fixture: main = c1, c2; feature = f1, f2, f3 (branched at c2),
/// feature checked out. One file per commit (f1.txt..f3.txt) so drops are
/// directly observable in the worktree, and a "shared.txt" that f1 edits
/// (main gets its own m1 commit editing it after the branch, so rebasing
/// feature onto main conflicts at f1).
fn rebase_repo() -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
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
    let commit_at = |msg: &str, date: &str| -> anyhow::Result<()> {
        let out = Command::new("git")
            .arg("commit")
            .arg("-q")
            .arg("--allow-empty")
            .arg("-m")
            .arg(msg)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "Rebase Tester")
            .env("GIT_AUTHOR_EMAIL", "rebase@example.com")
            .env("GIT_COMMITTER_NAME", "Rebase Tester")
            .env("GIT_COMMITTER_EMAIL", "rebase@example.com")
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

    git(&["init", "-q", "-b", "main"])?;
    git(&["config", "core.autocrlf", "false"])?;
    // The rebase subprocesses spawned by the engine do not inherit the
    // test's identity env vars — pin the identity in the repo so replayed
    // commits get a stable committer everywhere.
    git(&["config", "user.name", "Rebase Tester"])?;
    git(&["config", "user.email", "rebase@example.com"])?;

    commit_at("c1", "2026-02-01T00:00:00Z")?;
    std::fs::write(dir.path().join("shared.txt"), "base\n")?;
    git(&["add", "."])?;
    commit_at("c2", "2026-02-02T00:00:00Z")?;
    git(&["branch", "feature"])?;

    // main-side divergence after the branch (conflict partner for f1).
    std::fs::write(dir.path().join("shared.txt"), "main-side\n")?;
    git(&["add", "."])?;
    commit_at("m1", "2026-02-03T00:00:00Z")?;

    git(&["checkout", "-q", "feature"])?;
    std::fs::write(dir.path().join("shared.txt"), "feature-side\n")?;
    std::fs::write(dir.path().join("f1.txt"), "f1\n")?;
    git(&["add", "."])?;
    commit_at("f1", "2026-02-04T00:00:00Z")?;
    std::fs::write(dir.path().join("f2.txt"), "f2\n")?;
    git(&["add", "."])?;
    commit_at("f2", "2026-02-05T00:00:00Z")?;
    std::fs::write(dir.path().join("f3.txt"), "f3\n")?;
    git(&["add", "."])?;
    commit_at("f3", "2026-02-06T00:00:00Z")?;
    Ok(dir)
}

/// The engine binary used as GIT_SEQUENCE_EDITOR/GIT_EDITOR inside rebase
/// tests (the test binary's own main does not handle the editor flags).
fn set_editor_exe() {
    // Same value for every test → benign under the multi-threaded test
    // runner despite env vars being process-global.
    std::env::set_var(
        "BAMBOO_ENGINE_EDITOR_EXE",
        env!("CARGO_BIN_EXE_bamboo-engine"),
    );
}

/// `git log --format=%s <rev>` subjects, newest first.
fn subjects(dir: &tempfile::TempDir, rev: &str) -> anyhow::Result<Vec<String>> {
    let out = Command::new("git")
        .args(["log", "--format=%s", rev])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// The full commit sha of a revision (fixture commits are findable by
/// subject via `%H %s` parsing).
fn sha_of(dir: &tempfile::TempDir, subject: &str) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(["log", "--format=%H %s", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((sha, sub)) = line.split_once(' ') {
            if sub == subject {
                return Ok(sha.to_string());
            }
        }
    }
    anyhow::bail!("commit with subject {subject:?} not found on feature")
}

fn todo_entry(id: &str, action: bamboo_protocol::RebaseAction) -> bamboo_protocol::RebaseTodoEntry {
    bamboo_protocol::RebaseTodoEntry {
        id: id.to_string(),
        action,
        message: None,
    }
}

async fn start_rebase(
    session: &std::sync::Arc<bamboo_engine::session::Session>,
    onto: &str,
    todo: Vec<bamboo_protocol::RebaseTodoEntry>,
) -> anyhow::Result<bamboo_protocol::RebaseResult> {
    set_editor_exe();
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::StartRebase {
            onto: onto.to_string(),
            branch: Some("feature".to_string()),
            todo,
        })
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => Ok(r),
        bamboo_engine::session::WriteOutcome::Output(o) => {
            anyhow::bail!("expected a rebase result, got output {o:?}")
        }
    }
}

/// `onto`: the conflict-free tests rebase onto `c2` (the merge base, an
/// in-place replay); the conflict tests rebase onto `main` (m1 conflicts
/// with f1 on shared.txt).
fn feature_todo(dir: &tempfile::TempDir) -> anyhow::Result<Vec<bamboo_protocol::RebaseTodoEntry>> {
    let mut todo = Vec::new();
    for s in ["f1", "f2", "f3"] {
        todo.push(todo_entry(
            &sha_of(dir, s)?,
            bamboo_protocol::RebaseAction::Pick,
        ));
    }
    Ok(todo)
}

#[tokio::test]
async fn rebase_plan_lists_chain_in_replay_order() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let plan = bamboo_engine::gix_read::read_rebase_plan(&session, "main", Some("feature"))?;
    assert_eq!(plan.branch, "feature");
    assert_eq!(plan.onto, "main");
    // Upstream = merge-base(main, feature) = c2.
    assert_eq!(plan.upstream, sha_of(&dir, "c2")?);
    // Replay order: oldest → newest.
    let subjects: Vec<&str> = plan.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["f1", "f2", "f3"]);
    assert_eq!(plan.commits[0].id, sha_of(&dir, "f1")?);
    assert_eq!(plan.commits[0].author, "Rebase Tester");

    // branch: null = the current HEAD branch (feature is checked out).
    let plan = bamboo_engine::gix_read::read_rebase_plan(&session, "main", None)?;
    assert_eq!(plan.branch, "feature");
    assert_eq!(plan.commits.len(), 3);

    // Unknown branch / detached-HEAD-style input errors are structured.
    let err =
        bamboo_engine::gix_read::read_rebase_plan(&session, "main", Some("nope")).unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    Ok(())
}

#[tokio::test]
async fn start_rebase_drop_and_reorder() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // Reorder f3 before f1 and explicitly drop f2.
    let mut todo = feature_todo(&dir)?;
    todo.swap(0, 2);
    todo[1].action = bamboo_protocol::RebaseAction::Drop;
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);
    assert_eq!(result.reason, None);

    // History: c1, c2, f3', f1' (newest first: f1', f3', c2, c1).
    assert_eq!(subjects(&dir, "feature")?, vec!["f1", "f3", "c2", "c1"]);
    // Drop observable in the worktree.
    assert!(!dir.path().join("f2.txt").exists());
    assert!(dir.path().join("f1.txt").exists());
    assert!(dir.path().join("f3.txt").exists());

    // The rebase is journaled as one undoable transaction.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].description.starts_with("StartRebase"));
    Ok(())
}

#[tokio::test]
async fn start_rebase_reword_keeps_content_and_author() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let mut todo = feature_todo(&dir)?;
    todo[0].action = bamboo_protocol::RebaseAction::Reword;
    todo[0].message = Some("renamed one".to_string());
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);

    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "renamed one", "c2", "c1"]);
    // Content untouched by the reword.
    assert_eq!(std::fs::read_to_string(dir.path().join("f1.txt"))?, "f1\n");
    Ok(())
}

#[tokio::test]
async fn start_rebase_squash_folds_with_custom_message() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let mut todo = feature_todo(&dir)?;
    todo[1].action = bamboo_protocol::RebaseAction::Squash;
    todo[1].message = Some("folded f1 and f2".to_string());
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);

    // f1+f2 folded into one commit with the custom message, then f3.
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "folded f1 and f2", "c2", "c1"]);
    // Squash keeps both files' content.
    assert_eq!(std::fs::read_to_string(dir.path().join("f1.txt"))?, "f1\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("f2.txt"))?, "f2\n");
    Ok(())
}

#[tokio::test]
async fn start_rebase_conflict_stop_and_continue() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let f1_sha = sha_of(&dir, "f1")?;

    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    // The conflict at f1 (shared.txt) is a NORMAL outcome, not an error.
    assert!(result.stopped);
    assert_eq!(
        result.reason,
        Some(bamboo_protocol::RebaseStopReason::Conflict)
    );
    assert_eq!(result.stopped_sha.as_deref(), Some(f1_sha.as_str()));
    assert_eq!(result.msgnum, Some(1));
    assert_eq!(result.end, Some(3));

    // The stop is visible through the status read (and HEAD is detached
    // mid-rebase, but on the right branch name).
    let status = bamboo_engine::gix_read::read_rebase_status(&session)?;
    assert_eq!(status.state, bamboo_protocol::RebaseRunState::Stopped);
    assert_eq!(status.branch.as_deref(), Some("feature"));
    assert_eq!(status.backend, Some(bamboo_protocol::RebaseBackend::Merge));
    assert_eq!(status.stopped_sha.as_deref(), Some(f1_sha.as_str()));

    // Continue without resolving is a structured rejection.
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseContinue)
        .await
        .unwrap_err();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // Resolve: take both sides' marker-free resolution and stage it.
    std::fs::write(dir.path().join("shared.txt"), "resolved\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "shared.txt".to_string(),
        ]))
        .await?;
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseContinue)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => {
            assert!(!r.stopped);
        }
        _ => anyhow::bail!("expected a rebase result from rebaseContinue"),
    }

    // Full replay on top of m1 with the resolved content.
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "f1", "m1", "c2", "c1"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt"))?,
        "resolved\n"
    );
    // The status is idle again.
    assert_eq!(
        bamboo_engine::gix_read::read_rebase_status(&session)?.state,
        bamboo_protocol::RebaseRunState::Idle
    );
    Ok(())
}

#[tokio::test]
async fn rebase_skip_drops_the_stopped_commit() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    assert!(result.stopped);

    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseSkip)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => assert!(!r.stopped),
        _ => anyhow::bail!("expected a rebase result from rebaseSkip"),
    }
    // f1 was skipped: no f1.txt, and history is f3, f2 on m1.
    assert!(!dir.path().join("f1.txt").exists());
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "m1", "c2", "c1"]);
    Ok(())
}

#[tokio::test]
async fn rebase_abort_restores_the_pre_rebase_state() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    assert!(result.stopped);

    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseAbort)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => {
            assert!(!r.stopped);
        }
        _ => anyhow::bail!("expected a rebase result from rebaseAbort"),
    }

    // Branch tip, worktree and HEAD fully back to the pre-rebase state.
    let head = std::fs::read_to_string(dir.path().join(".git").join("HEAD"))?;
    assert!(head.trim().ends_with("feature"));
    let tip = Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert_eq!(String::from_utf8_lossy(&tip.stdout).trim(), before_tip);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt"))?,
        "feature-side\n"
    );
    assert_eq!(
        bamboo_engine::gix_read::read_rebase_status(&session)?.state,
        bamboo_protocol::RebaseRunState::Idle
    );

    // The journaled rebase tx stayed consistent: undoing it is a clean
    // restore of the (already restored) state and empties the stack.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let outcome = session.undo.undo(entries[0].id)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));
    assert!(session.undo.list_transactions(10)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn undo_restores_after_completed_rebase() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    // Drop f2, complete the rebase in one shot.
    let mut todo = feature_todo(&dir)?;
    todo[1].action = bamboo_protocol::RebaseAction::Drop;
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);
    assert!(!dir.path().join("f2.txt").exists());

    settle().await;
    // Undo the rebase transaction.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let outcome = session.undo.undo(entries[0].id)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));

    // History, worktree and branch tip fully restored.
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "f1", "c2", "c1"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("f2.txt"))?, "f2\n");
    let tip = Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert_eq!(String::from_utf8_lossy(&tip.stdout).trim(), before_tip);
    let head = std::fs::read_to_string(dir.path().join(".git").join("HEAD"))?;
    assert!(head.trim().ends_with("feature"));
    Ok(())
}

#[tokio::test]
async fn start_rebase_rejected_while_rebase_in_progress() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // First rebase stops at the f1 conflict.
    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    assert!(result.stopped);

    // A second startRebase must be a structured INVALID_ARGUMENT — not a
    // misclassified "success with stopped: true" of a rebase that never
    // ran — and must not disturb the in-flight rebase.
    let epoch_before = session.current_epoch();
    let err = start_rebase(&session, "main", feature_todo(&dir)?)
        .await
        .unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("already in progress"));
    // Error paths still invalidate: the rebase command bumped the epoch.
    assert!(session.current_epoch() > epoch_before);

    // The in-flight rebase is untouched (still stopped, same sha) and its
    // sidecars were not overwritten.
    let status = bamboo_engine::gix_read::read_rebase_status(&session)?;
    assert_eq!(status.state, bamboo_protocol::RebaseRunState::Stopped);
    assert_eq!(
        status.stopped_sha.as_deref(),
        Some(sha_of(&dir, "f1")?.as_str())
    );

    // The in-flight rebase remains fully operable: abort restores the
    // pre-rebase state and its journaled undo tx survives (the binding
    // was not hijacked by the failed second start).
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseAbort)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => assert!(!r.stopped),
        _ => anyhow::bail!("expected a rebase result from rebaseAbort"),
    }
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "f1", "c2", "c1"]);
    let entries = session.undo.list_transactions(10)?;
    // Two journaled txs: the in-flight rebase's, plus the REFUSED start
    // (journal failed txs too, newest first). The rebase tx is the oldest.
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        session.undo.undo(entries[1].id)?,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn start_rebase_rejected_for_externally_started_rebase() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // Simulate an externally-started rebase (only git's on-disk state dir;
    // the engine has no tx binding for it).
    let merge_dir = dir.path().join(".git").join("rebase-merge");
    std::fs::create_dir_all(&merge_dir)?;
    std::fs::write(merge_dir.join("head-name"), "refs/heads/feature")?;
    std::fs::write(merge_dir.join("onto"), sha_of(&dir, "c2")?)?;

    let err = start_rebase(&session, "main", feature_todo(&dir)?)
        .await
        .unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("already in progress"));

    // No sidecars were written for the refused start (a stale todo/marker
    // would otherwise leak into the external rebase run).
    assert!(!dir
        .path()
        .join(".git")
        .join("bamboo")
        .join("rebase")
        .exists());
    Ok(())
}

#[tokio::test]
async fn undo_works_after_conflict_resolution_and_continue() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    // Conflict stop at f1.
    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    assert!(result.stopped);

    // Resolve through a JOURNALED Stage: its post snapshot freezes the
    // stopped-rebase state — the exact undo-safety hazard this test pins.
    std::fs::write(dir.path().join("shared.txt"), "resolved\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "shared.txt".to_string(),
        ]))
        .await?;
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseContinue)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => assert!(!r.stopped),
        _ => anyhow::bail!("expected a rebase result from rebaseContinue"),
    }
    settle().await;

    // Undo the StartRebase tx (the oldest entry; list is newest-first):
    // the safety check must pass because the continue refreshed the
    // frozen post snapshots, and the restore must return to pre-rebase
    // state (this is exactly the flow that used to block undo forever).
    let entries = session.undo.list_transactions(10)?;
    let rebase_tx = entries.last().expect("journaled rebase tx").id;
    let outcome = session.undo.undo(rebase_tx)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "f1", "c2", "c1"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt"))?,
        "feature-side\n"
    );
    let tip = Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert_eq!(String::from_utf8_lossy(&tip.stdout).trim(), before_tip);
    Ok(())
}

#[tokio::test]
async fn start_rebase_with_nothing_to_do_is_structured_success() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    // Rebase feature onto its own tip: nothing to replay. git would
    // refuse with "nothing to do" — the engine reports a structured
    // completed result instead (never stopped, never an error).
    let result = start_rebase(&session, &sha_of(&dir, "f3")?, feature_todo(&dir)?).await?;
    assert!(!result.stopped);
    assert_eq!(result.reason, None);
    assert_eq!(sha_of(&dir, "f3")?, before_tip);
    // Nothing on disk: no rebase dir, no sidecars, no journal noise.
    assert!(!dir.path().join(".git").join("rebase-merge").exists());
    assert!(!dir
        .path()
        .join(".git")
        .join("bamboo")
        .join("rebase")
        .exists());
    Ok(())
}

#[tokio::test]
async fn rebase_chain_excludes_merge_commits() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // Merge a side branch into feature: feature's tip becomes a merge
    // commit (parents: f3 + s1).
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()?;
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    };
    git(&["branch", "side", &sha_of(&dir, "f2")?])?;
    git(&["checkout", "-q", "side"])?;
    std::fs::write(dir.path().join("s1.txt"), "s1\n")?;
    git(&["add", "."])?;
    git(&[
        "commit",
        "-q",
        "-m",
        "s1",
        "--author",
        "Rebase Tester <rebase@example.com>",
    ])?;
    git(&["checkout", "-q", "feature"])?;
    git(&["merge", "-q", "--no-ff", "side", "-m", "merge1"])?;

    // The plan lists the linear chain only: merge commits are excluded
    // (git's own todo never contains them; a `pick <merge>` line would
    // make git refuse the whole todo).
    let plan = bamboo_engine::gix_read::read_rebase_plan(&session, "main", Some("feature"))?;
    let mut planned: Vec<String> = plan.commits.iter().map(|c| c.subject.clone()).collect();
    planned.sort();
    assert_eq!(planned, vec!["f1", "f2", "f3", "s1"]);

    // And the chain actually rebases: replay in place onto the merge
    // base, completing without a stop.
    let todo: Vec<bamboo_protocol::RebaseTodoEntry> = plan
        .commits
        .iter()
        .map(|c| bamboo_protocol::RebaseTodoEntry {
            id: c.id.clone(),
            action: bamboo_protocol::RebaseAction::Pick,
            message: None,
        })
        .collect();
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);
    let subjects = subjects(&dir, "feature")?;
    assert!(!subjects.contains(&"merge1".to_string()));
    assert!(subjects.contains(&"s1".to_string()));
    Ok(())
}

#[tokio::test]
async fn undo_of_completed_rebase_preserves_dirty_worktree() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    // Drop f2, complete the rebase in one shot.
    let mut todo = feature_todo(&dir)?;
    todo[1].action = bamboo_protocol::RebaseAction::Drop;
    let result = start_rebase(&session, &sha_of(&dir, "c2")?, todo).await?;
    assert!(!result.stopped);
    settle().await;

    // An uncommitted user change made AFTER the rebase completed.
    std::fs::write(dir.path().join("f1.txt"), "f1 with local edit\n")?;

    let entries = session.undo.list_transactions(10)?;
    let outcome = session.undo.undo(entries[0].id)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));

    // Refs/history/branch tip restored ...
    let subjects = subjects(&dir, "feature")?;
    assert_eq!(subjects, vec!["f3", "f2", "f1", "c2", "c1"]);
    let tip = Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert_eq!(String::from_utf8_lossy(&tip.stdout).trim(), before_tip);
    // ... but the uncommitted local edit survives (undo skips the worktree
    // reset when the worktree was dirty).
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f1.txt"))?,
        "f1 with local edit\n"
    );
    Ok(())
}

#[tokio::test]
async fn undo_of_stopped_rebase_aborts_and_restores() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;
    let before_tip = sha_of(&dir, "f3")?;

    let result = start_rebase(&session, "main", feature_todo(&dir)?).await?;
    assert!(result.stopped);

    // Undo the in-flight rebase directly: must abort the rebase on disk
    // and restore the pre-rebase branch/worktree.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let outcome = session.undo.undo(entries[0].id)?;
    assert!(matches!(
        outcome,
        bamboo_engine::undo::UndoOutcome::Restored { .. }
    ));

    assert!(!dir.path().join(".git").join("rebase-merge").exists());
    let head = std::fs::read_to_string(dir.path().join(".git").join("HEAD"))?;
    assert!(head.trim().ends_with("feature"));
    let tip = Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert_eq!(String::from_utf8_lossy(&tip.stdout).trim(), before_tip);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt"))?,
        "feature-side\n"
    );
    Ok(())
}

#[tokio::test]
async fn get_rebase_status_detects_external_rebase() -> anyhow::Result<()> {
    // Even a pure status read + continue flow can invoke the message
    // editor (git opens it when committing the conflict resolution), so
    // the editor exe must point at the real engine binary.
    set_editor_exe();
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // Idle first.
    assert_eq!(
        bamboo_engine::gix_read::read_rebase_status(&session)?.state,
        bamboo_protocol::RebaseRunState::Idle
    );

    // An EXTERNALLY started rebase (merge backend) stopping at the same
    // conflict: the engine must still see it (there is no engine-side
    // state to compare with — the read is pure on-disk detection).
    let out = Command::new("git")
        .args(["rebase", "main", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert!(
        !out.status.success(),
        "expected the rebase to stop at the conflict"
    );

    let status = bamboo_engine::gix_read::read_rebase_status(&session)?;
    assert_eq!(status.state, bamboo_protocol::RebaseRunState::Stopped);
    assert_eq!(status.backend, Some(bamboo_protocol::RebaseBackend::Merge));
    assert_eq!(status.branch.as_deref(), Some("feature"));
    assert_eq!(status.msgnum, Some(1));

    // The engine can drive it to completion (continue with resolution).
    std::fs::write(dir.path().join("shared.txt"), "resolved\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "shared.txt".to_string(),
        ]))
        .await?;
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseContinue)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => assert!(!r.stopped),
        _ => anyhow::bail!("expected a rebase result from rebaseContinue"),
    }
    assert_eq!(
        bamboo_engine::gix_read::read_rebase_status(&session)?.state,
        bamboo_protocol::RebaseRunState::Idle
    );
    Ok(())
}

#[tokio::test]
async fn rebase_continue_is_idempotent_when_idle() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // No rebase in progress: continue reports the success state (racing
    // completion / double-click), skip is a structured rejection.
    let outcome = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseContinue)
        .await?;
    match outcome {
        bamboo_engine::session::WriteOutcome::Rebase(r) => {
            assert!(!r.stopped);
        }
        _ => anyhow::bail!("expected a rebase result from rebaseContinue"),
    }
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseSkip)
        .await
        .unwrap_err();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::RebaseAbort)
        .await
        .unwrap_err();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    Ok(())
}

#[tokio::test]
async fn start_rebase_validation_rejects_stale_todo() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let mut todo = feature_todo(&dir)?;
    // A stale snapshot: one commit is silently missing (no explicit drop).
    todo.pop();
    let onto = sha_of(&dir, "c2")?;
    let before_tip = sha_of(&dir, "f3")?;
    let err = start_rebase(&session, &onto, todo).await.unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("stale plan"));

    // A made-up id: same freshness rejection.
    let unknown = "0123456789abcdef0123456789abcdef01234567".to_string();
    let err = start_rebase(
        &session,
        &sha_of(&dir, "c2")?,
        vec![todo_entry(&unknown, bamboo_protocol::RebaseAction::Pick)],
    )
    .await
    .unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // Nothing was mutated (no rebase dir, branch tip untouched).
    assert!(!dir.path().join(".git").join("rebase-merge").exists());
    assert_eq!(before_tip, sha_of(&dir, "f3")?);
    Ok(())
}

#[tokio::test]
async fn start_rebase_validation_rejects_bad_entries() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    // Rebase in place (onto the merge base): the entry-level validation
    // fires before any conflict could.
    let onto = sha_of(&dir, "c2")?;

    async fn expect_invalid(
        session: &std::sync::Arc<bamboo_engine::session::Session>,
        onto: &str,
        todo: Vec<bamboo_protocol::RebaseTodoEntry>,
        what: &str,
    ) {
        let err = start_rebase(session, onto, todo).await.unwrap_err();
        let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
        assert_eq!(
            err.code,
            bamboo_protocol::BambooError::INVALID_ARGUMENT,
            "{what}: {err}"
        );
    }

    // Phase-2 actions rejected.
    let mut todo = feature_todo(&dir)?;
    todo[0].action = bamboo_protocol::RebaseAction::Fixup;
    expect_invalid(&session, &onto, todo, "fixup").await;

    // Message on a pick.
    let mut todo = feature_todo(&dir)?;
    todo[0].message = Some("not allowed".to_string());
    expect_invalid(&session, &onto, todo, "message on pick").await;

    // Multi-line message.
    let mut todo = feature_todo(&dir)?;
    todo[0].action = bamboo_protocol::RebaseAction::Reword;
    todo[0].message = Some("line one\nline two".to_string());
    expect_invalid(&session, &onto, todo, "control chars in message").await;

    // Duplicate ids.
    let mut todo = feature_todo(&dir)?;
    todo[2].id = todo[1].id.clone();
    expect_invalid(&session, &onto, todo, "duplicate ids").await;

    // Option-like onto / branch names never reach a git argv.
    set_editor_exe();
    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::StartRebase {
            onto: "--upload-pack=x".to_string(),
            branch: None,
            todo: feature_todo(&dir)?,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    Ok(())
}

#[tokio::test]
async fn start_rebase_rejects_too_many_entries() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let mut todo = feature_todo(&dir)?;
    for i in 0..600 {
        // Distinct unknown-but-valid-hex ids: only the entry-count cap can
        // reject this (freshness would also complain, but the cap fires
        // first).
        todo.push(todo_entry(
            &format!("{i:040x}"),
            bamboo_protocol::RebaseAction::Drop,
        ));
    }
    let err = start_rebase(&session, &sha_of(&dir, "c2")?, todo)
        .await
        .unwrap_err();
    let err = err.downcast::<bamboo_protocol::BambooError>().unwrap();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("exceeds"));
    Ok(())
}

#[tokio::test]
async fn start_rebase_rejects_detached_head_without_branch() -> anyhow::Result<()> {
    let dir = rebase_repo()?;
    let session = open_session(&dir).await?;

    let out = Command::new("git")
        .args(["checkout", "-q", "--detach", "feature"])
        .current_dir(dir.path())
        .output()?;
    assert!(out.status.success());

    let err = session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::StartRebase {
            onto: "main".to_string(),
            branch: None,
            todo: feature_todo(&dir)?,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(err.message.contains("detached"));
    Ok(())
}

/// The fs watcher must survive a repo root rm -rf + recreate: inotify
/// watches are inode-based, so the session has to detect the root
/// deletion, wait for the paths to reappear, and re-arm — mutations in
/// the NEW directory must produce notifications again.
#[tokio::test]
async fn watcher_rearms_after_repo_recreate() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo)?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
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
    git(&["init", "-q", "-b", "main"])?;
    std::fs::write(repo.join("a.txt"), "a\n")?;
    git(&["add", "."])?;
    git(&["commit", "-q", "-m", "c1"])?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _session = bamboo_engine::session::Session::open(&repo, tx).await?;

    // Sanity: the watcher is alive — a worktree write produces a
    // notification. Retried rather than written once, because a backend is
    // not necessarily armed the instant `open` returns (macOS starts its
    // FSEvents stream on its own thread) and a write landing in that window
    // is simply lost. A watcher that is dead — or one dropping events because
    // it cannot match their path form — never delivers however often we write,
    // so the retry cannot mask the failure this check exists to catch.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut alive = false;
    loop {
        std::fs::write(repo.join("probe.txt"), "1\n")?;
        match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
            Ok(Some(_)) => {
                alive = true;
                break;
            }
            Ok(None) => anyhow::bail!("notification channel closed"),
            Err(_) => {}
        }
        if std::time::Instant::now() > deadline {
            break;
        }
    }
    assert!(alive, "no notification before recreate");

    // rm -rf + recreate: new inodes; the old watches are dead.
    std::fs::remove_dir_all(&repo)?;
    std::fs::create_dir(&repo)?;
    git(&["init", "-q", "-b", "main"])?;
    std::fs::write(repo.join("b.txt"), "b\n")?;
    git(&["add", "."])?;
    git(&["commit", "-q", "-m", "c2"])?;

    // Drain everything produced by the rm -rf deletion classification and
    // the recreate, so the probe assertion below can only pass on a LIVE
    // event from the re-armed watcher.
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }

    // The re-arm thread should fire a conservative refresh burst on the
    // ORIGINAL channel once the repo reappears, and mutations in the new
    // directory must produce notifications again. Retry-write probe2 until
    // an event arrives (the recreate's git ops happened while the watcher
    // was dead — only re-armed live events count).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut got = false;
    loop {
        std::fs::write(repo.join("probe2.txt"), "2\n")?;
        match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
            Ok(Some(_)) => {
                got = true;
                break;
            }
            Ok(None) => anyhow::bail!("notification channel closed"),
            Err(_) => {}
        }
        if std::time::Instant::now() > deadline {
            break;
        }
    }
    assert!(got, "watcher did not re-arm after repo recreate");
    Ok(())
}

/// The undo snapshots, journal, and audit log must self-heal when the
/// .git/bamboo tree vanishes under a live session (e.g. the repo dir was
/// deleted and recreated — UndoEngine::new only creates the dirs once).
#[tokio::test]
async fn undo_self_heals_after_bamboo_dir_removed() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::remove_dir_all(dir.path().join(".git").join("bamboo"))?;

    std::fs::write(dir.path().join("new.txt"), "new\n")?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;
    session
        .enqueue_write(bamboo_engine::write_queue::WriteCommand::Commit {
            message: "healed".to_string(),
            amend: false,
        })
        .await?;

    settle().await;
    let viewport = bamboo_protocol::GraphViewport {
        offset: 0,
        limit: 10,
        anchor_commit: None,
        epoch: 0,
    };
    let page = bamboo_engine::gix_read::read_graph_page(&session, &viewport)?;
    assert_eq!(page.commits.len(), 4);
    assert!(page.commits[0].message.contains("healed"));

    // The journal and audit log were recreated in the healed tree.
    let entries = session.undo.list_transactions(10)?;
    assert!(
        entries
            .iter()
            .any(|e| e.description.contains("healed") || e.description.contains("Stage")),
        "expected journaled transactions after self-heal, got {:?}",
        entries.iter().map(|e| &e.description).collect::<Vec<_>>()
    );
    assert!(dir
        .path()
        .join(".git")
        .join("bamboo")
        .join("audit.log")
        .exists());
    Ok(())
}

// ---------------------------------------------------------------------------
// Blame (getBlame)
// ---------------------------------------------------------------------------

/// Commit the staged changes in `dir` as `author` at a fixed timestamp and
/// return the new HEAD's sha. Identity comes from the environment (all four
/// variables), so the fixture repo needs no `user.*` config.
fn blame_commit(
    dir: &std::path::Path,
    msg: &str,
    author: &str,
    date: &str,
) -> anyhow::Result<String> {
    let email = format!("{}@example.com", author.to_lowercase());
    let out = Command::new("git")
        .args(["commit", "-q", "-m", msg])
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", author)
        .env("GIT_AUTHOR_EMAIL", &email)
        .env("GIT_COMMITTER_NAME", author)
        .env("GIT_COMMITTER_EMAIL", &email)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit {msg:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()?;
    Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
}

/// The blame fixture: `f.txt` evolving over three commits, each by a
/// different author at a distinct timestamp so attribution is unambiguous.
///
/// ```text
///   one   (Ann, 2026-01-01)  f.txt = a   b   c
///   two   (Bob, 2026-01-02)  f.txt = a   B2  c   d   e     line 2 rewritten, 4-5 appended
///   three (Cyd, 2026-01-03)  f.txt = a   B2  c   d   E5  f  line 5 rewritten, 6 appended
/// ```
///
/// Expected HEAD attribution, 0-based line → introducing commit:
/// `0→one, 1→two, 2→one, 3→two, 4→three, 5→three`.
struct BlameFixture {
    dir: tempfile::TempDir,
    one: String,
    two: String,
    three: String,
}

impl BlameFixture {
    /// Blame `f.txt` at HEAD through the engine's read path.
    fn blame(
        &self,
        session: &std::sync::Arc<bamboo_engine::session::Session>,
    ) -> anyhow::Result<bamboo_protocol::GetBlameResult> {
        bamboo_engine::gix_read::read_blame(
            session,
            &bamboo_protocol::GetBlame {
                path: "f.txt".to_string(),
                rev: None,
                contents: None,
            },
        )
    }

    /// The committed content of `f.txt` (what HEAD blames).
    fn head_content(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(self.dir.path().join("f.txt"))?)
    }
}

fn blame_repo() -> anyhow::Result<BlameFixture> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
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

    git(&["init", "-q", "-b", "main"])?;
    // Pin line endings: a CRLF conversion on checkout would change the
    // blamed bytes and with them the expected line count.
    git(&["config", "core.autocrlf", "false"])?;

    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n")?;
    git(&["add", "."])?;
    let one = blame_commit(dir.path(), "one", "Ann", "2026-01-01T00:00:00Z")?;

    std::fs::write(dir.path().join("f.txt"), "a\nB2\nc\nd\ne\n")?;
    git(&["add", "."])?;
    let two = blame_commit(dir.path(), "two", "Bob", "2026-01-02T00:00:00Z")?;

    std::fs::write(dir.path().join("f.txt"), "a\nB2\nc\nd\nE5\nf\n")?;
    git(&["add", "."])?;
    let three = blame_commit(dir.path(), "three", "Cyd", "2026-01-03T00:00:00Z")?;

    Ok(BlameFixture {
        dir,
        one,
        two,
        three,
    })
}

/// Expand hunks into one commit id per line — the invariant clients render
/// from, and the only shape both blame implementations must agree on.
fn blamed_lines(result: &bamboo_protocol::GetBlameResult) -> Vec<String> {
    let mut out = Vec::new();
    for e in &result.entries {
        for _ in 0..e.len {
            out.push(e.commit.id.clone());
        }
    }
    out
}

/// Assert the hunks tile the file exactly: ascending, contiguous, no empty
/// hunk, and covering all `line_count` lines. A gap would silently mislabel
/// every line after it, which is why the engine checks this too.
fn assert_tiles(result: &bamboo_protocol::GetBlameResult, line_count: u32) {
    let mut next = 0;
    for e in &result.entries {
        assert_eq!(
            e.start_line, next,
            "hunks must be contiguous and ascending: {:?}",
            result.entries
        );
        assert!(e.len >= 1, "empty hunk at line {}", e.start_line);
        next += e.len;
    }
    assert_eq!(next, line_count, "hunks must cover every line");
}

/// The commit metadata of the hunk covering `line` (0-based).
fn commit_at_line(
    result: &bamboo_protocol::GetBlameResult,
    line: u32,
) -> &bamboo_protocol::BlameCommit {
    let entry = result
        .entries
        .iter()
        .find(|e| line >= e.start_line && line < e.start_line + e.len)
        .unwrap_or_else(|| panic!("no blame entry covers line {line}: {:?}", result.entries));
    &entry.commit
}

#[tokio::test]
async fn blame_attributes_each_line_to_its_introducing_commit() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;

    let result = fx.blame(&session)?;
    assert_eq!(result.via, bamboo_protocol::BlameVia::Gix);
    assert_eq!(
        result.rev, fx.three,
        "rev must be the resolved HEAD commit id clients cache on"
    );
    assert_tiles(&result, 6);
    assert_eq!(
        blamed_lines(&result),
        vec![
            fx.one.clone(),
            fx.two.clone(),
            fx.one.clone(),
            fx.two.clone(),
            fx.three.clone(),
            fx.three.clone()
        ]
    );

    // Metadata is joined engine-side: one round trip must be enough to
    // render gutter text AND the hover card, so no field may be empty.
    let two = commit_at_line(&result, 1);
    assert_eq!(two.id, fx.two);
    assert_eq!(two.author_name, "Bob");
    assert_eq!(two.author_email, "bob@example.com");
    assert_eq!(two.summary, "two");
    assert!(two.author_time > 0, "author time must be joined");
    assert_eq!(
        two.committer_time, two.author_time,
        "the fixture commits with the same timestamp, so both must match"
    );

    // Distinct commits keep distinct timestamps (the fixture's ordering).
    assert!(
        commit_at_line(&result, 0).author_time < commit_at_line(&result, 1).author_time
            && commit_at_line(&result, 1).author_time < commit_at_line(&result, 4).author_time,
        "author times must follow history order"
    );

    // Adjacent lines from one commit collapse into a single hunk — the
    // response is hunks, not one record per line (a 5k-line file would
    // otherwise ship 5k entries).
    assert!(
        result.entries.len() < 6,
        "adjacent same-commit lines must merge, got {:?}",
        result.entries
    );
    Ok(())
}

#[tokio::test]
async fn blame_at_an_explicit_revision_stops_at_that_revision() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;

    // `f.txt` as of commit two: 5 lines, and the line three rewrote is
    // still Bob's.
    let by_sha = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: Some(fx.two.clone()),
            contents: None,
        },
    )?;
    assert_eq!(by_sha.rev, fx.two);
    assert_tiles(&by_sha, 5);
    assert_eq!(
        blamed_lines(&by_sha),
        vec![
            fx.one.clone(),
            fx.two.clone(),
            fx.one.clone(),
            fx.two.clone(),
            fx.two.clone()
        ]
    );

    // Revspecs with `~`/`^` must be accepted, not rejected by the input
    // validation: annotate-previous walks the chain with `<sha>^`.
    let by_revspec = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: Some("HEAD~1".to_string()),
            contents: None,
        },
    )?;
    assert_eq!(by_revspec.rev, fx.two);
    assert_eq!(blamed_lines(&by_revspec), blamed_lines(&by_sha));
    Ok(())
}

#[tokio::test]
async fn blame_follows_renames_and_reports_the_source_path() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
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
    git(&["init", "-q", "-b", "main"])?;
    git(&["config", "core.autocrlf", "false"])?;

    std::fs::write(dir.path().join("old.txt"), "x\ny\nz\n")?;
    git(&["add", "."])?;
    let created = blame_commit(dir.path(), "created", "Ann", "2026-01-01T00:00:00Z")?;

    // A pure rename: no content change, so similarity is 100% and rename
    // tracking must see it. Without `rewrites` every line would be pinned
    // to this commit and the file's real history would be lost.
    git(&["mv", "old.txt", "new.txt"])?;
    let renamed = blame_commit(dir.path(), "renamed", "Bob", "2026-01-02T00:00:00Z")?;
    assert_ne!(created, renamed);

    let session = open_session(&dir).await?;
    let result = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "new.txt".to_string(),
            rev: None,
            contents: None,
        },
    )?;
    assert_eq!(result.via, bamboo_protocol::BlameVia::Gix);
    assert_tiles(&result, 3);
    assert_eq!(
        blamed_lines(&result),
        vec![created.clone(); 3],
        "all lines predate the rename, so all belong to the creating commit"
    );
    for e in &result.entries {
        assert_eq!(
            e.source_path.as_deref(),
            Some("old.txt"),
            "a rename must surface the path the lines came from"
        );
    }

    // `git blame` follows renames by default too, so the fallback must report
    // the SAME source path — otherwise annotate-previous would open a
    // different file depending on which implementation happened to answer.
    let forced = bamboo_engine::gix_read::read_blame_with(
        &session,
        &bamboo_protocol::GetBlame {
            path: "new.txt".to_string(),
            rev: None,
            contents: None,
        },
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_eq!(forced.via, bamboo_protocol::BlameVia::GitPorcelain);
    assert_eq!(blamed_lines(&forced), blamed_lines(&result));
    assert_eq!(
        forced
            .entries
            .iter()
            .map(|e| e.source_path.as_deref())
            .collect::<Vec<_>>(),
        result
            .entries
            .iter()
            .map(|e| e.source_path.as_deref())
            .collect::<Vec<_>>(),
        "rename attribution must not depend on which implementation ran"
    );
    Ok(())
}

#[tokio::test]
async fn blame_of_a_dirty_buffer_marks_uncommitted_lines() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;
    let committed = fx.head_content()?;

    // The unsaved buffer inserts one line at the top: that line has no
    // commit, the rest keep their attribution (shifted by one).
    let buffer = format!("TOP\n{committed}");
    let result = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: Some(buffer.clone()),
        },
    )?;
    assert_eq!(result.via, bamboo_protocol::BlameVia::Gix);
    assert_tiles(&result, 7);

    let uncommitted = commit_at_line(&result, 0);
    assert_eq!(
        uncommitted.id, "0000000000000000000000000000000000000000",
        "buffer-only lines attribute to the null id"
    );
    assert_eq!(uncommitted.author_name, "Not Committed Yet");
    assert_eq!(uncommitted.author_time, 0);
    assert_eq!(
        blamed_lines(&result)[1..],
        blamed_lines(&fx.blame(&session)?)[..],
        "the committed lines keep their attribution"
    );

    // Blame is a read: the buffer must not reach the disk, and the worktree
    // must still hold the committed content.
    assert_eq!(fx.head_content()?, committed);
    Ok(())
}

#[tokio::test]
async fn blame_without_contents_annotates_the_worktree_file() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;
    let null_id = "0".repeat(40);

    // One line appended on disk but not committed. With no `contents` the
    // blamed file is the one the user is looking at: blaming HEAD's blob
    // instead would return 6 tiles for a 7-line buffer, i.e. every
    // annotation below the change on the wrong line.
    std::fs::write(fx.dir.path().join("f.txt"), "a\nB2\nc\nd\nE5\nf\nNEW\n")?;
    let result = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: None,
        },
    )?;
    assert_eq!(result.via, bamboo_protocol::BlameVia::Gix);
    assert_tiles(&result, 7);
    assert_eq!(
        blamed_lines(&result),
        vec![
            fx.one.clone(),
            fx.two.clone(),
            fx.one.clone(),
            fx.two.clone(),
            fx.three.clone(),
            fx.three.clone(),
            null_id.clone(),
        ]
    );
    assert_eq!(commit_at_line(&result, 6).author_name, "Not Committed Yet");

    // An explicit revision is a history question, not a worktree one: the
    // file as of that commit, dirty worktree notwithstanding.
    let at_head = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: Some("HEAD".to_string()),
            contents: None,
        },
    )?;
    assert_tiles(&at_head, 6);
    assert_eq!(at_head.rev, fx.three);
    assert!(
        !blamed_lines(&at_head).contains(&null_id),
        "nothing is uncommitted in a revision's own blob"
    );

    // The porcelain fallback reads the worktree file itself, so it must land
    // on the same attribution for both questions.
    for rev in [None, Some("HEAD".to_string())] {
        let forced = bamboo_engine::gix_read::read_blame_with(
            &session,
            &bamboo_protocol::GetBlame {
                path: "f.txt".to_string(),
                rev: rev.clone(),
                contents: None,
            },
            bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
        )?;
        assert_eq!(forced.via, bamboo_protocol::BlameVia::GitPorcelain);
        let expected = if rev.is_none() { &result } else { &at_head };
        assert_eq!(
            blamed_lines(&forced),
            blamed_lines(expected),
            "the fallback must not attribute lines differently (rev = {rev:?})"
        );
    }
    Ok(())
}

#[tokio::test]
async fn blame_of_an_unsaved_new_file_is_all_uncommitted() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;
    let null_id = "0".repeat(40);

    // `scratch.txt` exists only as an editor buffer: there is no committed
    // version to walk, so every line is "not committed yet" rather than an
    // error — and rather than the two implementations disagreeing, since
    // neither git nor gix blames a path that is absent from the revision.
    let buffer = "one\ntwo\nthree"; // no trailing newline on purpose
    let req = |contents: Option<&str>| bamboo_protocol::GetBlame {
        path: "scratch.txt".to_string(),
        rev: None,
        contents: contents.map(str::to_string),
    };
    let result = bamboo_engine::gix_read::read_blame(&session, &req(Some(buffer)))?;
    assert_eq!(result.rev, fx.three);
    assert_tiles(&result, 3);
    assert_eq!(
        result.entries.len(),
        1,
        "a uniformly uncommitted file is one hunk"
    );
    assert_eq!(blamed_lines(&result), vec![null_id.clone(); 3]);
    assert_eq!(result.entries[0].commit.author_name, "Not Committed Yet");
    assert_eq!(result.entries[0].commit.summary, "Uncommitted changes");

    // Same answer through the porcelain backend: the wire shape must not
    // depend on which implementation would have run.
    let forced = bamboo_engine::gix_read::read_blame_with(
        &session,
        &req(Some(buffer)),
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_eq!(blamed_lines(&forced), blamed_lines(&result));
    assert_eq!(
        forced.via,
        bamboo_protocol::BlameVia::GitPorcelain,
        "the absent-path early return must still name the backend the caller asked for"
    );

    // An empty buffer has no lines, so no hunks (a zero-length hunk would
    // break the tiling invariant the client renders from).
    let empty = bamboo_engine::gix_read::read_blame(&session, &req(Some("")))?;
    assert!(empty.entries.is_empty());
    assert_eq!(empty.rev, fx.three);
    Ok(())
}

#[tokio::test]
async fn blame_porcelain_fallback_matches_the_gix_path() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;
    let req = bamboo_protocol::GetBlame {
        path: "f.txt".to_string(),
        rev: None,
        contents: None,
    };

    let gix = bamboo_engine::gix_read::read_blame(&session, &req)?;
    let porcelain = bamboo_engine::gix_read::read_blame_with(
        &session,
        &req,
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;

    // `via` is on the wire precisely so a silently degraded read stays
    // visible; the two paths must not claim to be each other.
    assert_eq!(gix.via, bamboo_protocol::BlameVia::Gix);
    assert_eq!(porcelain.via, bamboo_protocol::BlameVia::GitPorcelain);
    assert_eq!(porcelain.rev, gix.rev);

    assert_tiles(&porcelain, 6);
    assert_eq!(
        blamed_lines(&porcelain),
        blamed_lines(&gix),
        "line attribution must not depend on which implementation ran"
    );
    assert_eq!(
        commit_at_line(&porcelain, 1).author_name,
        commit_at_line(&gix, 1).author_name,
        "metadata comes from the same engine-side join, so it must agree"
    );
    // Porcelain emits one record per LINE; the parser merges them back into
    // hunks, otherwise the client would see a different shape per backend.
    assert!(
        porcelain.entries.len() < 6,
        "porcelain records must be merged into hunks, got {:?}",
        porcelain.entries
    );

    // The dirty-buffer path has its own fallback shape (`--contents=-` fed
    // over stdin), so it is compared separately.
    let buffer = format!("TOP\n{}", fx.head_content()?);
    let dirty = |contents: Option<String>| bamboo_protocol::GetBlame {
        path: "f.txt".to_string(),
        rev: None,
        contents,
    };
    let gix_dirty = bamboo_engine::gix_read::read_blame(&session, &dirty(Some(buffer.clone())))?;
    let porcelain_dirty = bamboo_engine::gix_read::read_blame_with(
        &session,
        &dirty(Some(buffer)),
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_tiles(&porcelain_dirty, 7);
    assert_eq!(blamed_lines(&porcelain_dirty), blamed_lines(&gix_dirty));
    assert_eq!(
        commit_at_line(&porcelain_dirty, 0).id,
        "0000000000000000000000000000000000000000"
    );
    assert_eq!(
        commit_at_line(&porcelain_dirty, 0).author_name,
        commit_at_line(&gix_dirty, 0).author_name,
        "the null id's metadata is synthesized engine-side, so both paths agree"
    );
    Ok(())
}

#[tokio::test]
async fn blame_rejects_invalid_arguments_as_structured_errors() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;

    let invalid_arg = |r: anyhow::Result<bamboo_protocol::GetBlameResult>| {
        let err = r.expect_err("expected the request to be rejected");
        let bamboo = err.downcast_ref::<bamboo_protocol::BambooError>().expect(
            "input errors must stay structured (INVALID_ARGUMENT), not degrade to git errors",
        );
        assert_eq!(bamboo.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    };

    // Option-like paths are git's own argument parser hazard.
    invalid_arg(bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "--upload-pack=x".to_string(),
            rev: None,
            contents: None,
        },
    ));
    invalid_arg(bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: String::new(),
            rev: None,
            contents: None,
        },
    ));

    // Path traversal: blame is the read that opens a worktree FILE from this
    // argument (every other one hands it to git, which refuses to leave the
    // repository), so rooted paths and `..` are rejected before anything is
    // joined onto the worktree root. `/etc/passwd` is the interesting case:
    // absolute on Unix, but on Windows it carries no drive prefix and so is
    // NOT `is_absolute` — while `join` still discards everything after the
    // prefix and escapes to `C:\etc\passwd`. Rejecting by component covers
    // both, which is why the check is not `is_absolute`.
    for escaping in ["/etc/passwd", "../outside.txt", "sub/../../outside.txt"] {
        invalid_arg(bamboo_engine::gix_read::read_blame(
            &session,
            &bamboo_protocol::GetBlame {
                path: escaping.to_string(),
                rev: None,
                contents: None,
            },
        ));
    }
    // The same escape spelled the Windows way: rooted, no prefix. On Unix a
    // backslash is an ordinary filename character, so this only means what it
    // says where it can hurt.
    #[cfg(windows)]
    invalid_arg(bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "\\etc\\passwd".to_string(),
            rev: None,
            contents: None,
        },
    ));
    invalid_arg(bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: Some("--all".to_string()),
            contents: None,
        },
    ));

    // An oversized buffer is rejected in band with a save-first hint: the
    // 16 MiB framing cap would otherwise close the connection instead.
    let too_big = "x".repeat(8 * 1024 * 1024 + 1);
    let err = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: Some(too_big),
        },
    )
    .expect_err("oversized contents must be rejected");
    let bamboo = err
        .downcast_ref::<bamboo_protocol::BambooError>()
        .expect("the size cap must be a structured error");
    assert_eq!(bamboo.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);
    assert!(
        bamboo.message.contains("save"),
        "the message must tell the client what to do: {}",
        bamboo.message
    );

    // A worktree file has no wire framing to bound it at all, so the read is
    // capped the same way: without it one request would make the engine slurp
    // an arbitrarily large file and hand gix-blame a second copy of it.
    std::fs::write(
        fx.dir.path().join("huge.log"),
        vec![b'x'; 8 * 1024 * 1024 + 1],
    )?;
    let err = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "huge.log".to_string(),
            rev: None,
            contents: None,
        },
    )
    .expect_err("an oversized worktree file must be rejected");
    let bamboo = err
        .downcast_ref::<bamboo_protocol::BambooError>()
        .expect("the worktree size cap must stay a structured error too");
    assert_eq!(bamboo.code, bamboo_protocol::BambooError::INVALID_ARGUMENT);

    // A revision that does not exist is a git error, not an input error —
    // the client cannot fix it by changing the request shape, and hiding it
    // would render "no annotations" for a typo.
    let err = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: Some("deadbeefdeadbeef".to_string()),
            contents: None,
        },
    )
    .expect_err("an unknown revision must fail");
    let bamboo = err
        .downcast_ref::<bamboo_protocol::BambooError>()
        .expect("unknown revisions stay structured too");
    assert_eq!(bamboo.code, bamboo_protocol::BambooError::GIT_ERROR);
    Ok(())
}

#[tokio::test]
async fn blame_of_absent_path_and_unborn_head_is_empty_not_an_error() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let session = open_session(&fx.dir).await?;

    // Untracked/absent path: toggling blame on such a file is a no-op, the
    // same convention read_blob uses ("" instead of an error).
    let absent = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "no/such/file.txt".to_string(),
            rev: None,
            contents: None,
        },
    )?;
    assert!(absent.entries.is_empty());
    assert_eq!(absent.rev, fx.three);
    // An early return names the backend that WOULD have run, not a hardcoded
    // "gix": `via` is on the wire so a forced backend stays visible even when
    // it never got to blame anything.
    let absent_forced = bamboo_engine::gix_read::read_blame_with(
        &session,
        &bamboo_protocol::GetBlame {
            path: "no/such/file.txt".to_string(),
            rev: None,
            contents: None,
        },
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_eq!(absent_forced.via, bamboo_protocol::BlameVia::GitPorcelain);

    // Unborn HEAD (a repo without a single commit): empty entries and an
    // empty rev, so the client renders nothing instead of an error toast.
    let empty = tempfile::tempdir()?;
    let out = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(empty.path())
        .output()?;
    assert!(out.status.success());
    std::fs::write(empty.path().join("new.txt"), "line\n")?;
    let session = open_session(&empty).await?;
    let result = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "new.txt".to_string(),
            rev: None,
            contents: None,
        },
    )?;
    assert_eq!(result.rev, "");
    assert!(result.entries.is_empty());
    let unborn_forced = bamboo_engine::gix_read::read_blame_with(
        &session,
        &bamboo_protocol::GetBlame {
            path: "new.txt".to_string(),
            rev: None,
            contents: None,
        },
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_eq!(unborn_forced.via, bamboo_protocol::BlameVia::GitPorcelain);
    Ok(())
}

/// A repo with `core.autocrlf=true`: the worktree copy carries CRLF while
/// the blob keeps LF, and git converts before comparing. Blame has to
/// convert the same way, or every line of every file on a Windows checkout
/// reads "Not Committed Yet" — the exact failure the eol normalization in
/// `gix_read` exists to prevent.
fn crlf_repo() -> anyhow::Result<(tempfile::TempDir, String)> {
    let dir = tempfile::tempdir()?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
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
    git(&["init", "-q", "-b", "main"])?;
    git(&["config", "core.autocrlf", "true"])?;
    // Committed with LF, as git stores it after the clean filter.
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n")?;
    git(&["add", "."])?;
    let one = blame_commit(dir.path(), "one", "Ann", "2026-01-01T00:00:00Z")?;
    Ok((dir, one))
}

#[tokio::test]
async fn blame_converts_crlf_the_way_git_does() -> anyhow::Result<()> {
    let (dir, one) = crlf_repo()?;
    let null_id = "0".repeat(40);

    // The worktree copy: CRLF line endings throughout plus one genuinely
    // new line. Only that new line is uncommitted; the rest are `one`'s,
    // which is what `git blame --porcelain -- f.txt` reports here.
    std::fs::write(dir.path().join("f.txt"), "a\r\nb\r\nQ\r\nc\r\n")?;
    let session = open_session(&dir).await?;

    // No `contents` and no `rev`: blame the worktree file, which is what a
    // gutter renders against.
    let worktree = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: None,
        },
    )?;
    assert_tiles(&worktree, 4);
    assert_eq!(
        blamed_lines(&worktree),
        vec![one.clone(), one.clone(), null_id.clone(), one.clone()],
        "a CRLF worktree copy must not make committed lines look uncommitted"
    );

    // The same bytes arriving as an unsaved editor buffer (what VS Code
    // sends for a dirty document) must behave identically.
    let buffer = bamboo_engine::gix_read::read_blame(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: Some("a\r\nb\r\nQ\r\nc\r\n".to_string()),
        },
    )?;
    assert_eq!(
        blamed_lines(&buffer),
        blamed_lines(&worktree),
        "buffer and worktree blame are the same question"
    );

    // And the porcelain fallback, which delegates the conversion to git,
    // must agree — that equivalence is the point of the normalization.
    let porcelain = bamboo_engine::gix_read::read_blame_with(
        &session,
        &bamboo_protocol::GetBlame {
            path: "f.txt".to_string(),
            rev: None,
            contents: None,
        },
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert_eq!(
        blamed_lines(&porcelain),
        blamed_lines(&worktree),
        "the fallback must not attribute lines differently"
    );
    Ok(())
}

#[tokio::test]
async fn blame_of_a_worktree_deleted_file_is_empty() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    // Deleted on disk but still in HEAD: there are no lines to annotate, so
    // the answer is empty rather than HEAD's attribution (which would render
    // against a buffer that is gone) or a git error on a missing path.
    std::fs::remove_file(fx.dir.path().join("f.txt"))?;
    let session = open_session(&fx.dir).await?;
    let result = fx.blame(&session)?;
    assert!(result.entries.is_empty());
    assert_eq!(result.rev, fx.three);
    Ok(())
}

/// A worktree symlink must not be followed. Blame is the only read that opens
/// a client-named path itself, and the containment check upstream is LEXICAL:
/// `link.txt` passes it while pointing anywhere on the filesystem. Following
/// the link would read — and attribute lines of — a file outside the
/// repository, and blaming the link's own blob (what `git blame` does) would
/// number lines against a one-line target path while the editor shows the
/// resolved file. Empty annotations are the honest answer.
#[cfg(unix)]
#[tokio::test]
async fn blame_of_a_worktree_symlink_is_empty_not_a_read_outside_the_repo() -> anyhow::Result<()> {
    let fx = blame_repo()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("id_rsa"), "SECRET\nSECRET2\nSECRET3\n")?;
    // An absolute target: nothing about the requested path hints at it.
    std::os::unix::fs::symlink(
        outside.path().join("id_rsa"),
        fx.dir.path().join("link.txt"),
    )?;
    let session = open_session(&fx.dir).await?;
    let req = bamboo_protocol::GetBlame {
        path: "link.txt".to_string(),
        rev: None,
        contents: None,
    };
    let result = bamboo_engine::gix_read::read_blame(&session, &req)?;
    assert!(
        result.entries.is_empty(),
        "a symlink has no lines to annotate, got {:?}",
        result.entries
    );
    assert_eq!(result.rev, fx.three);

    // Both backends decide the blamed content before they run, so the
    // porcelain path cannot be the one that leaks either.
    let forced = bamboo_engine::gix_read::read_blame_with(
        &session,
        &req,
        bamboo_engine::gix_read::BlameBackend::GitPorcelainOnly,
    )?;
    assert!(forced.entries.is_empty());
    assert_eq!(forced.via, bamboo_protocol::BlameVia::GitPorcelain);
    Ok(())
}
