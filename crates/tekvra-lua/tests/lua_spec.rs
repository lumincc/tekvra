//! Specification tests for `tekvra-lua` (Task 6.2):
//! - Zero VM initialization by default (`experimental.lua_plugins = false`) and under `--no-plugins`
//! - Migration notice when `init.lua` exists while disabled
//! - Per-plugin `plugin.toml` capability denial on `tekvra.fs.read_bytes` when `fs_read` is absent
//! - Live `/reload`, `--no-jit` interpreter mode, and Luau GC drop safety (no panics on `Lua` drop)

use serde_json::json;
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::PluginToolExecutor;
use tekvra_lua::{LuaHostOptions, PluginHost};
use tempfile::TempDir;

#[tokio::test]
async fn skipped_vm_by_default_with_migration_notice_and_no_plugins_override() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    // Place a global `init.lua` that would panic if evaluated!
    std::fs::write(
        cfg_dir.join("init.lua"),
        "error('MUST NOT RUN BY DEFAULT!')\n",
    )
    .expect("write init.lua");

    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir.clone(),
            state_dir: state_dir.clone(),
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load default config");

    let (host, report) =
        PluginHost::initialize(resolved, LuaHostOptions::default()).expect("init default host");
    assert!(
        !host.is_vm_initialized(),
        "Lua VM MUST NOT initialize when experimental.lua_plugins is false"
    );
    assert!(!report.vm_initialized);
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("experimental.lua_plugins")),
        "expected migration notice when init.lua exists while disabled: {:?}",
        report.notices
    );

    // Even when global `tekvra.toml` sets `experimental.lua_plugins = true`, `--no-plugins` suppresses VM startup!
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");

    let resolved_enabled = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir,
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load enabled config");

    let (suppressed_host, suppressed_report) = PluginHost::initialize(
        resolved_enabled,
        LuaHostOptions {
            no_plugins: true,
            no_jit: false,
        },
    )
    .expect("init with --no-plugins");
    assert!(!suppressed_host.is_vm_initialized());
    assert!(!suppressed_report.vm_initialized);
}

#[tokio::test]
async fn plugin_toml_permission_denial_on_read_bytes_live_reload_no_jit_and_gc_drop_safety() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let plugin_dir = cfg_dir.join("lua").join("demo");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws/.git");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    std::fs::write(ws.join("notes.txt"), "HELLO_FROM_WORKSPACE").expect("write notes.txt");
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");

    let mut trust = TrustStore::load(&state_dir).expect("trust");
    trust.grant_trust(&ws).expect("grant trust");

    // 1. Write plugin WITHOUT `plugin.toml` that registers a tool attempting `tekvra.fs.read_bytes("notes.txt")`
    std::fs::write(plugin_dir.join("helper.lua"), "return { prefix = 'v1:' }\n")
        .expect("write helper.lua");
    std::fs::write(
        plugin_dir.join("init.lua"),
        r#"
local helper = require("helper")

tekvra.api.register_tool({
    name = "demo_reader",
    description = "Reads notes.txt via tekvra.fs.read_bytes",
    handler = function(_args)
        local raw = tekvra.fs.read_bytes("notes.txt")
        return helper.prefix .. raw
    end,
})

tekvra.api.register_command({
    name = "ping",
    description = "Returns pong",
    handler = function(arg)
        return helper.prefix .. "pong:" .. arg
    end,
})

tekvra.keymap.set("n", "<C-p>", "ping")
"#,
    )
    .expect("write plugin init.lua");

    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir,
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");

    let (host, report) = PluginHost::initialize(
        resolved,
        LuaHostOptions {
            no_plugins: false,
            no_jit: true, // Verify `--no-jit` interpreter mode!
        },
    )
    .expect("initialize host");
    assert!(host.is_vm_initialized());
    assert!(!report.jit_enabled);
    assert_eq!(report.loaded_plugins, vec!["demo".to_string()]);
    assert_eq!(host.registered_tools().len(), 1);
    assert_eq!(host.registered_keymaps().len(), 1);
    assert!(host.has_command("ping"));
    assert!(!host.has_command("nonexistent"));
    assert_eq!(
        host.invoke_command("ping", "42").expect("ping command"),
        "v1:pong:42"
    );

    // Calling `demo_reader` WITHOUT `fs_read` in `plugin.toml` MUST fail with permission denied!
    let denied = host
        .execute_plugin_tool("demo_reader", &json!({}))
        .await
        .expect_err("must deny tekvra.fs.read_bytes without plugin.toml fs_read");
    assert!(
        denied.contains("permission denied") && denied.contains("fs_read"),
        "got: {denied}"
    );

    // 2. Grant `fs_read = ["notes.txt"]` in `plugin.toml`, update `helper.lua` to `v2:`, and call `/reload`!
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"demo\"\nversion = \"0.2.0\"\n\n[permissions]\nfs_read = [\"notes.txt\"]\n",
    )
    .expect("write plugin.toml");
    std::fs::write(plugin_dir.join("helper.lua"), "return { prefix = 'v2:' }\n")
        .expect("update helper.lua");

    let reload_report = host.reload().expect("live /reload");
    assert!(reload_report.vm_initialized);

    let allowed = host
        .execute_plugin_tool("demo_reader", &json!({}))
        .await
        .expect("read_bytes succeeds after plugin.toml grant + /reload");
    assert_eq!(allowed.to_model_text(), "v2:HELLO_FROM_WORKSPACE");

    // Explicitly drop `host` to verify safe RegistryKey cleanup before `mlua::Lua` drop (zero GC panic)
    drop(host);
}

