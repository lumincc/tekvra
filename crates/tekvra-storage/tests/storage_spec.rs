//! Specification integration tests for `tekvra-storage` (Tasks `3.1` & `3.2`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use tekvra_protocol::{ApprovalScope, GrantLifetime, RevertScope, TokenUsage};
use tekvra_providers::ProviderMessage;
use tekvra_storage::{
    RolloutHeader, RolloutLog, RolloutRecord, SessionSummary, StateIndex, StorageError,
    WorkspaceSnapshotStore, rollout_path,
};
use tempfile::TempDir;

#[test]
fn single_writer_lease_partial_line_crash_recovery_fts5_and_retained_output() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let ws = tmp.path().join("workspace");
    fs::create_dir_all(&ws).expect("mkdir ws");

    let index = StateIndex::open(&state_dir).expect("open StateIndex");
    let hdr = RolloutHeader {
        format_version: 1,
        session_id: "thr_crash_test".to_string(),
        parent_session_id: None,
        forked_from_turn_id: None,
        cwd: ws.clone(),
        model: "openai/gpt-5.4-codex".to_string(),
        created_at_epoch_secs: 1_700_000_000,
    };

    let mut log = RolloutLog::create(&state_dir, &hdr).expect("create rollout");
    log.append(&RolloutRecord::TurnStarted {
        turn_id: "turn_1".to_string(),
        started_at: 1_700_000_001,
    })
    .expect("append TurnStarted");
    log.append(&RolloutRecord::Message {
        turn_id: "turn_1".to_string(),
        message: ProviderMessage::user_text("Refactor authentication middleware"),
    })
    .expect("append Message");

    // Second concurrent writer MUST fail with `SessionLocked` while `log` is alive!
    let second = RolloutLog::open_and_recover(&state_dir, "thr_crash_test");
    assert!(matches!(second, Err(StorageError::SessionLocked { .. })));

    // Drop first writer to release lease, then simulate crash leaving truncated JSON tail
    drop(log);
    {
        let p = rollout_path(&state_dir, "thr_crash_test");
        let mut f = OpenOptions::new().append(true).open(&p).expect("open raw");
        f.write_all(b"{\"record\":\"message\",\"turn_id\":\"turn_2\",\"unterminated")
            .expect("write partial tail");
    }

    let (recovered_log, records) =
        RolloutLog::open_and_recover(&state_dir, "thr_crash_test").expect("recover after crash");
    assert_eq!(
        records.len(),
        3,
        "header + turn_started + valid message preserved"
    );
    drop(recovered_log);

    // Verify FTS5 full-text search, usage ledger, permission grants, and retained output paging
    index
        .upsert_session(&SessionSummary {
            id: "thr_crash_test".to_string(),
            parent_id: None,
            forked_from_turn_id: None,
            cwd: ws.clone(),
            project_id: "proj_1".to_string(),
            model: "openai/gpt-5.4-codex".to_string(),
            title: Some("Auth Refactor".to_string()),
            preview: "Refactor authentication middleware".to_string(),
            archived: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_010,
        })
        .expect("upsert_session");

    index
        .index_message_text(
            "thr_crash_test",
            "turn_1",
            "user",
            "Refactor authentication middleware with zero downtime",
        )
        .expect("index_message_text");

    let hits = index
        .search_history("authentication", 10)
        .expect("fts5 search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "thr_crash_test");
    assert!(hits[0].snippet.contains("[authentication]"));

    let totals = index
        .record_usage(
            "thr_crash_test",
            "turn_1",
            "openai/gpt-5.4-codex",
            &TokenUsage {
                input_tokens: 500,
                cached_input_tokens: 450,
                output_tokens: 100,
                reasoning_tokens: 30,
                cost_usd: 0.005,
            },
            1_700_000_010,
        )
        .expect("record_usage");
    assert_eq!(totals.input_tokens, 500);

    index
        .save_permission_grant(
            Some(&ws),
            GrantLifetime::Project,
            &ApprovalScope::CommandPrefix {
                tokens: vec!["cargo".to_string(), "test".to_string()],
            },
            1_700_000_011,
        )
        .expect("save grant");
    let grants = index.load_permission_grants(&ws).expect("load grants");
    assert_eq!(grants.len(), 1);

    let out_id = index
        .store_retained_output(
            "thr_crash_test",
            "cargo test",
            "line one\nerror[E0425]: missing symbol\nline three\n",
            1_700_000_012,
        )
        .expect("store_retained_output");
    let regex_page = index
        .query_retained_output(&out_id, 1, 10, Some("E0425"))
        .expect("regex page");
    assert_eq!(regex_page.lines, vec!["2: error[E0425]: missing symbol"]);
}

