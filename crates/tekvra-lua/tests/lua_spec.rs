//! Specification tests for `tekvra-lua` (Task 6.2):
//! - Zero VM initialization by default (`experimental.lua_plugins = false`) and under `--no-plugins`
//! - Migration notice when `init.lua` exists while disabled
//! - Per-plugin `plugin.toml` capability denial on `tekvra.fs.read_bytes` when `fs_read` is absent
//! - Live `/reload`, `--no-jit` interpreter mode, and Luau GC drop safety (no panics on `Lua` drop)
//! - Real `tekvra.net.get` (SSRF-safe, per-hop `net` grants, 64 KiB body cap)
//! - `tekvra.keymap.set` validation and per-plugin load isolation

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tekvra_config::{CliConfigOverrides, ConfigPaths, TrustStore, load_config};
use tekvra_core::PluginToolExecutor;
use tekvra_lua::{LuaHostOptions, PluginHost};
use tekvra_protocol::{KeymapMode, KeymapSpec, parse_key_chord};
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
            ..LuaHostOptions::default()
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

tekvra.keymap.set("n", "<C-p>", "/ping")
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
            ..LuaHostOptions::default()
        },
    )
    .expect("initialize host");
    assert!(host.is_vm_initialized());
    assert!(!report.jit_enabled);
    assert_eq!(report.loaded_plugins, vec!["demo".to_string()]);
    assert_eq!(host.registered_tools().len(), 1);
    let keymaps = host.registered_keymaps();
    assert_eq!(keymaps.len(), 1);
    assert_eq!(keymaps[0].mode, KeymapMode::Normal);
    assert_eq!(keymaps[0].chord, parse_key_chord("<C-p>").expect("chord"));
    assert_eq!(keymaps[0].command, "/ping");
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

    let (host, report) = PluginHost::initialize(resolved, LuaHostOptions::default())
        .expect("a failing plugin is reported as a notice, never fatal");
    assert!(report.vm_initialized, "the VM itself still starts");
    assert!(
        report.loaded_plugins.is_empty(),
        "the failing plugin must not be listed as loaded: {:?}",
        report.loaded_plugins
    );
    let notice = report
        .notices
        .iter()
        .find(|n| n.contains("Failed to load plugin `hooks`"))
        .expect("notice naming the failed plugin");
    assert!(
        notice.contains("unknown hook event") && notice.contains("registrations were discarded"),
        "got: {notice}"
    );
    assert!(
        host.registered_tools().is_empty()
            && host.registered_commands().is_empty()
            && host.registered_keymaps().is_empty()
    );
}

/// Boots a plugin-enabled host over the plugins already written under `<cfg>/lua/<name>/`.
fn boot_host(
    ws: &Path,
    cfg_dir: &Path,
    state_dir: &Path,
    options: LuaHostOptions,
) -> (PluginHost, tekvra_lua::PluginLoadReport) {
    std::fs::create_dir_all(cfg_dir).expect("mkdir cfg");
    std::fs::create_dir_all(state_dir).expect("mkdir state");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir ws/.git");
    std::fs::write(
        cfg_dir.join("tekvra.toml"),
        "version = 2\n[experimental]\nlua_plugins = true\n",
    )
    .expect("write tekvra.toml");
    let mut trust = TrustStore::load(state_dir).expect("trust");
    trust.grant_trust(ws).expect("grant trust");
    let resolved = load_config(
        ConfigPaths {
            global_config_dir: cfg_dir.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            home_dir: Some(ws.to_path_buf()),
        },
        ws,
        &CliConfigOverrides::default(),
    )
    .expect("load config");
    PluginHost::initialize(resolved, options).expect("initialize host")
}

/// Writes `<cfg>/lua/<name>/{init.lua,plugin.toml}` (manifest optional).
fn write_plugin(cfg_dir: &Path, name: &str, init_lua: &str, manifest: Option<&str>) {
    let dir = cfg_dir.join("lua").join(name);
    std::fs::create_dir_all(&dir).expect("mkdir plugin");
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    if let Some(manifest) = manifest {
        std::fs::write(dir.join("plugin.toml"), manifest).expect("write plugin.toml");
    }
}

