use git_workbench_protocol::*;

#[test]
fn commit_roundtrip() {
    let c = Commit {
        id: "abc123".into(),
        message: "hello world".into(),
        author_name: "Alice".into(),
        author_email: "alice@example.com".into(),
        author_time: 1234567890,
        parent_ids: vec!["def456".into()],
        lane: 2,
    };
    let json = serde_json::to_string(&c).unwrap();
    let back: Commit = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn status_code_serializes_snake_case() {
    let item = StatusItem {
        path: "src/main.rs".into(),
        status: StatusCode::Untracked,
        old_path: None,
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"untracked\""));
}

#[test]
fn workbench_error_codes() {
    let e = WorkbenchError::epoch_mismatch(4, 5);
    assert_eq!(e.code, WorkbenchError::EPOCH_MISMATCH);
    assert!(e.message.contains("expected 4"));
    assert!(e.to_string().contains("WorkbenchError"));
}

#[test]
fn graph_page_serialization() {
    let page = GraphPage {
        commits: vec![],
        total_approx: 1000,
        has_more: true,
        epoch: 7,
        anchor_commit: Some("abc".into()),
    };
    let json = serde_json::to_string(&page).unwrap();
    assert!(json.contains("\"total_approx\":1000"));
    let back: GraphPage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.epoch, 7);
    assert!(back.has_more);
}