#[test]
fn multi_turn_workspace_snapshots_fork_and_atomic_conversation_plus_files_revert() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let ws = tmp.path().join("repo");
    fs::create_dir_all(ws.join("src")).expect("mkdir repo/src");

    // Initial workspace state before Turn 2 & Turn 3
    let keep_file = ws.join("src/keep.rs");
    let edit_file = ws.join("src/lib.rs");
    let delete_file = ws.join("src/obsolete.rs");
    fs::write(&keep_file, "pub fn untouched() {}\n").expect("write keep.rs");
    fs::write(&edit_file, "pub fn v1() -> i32 { 1 }\n").expect("write lib.rs");
    fs::write(&delete_file, "pub fn legacy() {}\n").expect("write obsolete.rs");

    let index = StateIndex::open(&state_dir).expect("open StateIndex");
    let snapshots = WorkspaceSnapshotStore::new(&state_dir);

    let hdr = RolloutHeader {
        format_version: 1,
        session_id: "thr_main".to_string(),
        parent_session_id: None,
        forked_from_turn_id: None,
        cwd: fs::canonicalize(&ws).expect("canonical ws"),
        model: "openai/gpt-5.4-codex".to_string(),
        created_at_epoch_secs: 1_700_000_100,
    };
    let mut rollout = RolloutLog::create(&state_dir, &hdr).expect("create rollout");
    index
        .upsert_session(&SessionSummary {
            id: "thr_main".to_string(),
            parent_id: None,
            forked_from_turn_id: None,
            cwd: hdr.cwd.clone(),
            project_id: "proj_repo".to_string(),
            model: hdr.model.clone(),
            title: Some("Main".to_string()),
            preview: "Turn 1".to_string(),
            archived: false,
            created_at: 1_700_000_100,
            updated_at: 1_700_000_100,
        })
        .expect("upsert");

    // Turn 1: read-only conversation turn (target state we will revert back to!)
    rollout
        .append(&RolloutRecord::TurnStarted {
            turn_id: "turn_1".to_string(),
            started_at: 1_700_000_101,
        })
        .expect("t1 start");
    rollout
        .append(&RolloutRecord::Message {
            turn_id: "turn_1".to_string(),
            message: ProviderMessage::user_text("Explain repo layout"),
        })
        .expect("t1 msg");

    // Turn 2: modifies `src/lib.rs` and creates `src/new_mod/added.rs`
    rollout
        .append(&RolloutRecord::TurnStarted {
            turn_id: "turn_2".to_string(),
            started_at: 1_700_000_110,
        })
        .expect("t2 start");
    let added_file = ws.join("src/new_mod/added.rs");
    let snap2 = snapshots
        .capture_paths(
            "thr_main",
            "turn_2",
            &ws,
            &[
                PathBuf::from("src/lib.rs"),
                PathBuf::from("src/new_mod/added.rs"),
            ],
        )
        .expect("snap2");
    rollout
        .append(&RolloutRecord::SnapshotRecorded {
            turn_id: "turn_2".to_string(),
            snapshot_id: snap2.snapshot_id,
        })
        .expect("rec snap2");
    fs::create_dir_all(added_file.parent().expect("parent")).expect("mkdir new_mod");
    fs::write(&edit_file, "pub fn v2() -> i32 { 2 }\n").expect("edit lib.rs v2");
    fs::write(&added_file, "pub fn added() {}\n").expect("create added.rs");

    // Fork session after Turn 2 and verify parent linkage!
    let fork_id = snapshots
        .fork_session(&index, "thr_main", Some("turn_2"), 1_700_000_115)
        .expect("fork after turn_2");
    let fork_summary = index
        .get_session(&fork_id)
        .expect("get fork")
        .expect("fork exists");
    assert_eq!(fork_summary.parent_id.as_deref(), Some("thr_main"));
    assert_eq!(fork_summary.forked_from_turn_id.as_deref(), Some("turn_2"));

    // Turn 3: modifies `src/lib.rs` AGAIN (v3) and deletes `src/obsolete.rs`
    rollout
        .append(&RolloutRecord::TurnStarted {
            turn_id: "turn_3".to_string(),
            started_at: 1_700_000_120,
        })
        .expect("t3 start");
    let snap3 = snapshots
        .capture_paths(
            "thr_main",
            "turn_3",
            &ws,
            &[
                PathBuf::from("src/lib.rs"),
                PathBuf::from("src/obsolete.rs"),
            ],
        )
        .expect("snap3");
    rollout
        .append(&RolloutRecord::SnapshotRecorded {
            turn_id: "turn_3".to_string(),
            snapshot_id: snap3.snapshot_id,
        })
        .expect("rec snap3");
    fs::write(&edit_file, "pub fn v3() -> i32 { 3 }\n").expect("edit lib.rs v3");
    fs::remove_file(&delete_file).expect("delete obsolete.rs");

    // Revert `conversation + files` from `turn_2` onwards (back to end of `turn_1`)!
    drop(rollout);
    let (mut rollout, current_records) =
        RolloutLog::open_and_recover(&state_dir, "thr_main").expect("reopen thr_main");
    let outcome = snapshots
        .revert_from_turn(
            &index,
            &mut rollout,
            &current_records,
            "turn_2",
            RevertScope::ConversationAndFiles,
            1_700_000_130,
        )
        .expect("revert turn_2..turn_3");
    drop(rollout);

    // 1. All 3 touched files MUST match exact pre-turn_2 state:
    assert_eq!(
        fs::read_to_string(&edit_file).expect("read lib.rs"),
        "pub fn v1() -> i32 { 1 }\n"
    );
    assert_eq!(
        fs::read_to_string(&delete_file).expect("read restored obsolete.rs"),
        "pub fn legacy() {}\n"
    );
    assert!(
        !added_file.exists(),
        "newly created file must be deleted on revert"
    );
    assert!(
        !ws.join("src/new_mod").exists(),
        "empty newly created directory cleaned up"
    );
    assert_eq!(
        fs::read_to_string(&keep_file).expect("keep.rs"),
        "pub fn untouched() {}\n"
    );

    // 2. Active rollout rewinds to Turn 1 (Header + Turn 1 start + Turn 1 msg = 3 records)
    let rewound = RolloutLog::read_records_readonly(&state_dir, "thr_main").expect("read rewound");
    assert_eq!(rewound.len(), 3);

    // 3. Abandoned branch remains recoverable in parent-linked storage!
    let branch_id = outcome
        .preserved_branch_session_id
        .expect("abandoned branch saved");
    let branch_records =
        RolloutLog::read_records_readonly(&state_dir, &branch_id).expect("read abandoned branch");
    assert_eq!(branch_records.len(), current_records.len());
}
