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
    let undo2 = git_workbench_engine::undo::UndoEngine::new(dir.path())?;
    let entries2 = undo2.list_transactions(10)?;
    assert_eq!(entries2.len(), 1);
    Ok(())
}

#[tokio::test]
async fn write_queue_stage_and_epoch_bump() -> anyhow::Result<()> {
    let dir = test_repo()?;
    let session = open_session(&dir).await?;

    std::fs::write(dir.path().join("new.txt"), "new\n")?;

    // Stage the new file through the write queue.
    session
        .enqueue_write(git_workbench_engine::write_queue::WriteCommand::Stage(vec![
            "new.txt".to_string(),
        ]))
        .await?;

    // Epoch must have bumped (the write queue bump plus possible fs
    // watcher bumps from the git add writing .git/index).
    assert!(session.current_epoch() >= 2);

    // The file should now appear as staged (tree-index change).
    let items = git_workbench_engine::gix_read::read_status(&session, None)?;
    let staged = items
        .iter()
        .any(|i| i.path == "new.txt" && i.status == git_workbench_protocol::StatusCode::Added);
    assert!(staged, "expected staged new.txt, got {:?}", items);

    // An undo snapshot and audit entry should exist.
    let entries = session.undo.list_transactions(10)?;
    assert_eq!(entries.len(), 1);
    let audit = std::fs::read_to_string(
        dir.path().join(".git").join("git-workbench").join("audit.log"),
    )?;
    assert!(audit.contains("\"status\":\"ok\""));
    Ok(())
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
