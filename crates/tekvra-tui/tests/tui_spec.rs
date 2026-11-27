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
use tekvra_tui::{CodeHighlighter, TuiApp, TuiCommand};

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
