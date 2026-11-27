//! `ratatui::backend::TestBackend` UI specification tests for `tekvra-tui` (Task 6.1):
//! - Zero MPL-2.0 syntax assets
//! - Raw vs Filtered terminal `ToolOutput` toggling
//! - Scope-ladder selection & secondary broad-grant confirmation
//! - `/tasks` (`Ctrl-X`) subagent switcher & live steering

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tekvra_core::SubagentEntry;
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, ApprovalScope, GrantLifetime, InputDeliveryMode,
    PermissionMode, ScopeLadderOption, SessionCommand, SessionEvent, ToolOutput,
};
use tekvra_tui::{ActiveOverlay, CodeHighlighter, SearchOverlayState, TuiApp, TuiCommand};

fn render_screen_text(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| app.render(f)).expect("draw");
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn permissive_syntax_highlighter_and_raw_filtered_terminal_toggle() {
    // 1. Ensure MPL-2.0 Rhai syntax asset is NOT bundled
    assert!(
        !CodeHighlighter::global().contains_excluded_syntax(),
        "MPL-2.0 Rhai syntax must not be bundled in tekvra-tui"
    );

    let mut app = TuiApp::new("thr_ui_01", "openai/gpt-5.4-codex", PermissionMode::Build);
    app.apply_session_event(SessionEvent::ItemCompleted {
        thread_id: "thr_ui_01".to_string(),
        turn_id: "turn_1".to_string(),
        item_id: "call_pty_1".to_string(),
        text: "filtered".to_string(),
        tool_output: Some(ToolOutput::Terminal {
            command: "cargo test".to_string(),
            session_id: None,
            exit_code: Some(101),
            filtered_output: "FAILED auth::login [filtered: 320 -> 2 lines]".to_string(),
            raw_output: "RAW_COMPILER_NOISE_LINE_42\nFAILED auth::login".to_string(),
            output_id: Some("out_01".to_string()),
            was_filtered: true,
        }),
    });

    // Initially renders FILTERED view
    let initial_screen = render_screen_text(&app, 110, 28);
    assert!(initial_screen.contains("VIEW: FILTERED"));
    assert!(initial_screen.contains("FAILED auth::login"));
    assert!(!initial_screen.contains("RAW_COMPILER_NOISE_LINE_42"));

    // Press `r` to toggle to RAW view!
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    let raw_screen = render_screen_text(&app, 110, 28);
    assert!(raw_screen.contains("VIEW: RAW"));
    assert!(raw_screen.contains("RAW_COMPILER_NOISE_LINE_42"));
}