/// Minimal HTTP/1.1 server on `127.0.0.1:<ephemeral>` recording request paths. `/redirect`
/// answers `302` to `http://localhost:<same port>/secret` so the redirect hop has a different
/// host string than the granted `127.0.0.1`.
fn spawn_loopback_http(requests: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let mut raw = Vec::new();
                loop {
                    let Ok(n) = stream.read(&mut buf) else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                requests.lock().unwrap().push(path.clone());
                let body: Vec<u8> = match path.as_str() {
                    "/hello" => b"HELLO_FROM_NET".to_vec(),
                    "/secret" => b"SECRET".to_vec(),
                    "/big" => vec![b'a'; 70 * 1024],
                    _ => Vec::new(),
                };
                let response = if path == "/redirect" {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });
    port
}

#[tokio::test]
async fn net_get_fetches_through_ssrf_safe_fetcher_with_per_hop_grants_and_body_cap() {
    let tmp = TempDir::new().expect("tempdir");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_loopback_http(Arc::clone(&requests));
    let cfg_dir = tmp.path().join("cfg");
    let ws = tmp.path().join("repo");
    let state_dir = tmp.path().join("state");
    for (name, manifest) in [
        (
            "nettest",
            Some("name = \"nettest\"\n\n[permissions]\nnet = [\"127.0.0.1\"]\n"),
        ),
        ("nonet", None),
        (
            "star",
            Some("name = \"star\"\n\n[permissions]\nnet = [\"*\"]\n"),
        ),
    ] {
        write_plugin(
            &cfg_dir,
            name,
            &format!(
                "tekvra.api.register_command({{ name = \"fetch_{name}\", description = \"GET\", handler = function(arg) return tekvra.net.get(arg) end }})\n"
            ),
            manifest,
        );
    }
    let loopback_admitted = LuaHostOptions {
        net_ssrf_allowlist: vec!["127.0.0.1".to_string()],
        ..LuaHostOptions::default()
    };
    let (host, report) = boot_host(&ws, &cfg_dir, &state_dir, loopback_admitted.clone());
    assert_eq!(report.loaded_plugins.len(), 3, "{:?}", report.notices);
    let hello = format!("http://127.0.0.1:{port}/hello");

    // 1. Granted host: a real response, not the old `authorized:<url>` stub.
    let text = host
        .invoke_command("fetch_nettest", &hello)
        .expect("granted fetch");
    assert!(
        text.starts_with("HTTP 200 ") && text.contains("HELLO_FROM_NET"),
        "{text}"
    );

    // 2. Redirect to a host that is not granted: denied before any request reaches it.
    let denied = host
        .invoke_command(
            "fetch_nettest",
            &format!("http://127.0.0.1:{port}/redirect"),
        )
        .expect_err("redirect hop must be denied");
    let denied = denied.to_string();
    assert!(
        denied.contains("permission denied") && denied.contains("redirect host `localhost`"),
        "{denied}"
    );
    assert!(
        !requests.lock().unwrap().iter().any(|p| p == "/secret"),
        "the non-granted hop must never be contacted: {:?}",
        requests
    );

    // 3. No `net` grant at all: nothing leaves the process.
    let before = requests.lock().unwrap().len();
    let no_grant = host
        .invoke_command("fetch_nonet", &hello)
        .expect_err("missing grant");
    let no_grant = no_grant.to_string();
    assert!(
        no_grant.contains("permission denied")
            && no_grant.contains("`net` capability for host `127.0.0.1`")
            && no_grant.contains("plugin `nonet`"),
        "{no_grant}"
    );
    assert_eq!(requests.lock().unwrap().len(), before);

    // 4. Grants never bypass SSRF policy: a host without the loopback admission blocks it.
    let (strict_host, _) = boot_host(&ws, &cfg_dir, &state_dir, LuaHostOptions::default());
    let ssrf = strict_host
        .invoke_command("fetch_star", &hello)
        .expect_err("loopback must stay blocked");
    assert!(ssrf.to_string().contains("blocked SSRF"), "{ssrf}");
    assert_eq!(requests.lock().unwrap().len(), before);

    // 5. Bodies over 64 KiB are capped with a marker; the status line stays intact.
    let big = host
        .invoke_command("fetch_nettest", &format!("http://127.0.0.1:{port}/big"))
        .expect("big fetch");
    assert_eq!(
        big,
        format!(
            "HTTP 200 http://127.0.0.1:{port}/big\n\n{}{}",
            "a".repeat(64 * 1024),
            "\n...[net.get: body truncated at 64 KiB]"
        )
    );
}

