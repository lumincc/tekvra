//! Round-trip serde and Codex v2 policy-mapping contract tests for `tekvra-protocol`.

use std::path::Path;

use tekvra_protocol::codex_v2::{
    CodexApprovalPolicy, CodexApprovalsReviewer, CodexSandboxMode, CodexSandboxPolicy, CodexThread,
    CodexThreadItem, CodexThreadStatus, CodexTurn, CodexTurnStatus, CodexUserInput,
    InitializeParams, InitializeResponse, JsonRpcId, JsonRpcRequest, map_codex_policy,
};
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, ApprovalScope, FileChangeKind, FileDiffEntry,
    GrantLifetime, InputDeliveryMode, PermissionMode, ScopeLadderOption, SessionCommand,
    SessionEvent, TokenUsage, ToolOutput,
};

#[test]
fn internal_session_and_tool_output_roundtrip() {
    let cmd = SessionCommand::SubmitInput {
        prompt: "run cargo test".to_string(),
        delivery: InputDeliveryMode::Guide,
    };
    let encoded = serde_json::to_string(&cmd).expect("serialize SessionCommand");
    let decoded: SessionCommand =
        serde_json::from_str(&encoded).expect("deserialize SessionCommand");
    assert_eq!(decoded, cmd);

    let event = SessionEvent::ItemCompleted {
        thread_id: "thr_01".to_string(),
        turn_id: "turn_01".to_string(),
        item_id: "item_01".to_string(),
        text: "Applied patch".to_string(),
        tool_output: Some(ToolOutput::Diff {
            files: vec![FileDiffEntry {
                path: "src/lib.rs".into(),
                moved_from: None,
                change_kind: FileChangeKind::Update,
                unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                additions: 1,
                deletions: 1,
            }],
            snapshot_id: Some("snap_abc".to_string()),
        }),
    };
    let json = serde_json::to_value(&event).expect("to_value SessionEvent");
    let roundtrip: SessionEvent = serde_json::from_value(json).expect("from_value SessionEvent");
    assert_eq!(roundtrip, event);

    let usage = TokenUsage {
        input_tokens: 1_000,
        cached_input_tokens: 950,
        output_tokens: 120,
        reasoning_tokens: 40,
        cost_usd: 0.012,
    };
    assert!((usage.cache_hit_ratio() - 0.95).abs() < 1e-9);
}

#[test]
fn scope_ladder_broad_confirmation_rules() {
    let home = Path::new("/home/alice");
    assert!(
        ApprovalScope::CommandPrefix {
            tokens: vec!["cargo".to_string()]
        }
        .requires_broad_confirmation(Some(home))
    );
    assert!(
        !ApprovalScope::CommandPrefix {
            tokens: vec!["cargo".to_string(), "test".to_string()]
        }
        .requires_broad_confirmation(Some(home))
    );
    assert!(
        ApprovalScope::PathPrefix {
            path: "/etc/nginx".into()
        }
        .requires_broad_confirmation(Some(home))
    );
    assert!(
        !ApprovalScope::PathPrefix {
            path: "/home/alice/repo".into()
        }
        .requires_broad_confirmation(Some(home))
    );

    let prompt = ApprovalPrompt {
        approval_id: "ap_1".to_string(),
        item_id: "it_1".to_string(),
        tool_name: "exec_command".to_string(),
        summary: "cargo test -p tekvra-core".to_string(),
        cwd: Some("/home/alice/repo".into()),
        once_only: false,
        ladder: vec![ScopeLadderOption {
            label: "cargo *".to_string(),
            scope: ApprovalScope::CommandPrefix {
                tokens: vec!["cargo".to_string()],
            },
            broad_grant: true,
        }],
    };
    let resolution = ApprovalResolution::Approve {
        scope: Some(prompt.ladder[0].scope.clone()),
        lifetime: GrantLifetime::Project,
        confirmed_broad: true,
    };
    let encoded = serde_json::to_string(&resolution).expect("serialize resolution");
    let decoded: ApprovalResolution =
        serde_json::from_str(&encoded).expect("deserialize resolution");
    assert_eq!(decoded, resolution);
}

#[test]
fn codex_v2_dto_and_sandbox_mapping_roundtrip() {
    let req_raw = r#"{
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "codex-desktop",
                "title": "Codex Desktop",
                "version": "0.172.0"
            }
        }
    }"#;
    let rpc: JsonRpcRequest = serde_json::from_str(req_raw).expect("parse JsonRpcRequest");
    assert_eq!(rpc.id, JsonRpcId::Number(1));
    let init: InitializeParams =
        serde_json::from_value(rpc.params.expect("params")).expect("parse InitializeParams");
    assert_eq!(init.client_info.name, "codex-desktop");

    let init_resp = InitializeResponse {
        codex_home: "/home/alice/.config/tekvra".to_string(),
        platform_family: "unix".to_string(),
        platform_os: "linux".to_string(),
        user_agent: "tekvra/0.1.0".to_string(),
    };
    let resp_val = serde_json::to_value(&init_resp).expect("serialize InitializeResponse");
    assert_eq!(resp_val["codexHome"], "/home/alice/.config/tekvra");

    let thread = CodexThread {
        id: "thr_1".to_string(),
        session_id: "thr_1".to_string(),
        project_id: "proj_1".to_string(),
        cli_version: "0.1.0".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_010,
        cwd: "/workspace".to_string(),
        ephemeral: false,
        model_provider: "openai".to_string(),
        preview: "Hello Tekvra".to_string(),
        source: "appServer".to_string(),
        status: CodexThreadStatus::Idle,
        turns: vec![CodexTurn {
            id: "turn_1".to_string(),
            items: vec![CodexThreadItem::UserMessage {
                id: "item_u1".to_string(),
                content: vec![CodexUserInput::Text {
                    text: "Hello Tekvra".to_string(),
                    text_elements: Vec::new(),
                }],
            }],
            status: CodexTurnStatus::Completed,
            error: None,
        }],
        forked_from_id: None,
        name: Some("Bootstrap".to_string()),
    };
    let thread_json = serde_json::to_string(&thread).expect("serialize CodexThread");
    let thread_back: CodexThread =
        serde_json::from_str(&thread_json).expect("deserialize CodexThread");
    assert_eq!(thread_back, thread);

    // Sandbox mapping checks
    let ro = map_codex_policy(
        Some(CodexSandboxMode::ReadOnly),
        None,
        Some(&CodexApprovalPolicy::default()),
        None,
    );
    assert_eq!(ro.mode, PermissionMode::Plan);
    assert!(ro.warning.is_some());

    let yolo = map_codex_policy(
        None,
        Some(&CodexSandboxPolicy::DangerFullAccess),
        Some(&CodexApprovalPolicy::Simple(
            tekvra_protocol::codex_v2::CodexSimpleApprovalPolicy::Never,
        )),
        Some(CodexApprovalsReviewer::User),
    );
    assert_eq!(yolo.mode, PermissionMode::Yolo);
    assert!(yolo.warning.is_none());
}
