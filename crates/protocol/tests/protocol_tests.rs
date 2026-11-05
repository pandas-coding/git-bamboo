use bamboo_protocol::*;

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
        conflict_kind: None,
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"untracked\""));
}

#[test]
fn status_item_conflict_kind_omitted_for_non_conflicts() {
    // Backwards wire compatibility: non-conflict entries carry no
    // conflict_kind field at all, and parsing old payloads (no field)
    // still yields None.
    let item = StatusItem {
        path: "a.txt".into(),
        status: StatusCode::Modified,
        old_path: None,
        staged: false,
        conflict_kind: None,
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("conflict_kind"));
    let legacy = r#"{"path":"a.txt","status":"modified","staged":false}"#;
    let back: StatusItem = serde_json::from_str(legacy).unwrap();
    assert_eq!(back.conflict_kind, None);
}

#[test]
fn conflict_kind_serializes_snake_case() {
    let item = StatusItem {
        path: "a.txt".into(),
        status: StatusCode::Conflict,
        old_path: None,
        staged: false,
        conflict_kind: Some(ConflictKind::BothModified),
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"conflict_kind\":\"both_modified\""));
    let back: StatusItem = serde_json::from_str(&json).unwrap();
    assert_eq!(back.conflict_kind, Some(ConflictKind::BothModified));
}

#[test]
fn resolve_conflict_roundtrip() {
    let req = ResolveConflict {
        path: "src/main.rs".into(),
        resolution: ConflictResolution::KeepTheirs,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"keep_theirs\""));
    let back: ResolveConflict = serde_json::from_str(&json).unwrap();
    assert_eq!(back.path, "src/main.rs");
    assert_eq!(back.resolution, ConflictResolution::KeepTheirs);
    // Unknown resolutions are rejected at the parse boundary, not deep
    // in the engine.
    assert!(serde_json::from_str::<ResolveConflict>(
        r#"{"path":"a","resolution":"bogus"}"#
    )
    .is_err());
}