#[test]
fn scope_ladder_modal_enforces_broad_grant_confirmation_and_tasks_switcher_steers_subagent() {
    let mut app = TuiApp::new("thr_ui_02", "openai/gpt-5.4-codex", PermissionMode::Build);

    // 1. Emit `ApprovalRequested` with narrow exact rung [0] and broad single-binary rung [1]
    app.apply_session_event(SessionEvent::ApprovalRequested {
        thread_id: "thr_ui_02".to_string(),
        turn_id: "turn_1".to_string(),
        prompt: ApprovalPrompt {
            approval_id: "appr_99".to_string(),
            item_id: "call_99".to_string(),
            tool_name: "exec_command".to_string(),
            summary: "cargo test --workspace".to_string(),
            cwd: None,
            once_only: false,
            ladder: vec![
                ScopeLadderOption {
                    label: "Exact: `cargo test --workspace`".to_string(),
                    scope: ApprovalScope::ExactCommand {
                        command: "cargo test --workspace".to_string(),
                    },
                    broad_grant: false,
                },
                ScopeLadderOption {
                    label: "All `cargo *` commands".to_string(),
                    scope: ApprovalScope::CommandPrefix {
                        tokens: vec!["cargo".to_string()],
                    },
                    broad_grant: true,
                },
            ],
        },
    });

    // Move Down to broad `cargo *` rung, press Tab twice (`Once` -> `Conversation` -> `Project`)
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    // First `Enter` on persistent broad grant MUST NOT immediately approve — it must show secondary confirmation!
    let first_enter = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(first_enter, TuiCommand::Noop);
    let confirm_screen = render_screen_text(&app, 110, 28);
    assert!(confirm_screen.contains("CONFIRM BROAD GRANT"));

    // Second `Enter` confirms `confirmed_broad = true` and emits `ResolveApproval`!
    let second_enter = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        second_enter,
        TuiCommand::Session(SessionCommand::ResolveApproval {
            approval_id: "appr_99".to_string(),
            resolution: ApprovalResolution::Approve {
                scope: Some(ApprovalScope::CommandPrefix {
                    tokens: vec!["cargo".to_string()],
                }),
                lifetime: GrantLifetime::Project,
                confirmed_broad: true,
            },
        })
    );

    // 2. Open `/tasks` (`Ctrl-X`), switch to subagent `task_abc`, and send live `Replace` steering!
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        TuiCommand::OpenTasksOverlay
    );
    app.open_tasks_overlay(vec![SubagentEntry {
        task_id: "task_abc".to_string(),
        parent_session_id: "thr_ui_02".to_string(),
        role: "fast".to_string(),
        model: "openai/gpt-5.4-mini".to_string(),
        background: true,
        status: "running".to_string(),
        summary: "Auditing auth module".to_string(),
    }]);
    app.apply_session_event(SessionEvent::ItemCompleted {
        thread_id: "task_abc".to_string(),
        turn_id: "sub_turn_1".to_string(),
        item_id: "sub_msg_1".to_string(),
        text: "SUBAGENT_LIVE_TRANSCRIPT_LINE".to_string(),
        tool_output: None,
    });
    // Also seed the assistant message start before completion or verify subagent transcript entry:
    app.apply_session_event(SessionEvent::SteeringInjected {
        thread_id: "task_abc".to_string(),
        turn_id: "sub_turn_1".to_string(),
        reason: "note".to_string(),
        message: "SUBAGENT_BANNER_MARKER".to_string(),
    });

    // Highlight `task_abc` (Down), toggle delivery to `Replace` (Tab), type `"focus on jwt"`, press Enter!
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    for ch in "focus on jwt".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let steer_cmd = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        steer_cmd,
        TuiCommand::SteerSubagent {
            task_id: "task_abc".to_string(),
            prompt: "focus on jwt".to_string(),
            delivery: InputDeliveryMode::Replace,
        }
    );

    // Now the TUI is viewing `task_abc`'s transcript!
    assert_eq!(app.viewing_subagent.as_deref(), Some("task_abc"));
    let sub_screen = render_screen_text(&app, 110, 28);
    assert!(sub_screen.contains("Subagent Transcript (task_abc)"));
    assert!(sub_screen.contains("SUBAGENT_BANNER_MARKER"));
}

#[test]
fn utf8_cjk_composer_cursor_editing_and_slash_commands_with_plugin_dispatch() {
    use tekvra_core::ReviewTarget;

    let mut app = TuiApp::new("thr_ui_03", "openai/gpt-5.4-codex", PermissionMode::Build);

    // 1. UTF-8 multi-byte CJK cursor movement, insertion, Delete, Backspace, and Ctrl-J newline
    for ch in "你好世界".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
    assert_eq!(app.input_buffer, "你好A世界");

    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    assert_eq!(app.input_buffer, "你好A界");

    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(app.input_buffer, "你好界");

    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(app.input_buffer, "你\n好界");

    // Clear composer via End + Backspaces
    app.input_buffer.clear();
    app.cursor_byte_pos = 0;

    // 2. `/model` and `/mode` slash commands update status bar and emit SessionCommand
    for ch in "/model anthropic/claude-sonnet-4-5".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Session(SessionCommand::SwitchModel {
            model: "anthropic/claude-sonnet-4-5".to_string(),
        })
    );
    assert_eq!(app.active_model, "anthropic/claude-sonnet-4-5");

    for ch in "/mode plan".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Session(SessionCommand::SetMode {
            mode: PermissionMode::Plan,
        })
    );
    assert_eq!(app.mode, PermissionMode::Plan);

    // 3. `/mcp` and `/review base main`
    for ch in "/mcp".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::ShowMcpStatus
    );
    app.apply_mcp_status("Connected MCP servers: stdio_srv (upper)");

    for ch in "/review base main".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::RunReview {
            target: ReviewTarget::BaseBranch {
                branch: "main".to_string(),
            },
        }
    );

    // 4. Custom Luau slash command `/hello tekvra`
    for ch in "/hello tekvra".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::InvokePluginCommand {
            name: "hello".to_string(),
            args: "tekvra".to_string(),
        }
    );
    app.apply_plugin_command_result("hello", Ok(Some("GREETINGS_FROM_LUAU:tekvra".to_string())));

    let screen = render_screen_text(&app, 120, 30);
    assert!(screen.contains("anthropic/claude-sonnet-4-5"));
    assert!(screen.contains("mode=Plan"));
    assert!(screen.contains("Connected MCP servers: stdio_srv"));
    assert!(screen.contains("GREETINGS_FROM_LUAU:tekvra"));
}