/// Boots a plugin-enabled host with the given `init.lua` under `<cfg>/lua/hooks/`.
fn init_hooks_host(tmp: &TempDir, init_lua: &str) -> PluginHost {
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let plugin_dir = cfg_dir.join("lua").join("hooks");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws/.git");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");

    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");
    std::fs::write(plugin_dir.join("init.lua"), init_lua).expect("write init.lua");

    let mut trust = TrustStore::load(&state_dir).expect("trust");
    trust.grant_trust(&ws).expect("grant trust");

    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir,
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");

    let (host, _report) =
        PluginHost::initialize(resolved, LuaHostOptions::default()).expect("initialize hooks host");
    host
}

#[tokio::test]
async fn hooks_registration_notes_deny_and_failure_isolation() {
    let tmp = TempDir::new().expect("tempdir");
    let init_lua = r#"
tekvra.api.register_hook("post_tool_use", function(payload)
    return "post:" .. payload.tool
end)

tekvra.api.register_hook("pre_tool_use", function(payload)
    if payload.tool == "exec_command" then
        return { decision = "deny", reason = "shell is forbidden" }
    end
    return nil
end)

tekvra.api.register_hook("turn_started", function(_payload)
    error("hook handler exploded")
end)

tekvra.api.register_hook("turn_completed", function(_payload)
    return nil
end)
"#;
    let host = init_hooks_host(&tmp, init_lua);
    use tekvra_core::{HookEvent, HookEventPayload};

    // post_tool_use handler returns a note carrying the tool name.
    let outcome = host.dispatch_hook(
        HookEvent::PostToolUse,
        HookEventPayload {
            tool: Some("file_write".to_string()),
            arguments: Some("{\"path\":\"a.rs\"}".to_string()),
            call_id: Some("call_1".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert!(outcome.denied.is_none());
    assert_eq!(outcome.notes, vec!["post:file_write".to_string()]);
    assert!(outcome.failures.is_empty());

    // pre_tool_use denies `exec_command` with the plugin-provided reason.
    let denied = host.dispatch_hook(
        HookEvent::PreToolUse,
        HookEventPayload {
            tool: Some("exec_command".to_string()),
            arguments: Some("{\"command\":\"rm -rf build\"}".to_string()),
            call_id: Some("call_2".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert_eq!(denied.denied.as_deref(), Some("shell is forbidden"));

    // pre_tool_use passes through for other tools (deny table only for exec_command).
    let allowed = host.dispatch_hook(
        HookEvent::PreToolUse,
        HookEventPayload {
            tool: Some("file_read".to_string()),
            arguments: Some("{}".to_string()),
            call_id: Some("call_3".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert!(allowed.denied.is_none());

    // turn_completed has only a nil-returning handler: empty outcome.
    let empty = host.dispatch_hook(
        HookEvent::TurnCompleted,
        HookEventPayload {
            turn_id: Some("turn_x".to_string()),
            status: Some("completed".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert!(empty.notes.is_empty() && empty.failures.is_empty() && empty.denied.is_none());

    // turn_started handler raises a Lua error: failure-isolated, never a panic.
    let failing = host.dispatch_hook(
        HookEvent::TurnStarted,
        HookEventPayload {
            turn_id: Some("turn_y".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert!(failing.notes.is_empty());
    assert!(failing.denied.is_none());
    assert!(
        failing
            .failures
            .iter()
            .any(|f| f.contains("hook handler exploded")),
        "{:?}",
        failing.failures
    );
}

#[tokio::test]
async fn hook_instruction_budget_aborts_runaway_handler() {
    let tmp = TempDir::new().expect("tempdir");
    let init_lua = r#"
tekvra.api.register_hook("turn_started", function(_payload)
    while true do end
end)
"#;
    let host = init_hooks_host(&tmp, init_lua);
    use tekvra_core::{HookEvent, HookEventPayload};

    let outcome = host.dispatch_hook(
        HookEvent::TurnStarted,
        HookEventPayload {
            turn_id: Some("turn_z".to_string()),
            ..HookEventPayload::default()
        },
    );
    assert!(outcome.notes.is_empty() && outcome.denied.is_none());
    assert!(
        outcome
            .failures
            .iter()
            .any(|f| f.contains("instruction budget exceeded")),
        "expected budget abort, got {:?}",
        outcome.failures
    );
    // Failure isolation: the VM stays alive for later dispatches.
    let after = host.dispatch_hook(HookEvent::SessionStart, HookEventPayload::default());
    assert!(after.failures.is_empty() && after.notes.is_empty());
}

#[tokio::test]
async fn unknown_hook_event_rejected_at_registration_time() {
    let tmp = TempDir::new().expect("tempdir");
    let init_lua = r#"
tekvra.api.register_hook("session_end", function(_payload)
    return "never"
end)
"#;
    let ws = tmp.path().join("repo");
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let plugin_dir = cfg_dir.join("lua").join("hooks");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws/.git");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");
    std::fs::write(plugin_dir.join("init.lua"), init_lua).expect("write init.lua");
    let mut trust = TrustStore::load(&state_dir).expect("trust");
    trust.grant_trust(&ws).expect("grant trust");
    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir,
            state_dir,
            home_dir: Some(ws.clone()),
        },
        &ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");

    let err = PluginHost::initialize(resolved, LuaHostOptions::default())
        .map(|_| ())
        .expect_err("unknown event must fail plugin load");
    assert!(err.to_string().contains("unknown hook event"), "got: {err}");
}