#[tokio::test]
async fn keymap_set_is_validated_at_load_and_exposes_parsed_specs() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_dir = tmp.path().join("cfg");
    let ws = tmp.path().join("repo");
    let state_dir = tmp.path().join("state");
    for (name, init_lua) in [
        (
            "good",
            "tekvra.keymap.set(\"n\", \"<C-g>\", \"/review\")\ntekvra.keymap.set(\"*\", \"<A-F4>\", \"/hello world\")\n",
        ),
        ("badmode", "tekvra.keymap.set(\"v\", \"<C-g>\", \"/x\")\n"),
        ("badchord", "tekvra.keymap.set(\"n\", \"<a>\", \"/x\")\n"),
        (
            "badreserved",
            "tekvra.keymap.set(\"n\", \"<C-c>\", \"/x\")\n",
        ),
        (
            "badcommand",
            "tekvra.keymap.set(\"n\", \"<C-g>\", \"ping\")\n",
        ),
    ] {
        write_plugin(&cfg_dir, name, init_lua, None);
    }
    let (host, report) = boot_host(&ws, &cfg_dir, &state_dir, LuaHostOptions::default());
    assert_eq!(report.loaded_plugins, vec!["good".to_string()]);
    let keymaps = host.registered_keymaps();
    assert_eq!(
        keymaps,
        vec![
            KeymapSpec::parse("n", "<C-g>", "/review").expect("spec"),
            KeymapSpec::parse("*", "<A-F4>", "/hello world").expect("spec"),
        ]
    );
    assert_eq!(report.notices.len(), 4, "{:?}", report.notices);
    for (plugin, reason) in [
        ("badmode", "invalid keymap mode `v`"),
        ("badchord", "a bare key cannot be bound"),
        ("badreserved", "reserved for interrupt / quit"),
        ("badcommand", "expected a slash command"),
    ] {
        let notice = report
            .notices
            .iter()
            .find(|n| n.contains(&format!("Failed to load plugin `{plugin}`")))
            .unwrap_or_else(|| panic!("no notice for {plugin}: {:?}", report.notices));
        assert!(notice.contains(reason), "{plugin}: {notice}");
    }
}

#[tokio::test]
async fn failing_plugin_is_isolated_and_its_registrations_rolled_back() {
    use tekvra_core::{HookEvent, HookEventPayload};

    let tmp = TempDir::new().expect("tempdir");
    let cfg_dir = tmp.path().join("cfg");
    let ws = tmp.path().join("repo");
    let state_dir = tmp.path().join("state");
    // `a_broken` registers a command and a hook BEFORE its failing keymap call: all of it
    // must vanish. `b_works` loads after it and must be unaffected.
    write_plugin(
        &cfg_dir,
        "a_broken",
        r#"tekvra.api.register_command({ name = "ghost", description = "", handler = function() return "boo" end })
tekvra.api.register_hook("session_start", function() return "ghost note" end)
tekvra.keymap.set("v", "<C-g>", "/x")
"#,
        None,
    );
    write_plugin(
        &cfg_dir,
        "b_works",
        r#"tekvra.api.register_command({ name = "alive", description = "", handler = function() return "alive" end })
tekvra.keymap.set("i", "<C-g>", "/alive")
"#,
        None,
    );
    let (host, report) = boot_host(&ws, &cfg_dir, &state_dir, LuaHostOptions::default());
    assert_eq!(report.loaded_plugins, vec!["b_works".to_string()]);
    let notice = report
        .notices
        .iter()
        .find(|n| n.contains("Failed to load plugin `a_broken`"))
        .expect("notice naming the broken plugin");
    assert!(
        notice.contains("invalid keymap mode `v`")
            && notice.contains("registrations were discarded"),
        "{notice}"
    );
    assert!(!host.has_command("ghost"));
    assert!(host.has_command("alive"));
    assert_eq!(host.invoke_command("alive", "").expect("alive"), "alive");
    let keymaps = host.registered_keymaps();
    assert_eq!(keymaps.len(), 1);
    assert_eq!(keymaps[0].command, "/alive");
    let outcome = host.dispatch_hook(HookEvent::SessionStart, HookEventPayload::default());
    assert!(
        outcome.notes.is_empty() && outcome.failures.is_empty(),
        "{outcome:?}"
    );
}
