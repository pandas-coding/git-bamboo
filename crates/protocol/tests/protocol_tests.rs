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
        staged: false,
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

#[test]
fn commit_lane_is_u16() {
    let c = Commit {
        id: "abc123".into(),
        message: "hello world".into(),
        author_name: "Alice".into(),
        author_email: "alice@example.com".into(),
        author_time: 1234567890,
        parent_ids: vec!["def456".into()],
        lane: 60000,
    };
    let json = serde_json::to_string(&c).unwrap();
    assert!(json.contains("\"lane\":60000"));
    let back: Commit = serde_json::from_str(&json).unwrap();
    assert_eq!(back.lane, 60000);
}

#[test]
fn notifications_carry_epoch() {
    let refs = RefsChanged {
        changed_refs: vec!["refs/heads/main".into()],
        epoch: 7,
    };
    let json = serde_json::to_string(&refs).unwrap();
    assert!(json.contains("\"changed_refs\""));
    assert!(json.contains("\"epoch\":7"));

    let idx = IndexChanged { epoch: 3 };
    let json = serde_json::to_string(&idx).unwrap();
    assert!(json.contains("\"epoch\":3"));

    let head = HeadChanged {
        new_head: "ref: refs/heads/main".into(),
        epoch: 9,
    };
    let json = serde_json::to_string(&head).unwrap();
    assert!(json.contains("\"new_head\""));
    assert!(json.contains("\"epoch\":9"));
}

#[test]
fn undo_response_serializes_like_the_original() {
    // The hand-rolled response was {"restored_refs": N, "epoch": E};
    // `noop` is informational and stays off the wire.
    let r = UndoResponse {
        restored_refs: 2,
        noop: false,
        epoch: 5,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(json, "{\"restored_refs\":2,\"epoch\":5}");

    let r = UndoResponse {
        restored_refs: 0,
        noop: true,
        epoch: 9,
    };
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        "{\"restored_refs\":0,\"epoch\":9}"
    );
}

#[test]
fn initialize_result_keeps_wire_format() {
    // The hand-rolled initialize response's exact wire format, preserved
    // by the typed struct (note camelCase "writeOps").
    let expected: serde_json::Value = serde_json::from_str(
        r#"{"session_id":"s1","protocol_version":1,"engine_version":"0.1.0","epoch":4,"capabilities":{"graph":true,"status":true,"writeOps":true,"undo":true,"notifications":true}}"#,
    )
    .unwrap();
    let result = InitializeResult {
        session_id: "s1".into(),
        protocol_version: 1,
        engine_version: "0.1.0".into(),
        epoch: 4,
        capabilities: Capabilities {
            graph: true,
            status: true,
            write_ops: true,
            undo: true,
            notifications: true,
        },
    };
    assert_eq!(serde_json::to_value(&result).unwrap(), expected);
}

#[test]
fn request_defaults_match_old_parser() {
    // switchBranch: auto_stash defaults to true when absent.
    let sb: SwitchBranch = serde_json::from_str(r#"{"name":"main"}"#).unwrap();
    assert!(sb.auto_stash);
    // commit: amend defaults to false.
    let cm: CommitMsg = serde_json::from_str(r#"{"message":"m"}"#).unwrap();
    assert!(!cm.amend);
    // listUndoStack: limit defaults to 50.
    let lu: ListUndoStack = serde_json::from_str("{}").unwrap();
    assert_eq!(lu.limit, 50);
    // getStatus: paths optional.
    let gs: GetStatus = serde_json::from_str("{}").unwrap();
    assert!(gs.paths.is_none());
}