#[test]
fn fuzzy_slash_and_at_file_completion_excludes_secrets_and_sessions_picker_resumes_or_forks() {
    use std::path::PathBuf;
    use tekvra_storage::SessionSummary;

    let mut app = TuiApp::new("thr_main_01", "openai/gpt-5.4-codex", PermissionMode::Build);
    app.set_workspace_files(vec![
        ".git/config".to_string(),
        ".env".to_string(),
        ".env.production".to_string(),
        "secrets/prod.pem".to_string(),
        "crates/tekvra-core/src/session.rs".to_string(),
        "crates/tekvra-tui/src/app.rs".to_string(),
        "README.md".to_string(),
    ]);
    app.set_extra_slash_commands(vec![(
        "deploy".to_string(),
        "Run Luau deploy helper".to_string(),
    )]);

    // Verify secret & `.git` paths were scrubbed from `workspace_files`
    assert_eq!(
        app.workspace_files,
        vec![
            "README.md".to_string(),
            "crates/tekvra-core/src/session.rs".to_string(),
            "crates/tekvra-tui/src/app.rs".to_string(),
        ]
    );

    // 1. Slash command fuzzy completion (`/dep` -> Tab completes `/deploy `)
    for ch in "/dep".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(app.completion.is_some());
    let popup_screen = render_screen_text(&app, 120, 30);
    assert!(popup_screen.contains("/deploy"));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input_buffer, "/deploy ");
    assert!(app.completion.is_none());

    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();

    // 2. `@file` fuzzy completion mid-sentence (`Inspect @core/session` -> Enter inserts `@crates/tekvra-core/src/session.rs `)
    for ch in "Inspect @core/session".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(
        app.completion.is_some(),
        "expected @file popup for `@core/session`"
    );
    let file_popup_screen = render_screen_text(&app, 120, 30);
    assert!(file_popup_screen.contains("@crates/tekvra-core/src/session.rs"));
    assert!(!file_popup_screen.contains(".env"));
    assert!(!file_popup_screen.contains("prod.pem"));

    // Pressing Enter while `@file` popup is active inserts the path without prematurely submitting!
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    assert_eq!(
        app.input_buffer,
        "Inspect @crates/tekvra-core/src/session.rs "
    );

    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();

    // 3. `/sessions` opens interactive session picker and supports Resume & Fork (`Tab`/`f`)
    for ch in "/sessions".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::OpenSessionsOverlay
    );

    app.open_sessions_overlay(vec![
        SessionSummary {
            id: "thr_recent_1".to_string(),
            parent_id: None,
            forked_from_turn_id: None,
            cwd: PathBuf::from("/repo"),
            project_id: "p1".to_string(),
            model: "openai/gpt-5.4-codex".to_string(),
            title: Some("Refactor MCP client".to_string()),
            preview: "Refactor MCP".to_string(),
            archived: false,
            created_at: 100,
            updated_at: 200,
        },
        SessionSummary {
            id: "thr_older_2".to_string(),
            parent_id: None,
            forked_from_turn_id: None,
            cwd: PathBuf::from("/repo"),
            project_id: "p1".to_string(),
            model: "anthropic/claude-sonnet-4-5".to_string(),
            title: None,
            preview: "Audit OAuth PKCE".to_string(),
            archived: false,
            created_at: 50,
            updated_at: 120,
        },
    ]);

    let sessions_screen = render_screen_text(&app, 120, 30);
    assert!(sessions_screen.contains("Refactor MCP client"));
    assert!(sessions_screen.contains("Audit OAuth PKCE"));

    // Navigate Down to `thr_older_2`, toggle Fork mode (`Tab`), and press Enter!
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::SwitchSession {
            session_id: "thr_older_2".to_string(),
            fork: true,
        }
    );
}

#[test]
fn undo_compact_and_skill_commands_route_to_session_runner() {
    let mut app = TuiApp::new("thr_ui_10", "openai/gpt-5.4-codex", PermissionMode::Build);

    for ch in "/undo".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Session(SessionCommand::UndoLastTurn {
            scope: tekvra_protocol::RevertScope::ConversationAndFiles,
        })
    );

    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/compact".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Session(SessionCommand::Compact)
    );

    // `/skill <name> [prompt]` is passed through verbatim to the session runner
    // (which expands SKILL.md instructions against the developer-prefix-invariant frame).
    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/skill review-api Audit the HTTP handlers".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let submitted = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match submitted {
        TuiCommand::Session(SessionCommand::SubmitInput {
            prompt,
            delivery,
            images,
        }) => {
            assert_eq!(prompt, "/skill review-api Audit the HTTP handlers");
            assert_eq!(delivery, InputDeliveryMode::Next);
            assert!(images.is_empty());
        }
        other => panic!("expected SubmitInput passthrough, got {other:?}"),
    }

    // Unknown `/trust <bogus>` reports usage without dispatching.
    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/trust bogus".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    // `/trust status` dispatches the trust hook command.
    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/trust status".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::RunTrust {
            action: tekvra_tui::TrustUiCommand::Status,
        }
    );
}