#[test]
fn bamboo_error_codes() {
    let e = BambooError::epoch_mismatch(4, 5);
    assert_eq!(e.code, BambooError::EPOCH_MISMATCH);
    assert!(e.message.contains("expected 4"));
    assert!(e.to_string().contains("BambooError"));
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

// ---------------------------------------------------------------------------
// Network ops protocol (push force modes, pull strategy)
// ---------------------------------------------------------------------------

#[test]
fn force_mode_string_forms() {
    let p: Push =
        serde_json::from_str(r#"{"remote":"origin","branch":"main","force":"lease"}"#).unwrap();
    assert_eq!(p.force, ForceMode::Lease);

    let p: Push =
        serde_json::from_str(r#"{"remote":"origin","branch":"main","force":"force"}"#).unwrap();
    assert_eq!(p.force, ForceMode::Force);

    let p: Push =
        serde_json::from_str(r#"{"remote":"origin","branch":"main","force":"none"}"#).unwrap();
    assert_eq!(p.force, ForceMode::None);
}

#[test]
fn force_mode_legacy_bool_maps_to_lease_and_none() {
    // Old clients sent `force: true/false`; no protocol version jump, so
    // the new enum must still parse them.
    let p: Push =
        serde_json::from_str(r#"{"remote":"origin","branch":"main","force":true}"#).unwrap();
    assert_eq!(p.force, ForceMode::Lease);
    let p: Push =
        serde_json::from_str(r#"{"remote":"origin","branch":"main","force":false}"#).unwrap();
    assert_eq!(p.force, ForceMode::None);
}

#[test]
fn force_mode_defaults_and_roundtrip() {
    // Absent force = none, absent set_upstream = false.
    let p: Push = serde_json::from_str(r#"{"remote":"origin","branch":"main"}"#).unwrap();
    assert_eq!(p.force, ForceMode::None);
    assert!(!p.set_upstream);

    // Roundtrip through the wire format.
    let p = Push {
        remote: "origin".into(),
        branch: "main".into(),
        force: ForceMode::Lease,
        set_upstream: true,
    };
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains(r#""force":"lease""#), "{json}");
    let back: Push = serde_json::from_str(&json).unwrap();
    assert_eq!(back.force, ForceMode::Lease);
    assert!(back.set_upstream);
}

#[test]
fn force_mode_rejects_unknown_strings() {
    assert!(serde_json::from_str::<Push>(r#"{"remote":"o","branch":"b","force":"nuke"}"#).is_err());
}

#[test]
fn pull_strategy_wire_forms() {
    let base = r#"{"remote":"origin","branch":"main"}"#;
    let p: Pull = serde_json::from_str(base).unwrap();
    assert_eq!(p.strategy, PullStrategy::Default);
    assert!(!p.auto_stash);

    for (wire, strategy) in [
        ("\"merge\"", PullStrategy::Merge),
        ("\"rebase\"", PullStrategy::Rebase),
        ("\"ff-only\"", PullStrategy::FfOnly),
    ] {
        let p: Pull = serde_json::from_str(&format!(
            r#"{{"remote":"o","branch":"b","strategy":{wire},"auto_stash":true}}"#
        ))
        .unwrap();
        assert_eq!(p.strategy, strategy);
        assert!(p.auto_stash);
    }
}

#[test]
fn stash_types_wire_shapes() {
    // push: message required-ish (Option), flags default false.
    let p: StashPush = serde_json::from_str(r#"{"message":null}"#).unwrap();
    assert!(p.message.is_none());
    assert!(!p.include_untracked && !p.staged_only);

    let pop: StashPop = serde_json::from_str("{}").unwrap();
    assert!(pop.stash_ref.is_none());
    assert!(!pop.apply && !pop.reinstate_index);

    let drop: StashDrop = serde_json::from_str(r#"{"stash_ref":"stash@{1}"}"#).unwrap();
    assert_eq!(drop.stash_ref.as_deref(), Some("stash@{1}"));

    // StashEntry serializes `ref` (reserved keyword) on the wire.
    let e = StashEntry {
        stash_ref: "stash@{0}".into(),
        message: "WIP on main".into(),
        date: "2026-10-26 12:00:00 +0800".into(),
    };
    let json = serde_json::to_string(&e).unwrap();
    assert!(json.contains(r#""ref":"stash@{0}""#), "{json}");
    let back: StashEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);

    // getBranchStatus: branch optional (defaults to current).
    let g: GetBranchStatus = serde_json::from_str("{}").unwrap();
    assert!(g.branch.is_none());

    // getRemotes: empty params object.
    let _: GetRemotes = serde_json::from_str("{}").unwrap();
}

#[test]
fn new_error_code_constructors() {
    let e = BambooError::stash_conflict("conflict; stash retained");
    assert_eq!(e.code, BambooError::STASH_CONFLICT);
    let e = BambooError::push_rejected("rejected (fetch first)");
    assert_eq!(e.code, BambooError::PUSH_REJECTED);
    let e = BambooError::auth_required("could not read Username");
    assert_eq!(e.code, BambooError::AUTH_REQUIRED);
}

#[test]
fn diff_types_wire_shapes() {
    // LineRange is a flat {start, end} object.
    let range: LineRange = serde_json::from_str(r#"{"start":2,"end":6}"#).unwrap();
    assert_eq!((range.start, range.end), (2, 6));
    assert!(serde_json::from_str::<LineRange>(r#"{"start":0,"end":6}"#).is_ok()); // bounds enforced engine-side
    assert!(serde_json::from_str::<LineRange>("{}").is_err());

    // GetDiff roundtrips with snake_case hunk fields and omitted-when-empty optional flavor.
    let result = GetDiffResult {
        binary: false,
        hunks: vec![DiffHunk {
            old_start: 1,
            old_lines: 5,
            new_start: 1,
            new_lines: 5,
            lines: vec![
                DiffLine { kind: DiffLineKind::Context, text: "line1".into() },
                DiffLine { kind: DiffLineKind::Removed, text: "line2".into() },
                DiffLine { kind: DiffLineKind::Added, text: "l2-dirty".into() },
            ],
        }],
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("\"old_start\":1"));
    assert!(json.contains("\"new_lines\":5"));
    assert!(json.contains("\"context\""));
    assert!(json.contains("\"removed\""));
    assert!(json.contains("\"added\""));
    let back: GetDiffResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back, result);

    // stageRanges / unstageRanges roundtrip.
    let stage: StageRanges = serde_json::from_str(
        r#"{"path":"src/main.rs","ranges":[{"start":1,"end":5}]}"#,
    )
    .unwrap();
    assert_eq!(stage.path, "src/main.rs");
    let unstage: UnstageRanges = serde_json::from_str(
        r#"{"path":"src/main.rs","ranges":[{"start":1,"end":5}]}"#,
    )
    .unwrap();
    assert_eq!(unstage.ranges.len(), 1);
}

// ---------------------------------------------------------------------------
// Interactive rebase types
// ---------------------------------------------------------------------------

#[test]
fn rebase_result_roundtrip() {
    let r = RebaseResult {
        stopped: true,
        reason: Some(RebaseStopReason::RewordStop),
        stopped_sha: Some("abc".into()),
        msgnum: Some(2),
        end: Some(5),
        finished: false,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"reword_stop\""));
    let back: RebaseResult = serde_json::from_str(&json).unwrap();
    assert_eq!(r, back);

    // The idle/default shape round-trips too.
    let r = RebaseResult::default();
    let back: RebaseResult =
        serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(r, back);
}

#[test]
fn rebase_action_serializes_lowercase() {
    let json = serde_json::to_string(&RebaseAction::Pick).unwrap();
    assert_eq!(json, "\"pick\"");
    assert_eq!(
        serde_json::to_string(&RebaseAction::Fixup).unwrap(),
        "\"fixup\""
    );
    // Parsing is case-sensitive lowercase, like git's todo words.
    let a: RebaseAction = serde_json::from_str("\"squash\"").unwrap();
    assert_eq!(a, RebaseAction::Squash);
}

#[test]
fn rebase_todo_entry_roundtrip() {
    let e = RebaseTodoEntry {
        id: "0123456789abcdef0123456789abcdef01234567".into(),
        action: RebaseAction::Reword,
        message: Some("renamed".into()),
    };
    let json = serde_json::to_string(&e).unwrap();
    assert!(json.contains("\"reword\""));
    let back: RebaseTodoEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(e, back);

    // message is optional on the wire.
    let back: RebaseTodoEntry =
        serde_json::from_str("{\"id\":\"x\",\"action\":\"pick\"}").unwrap();
    assert_eq!(back.message, None);
}

#[test]
fn rebase_plan_and_status_roundtrip() {
    let req = GetRebasePlan {
        onto: "main".into(),
        branch: None,
    };
    let back: GetRebasePlan = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(req, back);

    let result = GetRebasePlanResult {
        onto: "main".into(),
        upstream: "c2".into(),
        branch: "feature".into(),
        commits: vec![RebasePlanCommit {
            id: "abc".into(),
            subject: "f1".into(),
            author: "Tester".into(),
            time: 123,
        }],
    };
    let back: GetRebasePlanResult =
        serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(result, back);

    let status = GetRebaseStatusResult {
        state: RebaseRunState::Stopped,
        backend: Some(RebaseBackend::Merge),
        branch: Some("feature".into()),
        onto: Some("main".into()),
        orig_head: Some("tip".into()),
        stopped_sha: Some("abc".into()),
        msgnum: Some(1),
        end: Some(3),
        done: vec!["pick abc f1".into()],
        remaining: vec!["pick def f2".into()],
        done_entries: vec![RebaseStatusEntry {
            action: RebaseAction::Pick,
            id: "abc".into(),
            subject: "f1".into(),
        }],
        remaining_entries: vec![RebaseStatusEntry {
            action: RebaseAction::Squash,
            id: "def".into(),
            subject: "f2".into(),
        }],
    };
    let json = serde_json::to_string(&status).unwrap();
    assert!(json.contains("\"stopped\""));
    assert!(json.contains("\"merge\""));
    let back: GetRebaseStatusResult = serde_json::from_str(&json).unwrap();
    assert_eq!(status, back);

    // The idle default serializes compactly (optional fields skipped).
    let idle = GetRebaseStatusResult::default();
    let json = serde_json::to_string(&idle).unwrap();
    assert!(json.contains("\"idle\""));
    assert!(!json.contains("backend"));
}

#[test]
fn start_rebase_request_roundtrip() {
    let req = StartRebase {
        onto: "main".into(),
        branch: Some("feature".into()),
        todo: vec![RebaseTodoEntry {
            id: "abc".into(),
            action: RebaseAction::Drop,
            message: None,
        }],
    };
    let back: StartRebase = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(req, back);

    // branch/todo default to null/empty.
    let req: StartRebase = serde_json::from_str("{\"onto\":\"main\"}").unwrap();
    assert_eq!(req.branch, None);
    assert!(req.todo.is_empty());

    let c: RebaseContinue = serde_json::from_str("{}").unwrap();
    let _: RebaseSkip = serde_json::from_str("{}").unwrap();
    let _: RebaseAbort = serde_json::from_str("{}").unwrap();
    let _ = c;
}