#[test]
fn skill_name_completion_fuzzy_filters_discovered_skills() {
    let mut app = TuiApp::new("thr_ui_11", "openai/gpt-5.4-codex", PermissionMode::Build);
    app.set_discovered_skills(vec![
        ("review-api".to_string(), "API review checklist".to_string()),
        ("refactor".to_string(), "Refactor helpers".to_string()),
    ]);

    for ch in "/skill rev".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(app.completion.is_some(), "expected /skill argument popup");
    let screen = render_screen_text(&app, 120, 30);
    assert!(screen.contains("review-api"));
    assert!(
        !screen.contains("refactor"),
        "non-matching skill must be filtered out"
    );

    // Enter accepts the highlighted skill into the composer without submitting.
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    assert_eq!(app.input_buffer, "/skill review-api");
}

#[test]
fn bare_model_command_opens_fuzzy_picker_modal() {
    let mut app = TuiApp::new("thr_ui_12", "openai/gpt-5.4-codex", PermissionMode::Build);
    app.set_model_catalog(
        vec![
            ("openai/gpt-4o".to_string(), "OpenAI GPT-4o".to_string()),
            (
                "anthropic/claude-sonnet-4-5".to_string(),
                "Anthropic Claude".to_string(),
            ),
        ],
        128_000,
    );

    for ch in "/model".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    let screen = render_screen_text(&app, 120, 30);
    assert!(screen.contains("Model Picker"));
    assert!(screen.contains("openai/gpt-4o"));
    assert!(screen.contains("anthropic/claude-sonnet-4-5"));

    // Fuzzy filter narrows the list; Enter dispatches SwitchModel and closes the modal.
    for ch in "claude".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let filtered = render_screen_text(&app, 120, 30);
    assert!(
        !filtered.contains("openai/gpt-4o"),
        "filter must hide non-matching models"
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Session(SessionCommand::SwitchModel {
            model: "anthropic/claude-sonnet-4-5".to_string(),
        })
    );
    assert_eq!(app.active_model, "anthropic/claude-sonnet-4-5");

    // With no catalog, bare `/model` stays a banner instead of a modal.
    let mut bare = TuiApp::new("thr_ui_12b", "openai/gpt-5.4-codex", PermissionMode::Build);
    for ch in "/model".chars() {
        bare.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        bare.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
    assert!(matches!(bare.overlay, tekvra_tui::ActiveOverlay::None));
}

#[test]
fn copy_command_extracts_latest_reply_and_code_block() {
    use tekvra_tui::TranscriptEntry;
    let mut app = TuiApp::new("thr_ui_13", "openai/gpt-5.4-codex", PermissionMode::Build);

    // No assistant output yet -> error banner, no dispatch.
    for ch in "/copy".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );

    app.main_transcript.push(TranscriptEntry::AssistantMessage {
        item_id: "item_1".to_string(),
        reasoning: String::new(),
        markdown: "Here is the patch:\n```rust\nSELECTED_CODE_BODY\n```".to_string(),
    });

    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/copy code".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let copied = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match copied {
        TuiCommand::CopyToClipboard { text, label } => {
            assert_eq!(text, "SELECTED_CODE_BODY");
            assert_eq!(label, "code block");
        }
        other => panic!("expected CopyToClipboard, got {other:?}"),
    }

    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/copy".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
        TuiCommand::CopyToClipboard { text, label } => {
            assert!(text.contains("Here is the patch:"));
            assert_eq!(label, "reply");
        }
        other => panic!("expected full-reply CopyToClipboard, got {other:?}"),
    }

    // `/copy code` without any fence reports a soft error.
    app.main_transcript.push(TranscriptEntry::AssistantMessage {
        item_id: "item_2".to_string(),
        reasoning: String::new(),
        markdown: "plain prose only".to_string(),
    });
    app.input_buffer.clear();
    app.cursor_byte_pos = 0;
    app.refresh_completions();
    for ch in "/copy code".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        TuiCommand::Noop
    );
}

#[test]
fn mouse_wheel_scrolls_transcript_without_touching_composer() {
    let mut app = TuiApp::new("thr_ui_14", "openai/gpt-5.4-codex", PermissionMode::Build);
    for ch in "hello world".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.scroll_offset = 10;

    let down = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(app.handle_mouse(down), TuiCommand::Noop);
    assert_eq!(app.scroll_offset, 13);
    assert_eq!(app.input_buffer, "hello world");

    let up = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollUp,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(up);
    app.handle_mouse(up);
    assert_eq!(app.scroll_offset, 7);
    assert_eq!(app.cursor_byte_pos, "hello world".len());
}

#[test]
fn compaction_banner_and_status_bar_render_telemetry() {
    let mut app = TuiApp::new("thr_ui_15", "openai/gpt-5.4-codex", PermissionMode::Build);
    app.set_model_catalog(Vec::new(), 128_000);
    app.apply_session_event(SessionEvent::CompactionCompleted {
        thread_id: "thr_ui_15".to_string(),
        generation: 3,
        before_tokens: 1_000,
        after_tokens: 400,
    });
    let screen = render_screen_text(&app, 120, 30);
    assert!(
        screen.contains("Context compacted to generation 3"),
        "compaction banner must render: {screen}"
    );
    assert!(screen.contains("1000 -> 400"));

    // Status bar context-utilization percentage against the model context window.
    app.apply_session_event(SessionEvent::TokenUsageUpdated {
        turn_id: "turn_1".to_string(),
        thread_id: "thr_ui_15".to_string(),
        usage: tekvra_protocol::TokenUsage {
            input_tokens: 64_000,
            cached_input_tokens: 32_000,
            output_tokens: 1_000,
            reasoning_tokens: 0,
            cost_usd: 0.25,
        },
    });
    let with_usage = render_screen_text(&app, 140, 30);
    assert!(
        with_usage.contains("ctx 50%"),
        "status bar must show ctx%: {with_usage}"
    );
    assert!(with_usage.contains("cache 50%"));
}

#[test]
fn question_modal_answers_prompts_and_search_enter_jumps_to_session() {
    let mut app = TuiApp::new("thr_ui_11", "openai/gpt-5.4-codex", PermissionMode::Build);

    // 1. `UserInputRequested` opens the Question modal listing both prompts.
    let ask = |app: &mut TuiApp| {
        app.apply_session_event(SessionEvent::UserInputRequested {
            thread_id: "thr_ui_11".to_string(),
            turn_id: "turn_q".to_string(),
            item_id: "q_abc".to_string(),
            questions: vec![
                ("q0".to_string(), "Which database?".to_string()),
                ("q1".to_string(), "Migrate now?".to_string()),
            ],
        });
    };
    ask(&mut app);
    let screen = render_screen_text(&app, 90, 24);
    assert!(screen.contains("Which database?"), "{screen}");
    assert!(screen.contains("Migrate now?"));

    // Type `sqlite` on q0, Tab to q1, type `yes`, Enter submits both drafts.
    for ch in "sqlite".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    for ch in "yes".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let submit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        submit,
        TuiCommand::Session(SessionCommand::ResolveUserInput {
            item_id: "q_abc".to_string(),
            answers: std::collections::BTreeMap::from([
                ("q0".to_string(), vec!["sqlite".to_string()]),
                ("q1".to_string(), vec!["yes".to_string()]),
            ]),
        })
    );
    assert!(matches!(app.overlay, ActiveOverlay::None));

    // 2. Esc dismisses the modal but STILL resolves (empty drafts) so the turn cannot hang.
    ask(&mut app);
    let dismiss = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        dismiss,
        TuiCommand::Session(SessionCommand::ResolveUserInput {
            item_id: "q_abc".to_string(),
            answers: std::collections::BTreeMap::new(),
        })
    );

    // 3. `/search` results jump: Enter on a selected hit switches to that session.
    app.overlay = ActiveOverlay::Search(SearchOverlayState {
        query: "needle".to_string(),
        hits: vec![tekvra_storage::HistorySearchHit {
            session_id: "thr_old_42".to_string(),
            turn_id: "turn_7".to_string(),
            role: "assistant".to_string(),
            snippet: "needle in haystack".to_string(),
        }],
        selected: 0,
    });
    let jump = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        jump,
        TuiCommand::SwitchSession {
            session_id: "thr_old_42".to_string(),
            fork: false,
        }
    );
}
