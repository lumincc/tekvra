//! Implementations behind the `tekvra trust|mcp|completion|worktree` subcommands and the
//! shared `-i/--image` attachment loader.
//!
//! These operations intentionally short-circuit before plugin / MCP / session initialization
//! (mirroring `tekvra auth`), so they stay fast, offline, and safe to run from shell scripts.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use clap::Command as ClapCommand;
use tekvra_config::{
    CliConfigOverrides, ConfigPaths, McpServerConfig, ResolvedConfig, TrustStore,
    canonicalize_or_clean, load_config,
};
use tekvra_protocol::InputImage;
use tekvra_storage::{StateIndex, UsageGroupBy, UsageRow};
use tekvra_tools::{ConfiguredMcpGateway, McpGateway, mcp_catalog_lines};

/// Maximum accepted image attachment size per file (10 MiB).
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Parsed `tekvra usage` invocation.
#[derive(Debug, Clone)]
pub struct UsageOptions {
    /// Optional trailing window in days.
    pub days: Option<u32>,
    /// Grouping key (model default).
    pub by: UsageGroupBy,
    /// Emit JSON instead of a table.
    pub json: bool,
    /// Optional CSV export path (full per-turn rows).
    pub csv: Option<PathBuf>,
}

/// Parses the `--by` string into a [`UsageGroupBy`].
///
/// # Errors
/// Returns an error for unknown grouping keys.
pub fn parse_usage_group_by(by: &str) -> Result<UsageGroupBy> {
    match by {
        "model" => Ok(UsageGroupBy::Model),
        "day" => Ok(UsageGroupBy::Day),
        "session" => Ok(UsageGroupBy::Session),
        other => bail!("unknown --by grouping `{other}`; expected model|day|session"),
    }
}

/// Formats usage rows as an aligned ASCII table (costs to 4 decimals).
pub fn format_usage_table(rows: &[UsageRow]) -> String {
    if rows.is_empty() {
        return "no usage recorded.\n".to_string();
    }
    let mut out = String::new();
    let key_w = rows
        .iter()
        .map(|r| r.key.len())
        .max()
        .unwrap_or(3)
        .max("KEY".len());
    let num_w = 10usize;
    out.push_str(&format!(
        "{:<key_w$}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {:>10}\n",
        "KEY", "INPUT", "CACHED", "OUTPUT", "REASON", "COST"
    ));
    for r in rows {
        out.push_str(&format!(
            "{:<key_w$}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {:>10.4}\n",
            r.key,
            r.input_tokens,
            r.cached_input_tokens,
            r.output_tokens,
            r.reasoning_tokens,
            r.cost_usd
        ));
    }
    out
}

/// Escapes one CSV field per RFC 4180 (quote-wrap on comma/quote/newline).
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Serializes per-turn usage rows as CSV (header + one line per record).
pub fn format_usage_csv(rows: &[tekvra_storage::UsageRecordRow]) -> String {
    let mut out = String::from(
        "session_id,turn_id,model,input_tokens,cached_input_tokens,output_tokens,reasoning_tokens,cost_usd,recorded_at\n",
    );
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            csv_field(&r.session_id),
            csv_field(&r.turn_id),
            csv_field(&r.model),
            r.input_tokens,
            r.cached_input_tokens,
            r.output_tokens,
            r.reasoning_tokens,
            r.cost_usd,
            r.recorded_at,
        ));
    }
    out
}

/// Runs `tekvra usage` against the local ledger (read-only; short-circuits before plugins).
///
/// # Errors
/// Returns an error when the state index cannot be opened or the CSV target cannot be written.
pub fn handle_usage_command(paths: &ConfigPaths, options: &UsageOptions) -> Result<()> {
    let index = StateIndex::open(&paths.state_dir).context("opening state index")?;

    if let Some(csv_path) = &options.csv {
        let rows = index.usage_rows_since(options.days)?;
        let body = format_usage_csv(&rows);
        std::fs::write(csv_path, body)
            .with_context(|| format!("writing CSV to {}", csv_path.display()))?;
        println!("wrote {} usage rows to {}", rows.len(), csv_path.display());
        return Ok(());
    }

    let rows = index.usage_aggregate(options.days, options.by)?;
    if options.json {
        let value = serde_json::json!({
            "days": options.days,
            "by": match options.by {
                UsageGroupBy::Model => "model",
                UsageGroupBy::Day => "day",
                UsageGroupBy::Session => "session",
            },
            "rows": rows
                .iter()
                .map(|r| serde_json::json!({
                    "key": r.key,
                    "input_tokens": r.input_tokens,
                    "cached_input_tokens": r.cached_input_tokens,
                    "output_tokens": r.output_tokens,
                    "reasoning_tokens": r.reasoning_tokens,
                    "cost_usd": r.cost_usd,
                }))
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("serializing usage JSON")?
        );
        return Ok(());
    }
    print!("{}", format_usage_table(&rows));
    Ok(())
}

/// Runs `tekvra memory` — print the project MEMORY.md (path + content), or `--append TEXT`
/// to add one dated entry. Stateless: short-circuits before plugins/session init.
///
/// # Errors
/// Returns an error when appending fails (size caps, unwritable path).
pub fn handle_memory_command(paths: &ConfigPaths, cwd: &Path, append: Option<&str>) -> Result<()> {
    let store = tekvra_tools::MemoryStore::open(&paths.state_dir, cwd);
    match append {
        Some(entry) => {
            store.append(entry)?;
            println!("saved to {}", store.path().display());
        }
        None => {
            println!("memory file: {}", store.path().display());
            let content = store.load();
            if content.trim().is_empty() {
                println!("(empty)");
            } else {
                println!();
                print!("{content}");
            }
        }
    }
    Ok(())
}

/// One `tekvra doctor` health-check result.
#[derive(Debug, Clone)]
pub struct DoctorCheck {
    /// Short check name.
    pub name: String,
    /// Outcome (`ok`, `warn`, `fail`).
    pub status: &'static str,
    /// Human detail line.
    pub detail: String,
}

/// Parsed `tekvra doctor` invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorOptions {
    /// Emit the check array as JSON instead of a table.
    pub json: bool,
    /// Probe provider base URLs with a bounded HEAD (network access).
    pub net: bool,
}

/// Runs the read-only `tekvra doctor` health checks (never heals; never prints tokens).
///
/// # Errors
/// Returns an error when at least one check fails (exit-code signal); table/JSON output
/// is still printed first.
pub async fn handle_doctor_command(
    paths: &ConfigPaths,
    cwd: &Path,
    overrides: &CliConfigOverrides,
    options: DoctorOptions,
) -> Result<()> {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    // 1. Config load: notices downgrade to warns, hard load errors fail.
    let resolved = match load_config(paths.clone(), cwd, overrides) {
        Ok(resolved) => {
            let notices = resolved.notices.len();
            checks.push(DoctorCheck {
                name: "config".to_string(),
                status: if notices == 0 { "ok" } else { "warn" },
                detail: if notices == 0 {
                    "loaded clean".to_string()
                } else {
                    format!("loaded with {notices} notice(s)")
                },
            });
            Some(resolved)
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "config".to_string(),
                status: "fail",
                detail: err.to_string(),
            });
            None
        }
    };

    // 2. Workspace trust.
    let canonical = canonicalize_or_clean(cwd);
    match TrustStore::load(&paths.state_dir) {
        Ok(store) => {
            if store.is_trusted(&canonical) {
                checks.push(DoctorCheck {
                    name: "trust".to_string(),
                    status: "ok",
                    detail: format!("{} is trusted", canonical.display()),
                });
            } else {
                checks.push(DoctorCheck {
                    name: "trust".to_string(),
                    status: "warn",
                    detail: format!(
                        "{} is not trusted (run `tekvra trust allow`)",
                        canonical.display()
                    ),
                });
            }
        }
        Err(err) => checks.push(DoctorCheck {
            name: "trust".to_string(),
            status: "fail",
            detail: err.to_string(),
        }),
    }

    if let Some(resolved) = &resolved {
        // 3. Per-provider: key env resolution + stored account names (never values).
        let registry = tekvra_providers::ProviderRegistry::new();
        let cred = tekvra_providers::CredentialStore::load(&paths.state_dir).ok();
        for def in registry.iter() {
            let accounts = cred
                .as_ref()
                .map(|c| c.list_for_provider(&def.id).len())
                .unwrap_or(0);
            let env_state = match def.default_api_key_env.as_deref() {
                Some(key) if std::env::var_os(key).is_some_and(|v| !v.is_empty()) => "set",
                Some(key) => key,
                None => "none",
            };
            let status = if accounts > 0 || env_state == "set" {
                "ok"
            } else {
                "warn"
            };
            checks.push(DoctorCheck {
                name: format!("provider {}", def.id),
                status,
                detail: format!("api_key_env {env_state}; {} stored account(s)", accounts),
            });
        }
        for (id, entry) in &resolved.providers.providers {
            let base = entry.base_url.as_deref().unwrap_or("(builtin)");
            checks.push(DoctorCheck {
                name: format!("provider {id} (custom)"),
                status: "ok",
                detail: format!("base_url {base}"),
            });
        }

        // 4. MCP server commands resolvable on PATH.
        for (name, server) in &resolved.settings.mcp_servers {
            let Some(cmd) = server.command.as_deref() else {
                continue;
            };
            let found = resolve_on_path(cmd).is_some();
            checks.push(DoctorCheck {
                name: format!("mcp {name}"),
                status: if found { "ok" } else { "warn" },
                detail: if found {
                    format!("`{cmd}` found")
                } else {
                    format!("`{cmd}` not found on PATH")
                },
            });
        }
    }

    // 5. state.db opens read-write and passes integrity_check.
    match tekvra_storage::StateIndex::open(&paths.state_dir) {
        Ok(index) => match index.integrity_check() {
            Ok(detail) => checks.push(DoctorCheck {
                name: "state.db".to_string(),
                status: "ok",
                detail,
            }),
            Err(err) => checks.push(DoctorCheck {
                name: "state.db".to_string(),
                status: "fail",
                detail: err.to_string(),
            }),
        },
        Err(err) => checks.push(DoctorCheck {
            name: "state.db".to_string(),
            status: "fail",
            detail: err.to_string(),
        }),
    }

    // 6. State dirs writable (scratch probe, cleaned up after).
    let scratch = paths.state_dir.join("doctor-scratch");
    match std::fs::create_dir_all(&scratch) {
        Ok(()) => {
            let writable = std::fs::write(scratch.join("probe"), b"ok").is_ok();
            let _ = std::fs::remove_dir_all(&scratch);
            checks.push(DoctorCheck {
                name: "state-dirs".to_string(),
                status: if writable { "ok" } else { "fail" },
                detail: format!("{} writable: {}", paths.state_dir.display(), writable),
            });
        }
        Err(err) => checks.push(DoctorCheck {
            name: "state-dirs".to_string(),
            status: "fail",
            detail: format!("{}: {err}", paths.state_dir.display()),
        }),
    }

    // 7. Tool presence (warn-only).
    for (name, args) in [("git", &["--version"][..]), ("cargo", &["--version"][..])] {
        let present = StdCommand::new(name)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        checks.push(DoctorCheck {
            name: name.to_string(),
            status: if present { "ok" } else { "warn" },
            detail: if present {
                "found".to_string()
            } else {
                "not found (warn-only)".to_string()
            },
        });
    }

    // 8. Optional network probe (bounded TCP connect per builtin provider base URL).
    if options.net {
        let registry = tekvra_providers::ProviderRegistry::new();
        for def in registry.iter() {
            let host_port = host_port_of(&def.base_url);
            let detail = match host_port {
                Some((host, port)) => {
                    let timeout = std::time::Duration::from_secs(5);
                    match tokio::time::timeout(
                        timeout,
                        tokio::net::TcpStream::connect((host.as_str(), port)),
                    )
                    .await
                    {
                        Ok(Ok(_)) => format!("tcp {host}:{port} reachable"),
                        Ok(Err(e)) => format!("tcp {host}:{port}: {e}"),
                        Err(_) => format!("tcp {host}:{port}: timed out after 5s"),
                    }
                }
                None => "unparseable base_url".to_string(),
            };
            checks.push(DoctorCheck {
                name: format!("net {}", def.id),
                status: if detail.contains("reachable") {
                    "ok"
                } else {
                    "warn"
                },
                detail,
            });
        }
    }

    // Report.
    let failed = checks.iter().filter(|c| c.status == "fail").count();
    if options.json {
        let arr = checks
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "status": c.status, "detail": c.detail }))
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "checks": arr,
                "healthy": failed == 0,
            }))?
        );
    } else {
        for c in &checks {
            println!(
                "[{:>4}] {:28} {}",
                c.status.to_uppercase(),
                c.name,
                c.detail
            );
        }
        println!("\n{} check(s), {failed} failure(s)", checks.len());
    }
    if failed == 0 {
        Ok(())
    } else {
        bail!("doctor found {failed} failing check(s)");
    }
}

/// Extracts `(host, port)` from an HTTP(S) base URL without a URL crate.
#[must_use]
fn host_port_of(base_url: &str) -> Option<(String, u16)> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))?;
    let host_part = rest.split(['/', '?', '#']).next()?;
    let (host, port) = if let Some((h, p)) = host_part.rsplit_once(':') {
        (h.to_string(), p.parse::<u16>().ok()?)
    } else if base_url.starts_with("https://") {
        (host_part.to_string(), 443)
    } else {
        (host_part.to_string(), 80)
    };
    if host.is_empty() {
        None
    } else {
        Some((host, port))
    }
}

/// `which`-style PATH scan for a command name (Windows-aware via PATHEXT).
#[must_use]
fn resolve_on_path(cmd: &str) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cmd.contains('/') || cmd.contains('\\') {
        vec![PathBuf::from(cmd)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .flat_map(|dir| {
                #[cfg(windows)]
                {
                    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into());
                    let mut v = vec![dir.join(cmd)];
                    for ext in exts.split(';').filter(|e| !e.is_empty()) {
                        v.push(dir.join(format!("{cmd}{}", ext.to_ascii_lowercase())));
                    }
                    v
                }
                #[cfg(not(windows))]
                {
                    vec![dir.join(cmd)]
                }
            })
            .collect()
    };
    candidates.into_iter().find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// trust
// ---------------------------------------------------------------------------

/// Folder-trust subcommand action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustAction {
    /// Print whether the workspace root is trusted.
    Status,
    /// Grant trust for the workspace root.
    Allow,
    /// Revoke trust for the workspace root.
    Revoke,
}

/// Runs `tekvra trust <status|allow|revoke>` against the canonical trust store.
///
/// # Errors
/// Returns an error when the trust store cannot be loaded or updated.
pub fn handle_trust_command(paths: &ConfigPaths, cwd: &Path, action: TrustAction) -> Result<()> {
    let mut store = TrustStore::load(&paths.state_dir).context("loading trust store")?;
    let canonical = canonicalize_or_clean(cwd);
    match action {
        TrustAction::Status => {
            if store.is_trusted(&canonical) {
                println!("trusted: {}", canonical.display());
            } else {
                println!("not trusted: {}", canonical.display());
                println!("run `tekvra trust allow` to grant folder trust");
            }
        }
        TrustAction::Allow => {
            store.grant_trust(&canonical)?;
            println!("granted trust: {}", canonical.display());
        }
        TrustAction::Revoke => {
            if store.revoke_trust(&canonical)? {
                println!("revoked trust: {}", canonical.display());
            } else {
                println!("was not trusted: {}", canonical.display());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// mcp
// ---------------------------------------------------------------------------

/// Scope of the `tekvra.toml` file mutated by `tekvra mcp add|remove`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    /// `<workspace>/tekvra.toml` (the default).
    Project,
    /// `<global_config_dir>/tekvra.toml`.
    User,
}

/// Arguments for `tekvra mcp add <NAME>`.
#[derive(Debug, Clone)]
pub struct McpAddSpec {
    /// Server registry key under `[mcp_servers.<NAME>]`.
    pub name: String,
    /// Stdio executable (mutually exclusive with `url`).
    pub command: Option<String>,
    /// Stdio argument vector (`--` trailing args).
    pub command_args: Vec<String>,
    /// HTTP/SSE endpoint URL (mutually exclusive with `command`).
    pub url: Option<String>,
    /// Environment variable holding the Bearer token for HTTP servers.
    pub bearer_env: Option<String>,
    /// Which `tekvra.toml` file to edit.
    pub scope: ConfigScope,
}

/// Runs `tekvra mcp <list|add|remove|test>` against merged configuration.
///
/// `add`/`remove` edit `tekvra.toml` via `toml_edit` so unrelated comments and
/// formatting survive the mutation.
///
/// # Errors
/// Returns an error on invalid arguments, unparsable TOML, or handshake failure in `test`.
pub async fn handle_mcp_command(
    paths: &ConfigPaths,
    resolved: &ResolvedConfig,
    action: McpAction,
) -> Result<()> {
    match action {
        McpAction::List => {
            if resolved.settings.mcp_servers.is_empty() {
                println!("no MCP servers configured");
                return Ok(());
            }
            for (name, cfg) in &resolved.settings.mcp_servers {
                let kind = if cfg.url.is_some() { "http" } else { "stdio" };
                let state = if cfg.enabled { "enabled" } else { "disabled" };
                println!("{name}\t{kind}\t{state}");
            }
            Ok(())
        }
        McpAction::Add(spec) => mcp_add(paths, resolved, spec),
        McpAction::Remove { name } => mcp_remove(paths, resolved, &name),
        McpAction::Test { name } => mcp_test(resolved, &name).await,
    }
}

/// Selected `tekvra mcp` subcommand.
#[derive(Debug, Clone)]
pub enum McpAction {
    /// List configured servers.
    List,
    /// Add or overwrite a server entry.
    Add(McpAddSpec),
    /// Remove a server entry.
    Remove {
        /// Registry key to remove.
        name: String,
    },
    /// Perform a one-shot handshake and list advertised tools.
    Test {
        /// Registry key to test.
        name: String,
    },
}

fn project_toml_path(resolved: &ResolvedConfig) -> PathBuf {
    // The config loader reads project overrides from `<git_root>/.tekvra/tekvra.toml`.
    resolved.cwd.join(".tekvra").join("tekvra.toml")
}

fn load_toml_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    if !path.exists() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    raw.parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))
}

fn mcp_add(paths: &ConfigPaths, resolved: &ResolvedConfig, spec: McpAddSpec) -> Result<()> {
    if spec.command.is_some() == spec.url.is_some() {
        bail!("pass exactly one of `--command` (stdio) or `--url` (http)");
    }
    if spec.name.is_empty()
        || !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid MCP server name `{}`", spec.name);
    }

    let path = match spec.scope {
        ConfigScope::Project => project_toml_path(resolved),
        ConfigScope::User => paths.global_config_dir.join("tekvra.toml"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut doc = load_toml_document(&path)?;

    let servers = doc
        .as_table_mut()
        .entry("mcp_servers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .context("`mcp_servers` must be a TOML table")?;
    let mut entry = toml_edit::Table::new();
    entry["enabled"] = toml_edit::value(true);
    if let Some(cmd) = &spec.command {
        entry["command"] = toml_edit::value(cmd.as_str());
        if !spec.command_args.is_empty() {
            let arr = toml_edit::Array::from_iter(spec.command_args.iter().map(String::as_str));
            entry["args"] = toml_edit::value(arr);
        }
    }
    if let Some(url) = &spec.url {
        entry["url"] = toml_edit::value(url.as_str());
    }
    if let Some(env_name) = &spec.bearer_env {
        entry["bearer_token_env_var"] = toml_edit::value(env_name.as_str());
    }
    servers.insert(&spec.name, toml_edit::Item::Table(entry));

    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote [mcp_servers.{}] to {}", spec.name, path.display());
    Ok(())
}

fn mcp_remove(paths: &ConfigPaths, resolved: &ResolvedConfig, name: &str) -> Result<()> {
    // Remove from the scope that actually defines the entry (project preferred).
    let candidates = [
        (ConfigScope::Project, project_toml_path(resolved)),
        (
            ConfigScope::User,
            paths.global_config_dir.join("tekvra.toml"),
        ),
    ];
    for (scope, path) in candidates {
        if !path.exists() {
            continue;
        }
        let mut doc = load_toml_document(&path)?;
        let removed = doc
            .as_table_mut()
            .get_mut("mcp_servers")
            .and_then(|item| item.as_table_mut())
            .and_then(|t| t.remove(name))
            .is_some();
        if removed {
            std::fs::write(&path, doc.to_string())
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "removed [mcp_servers.{name}] from {} config",
                match scope {
                    ConfigScope::Project => "project",
                    ConfigScope::User => "user",
                }
            );
            return Ok(());
        }
    }
    bail!("MCP server `{name}` is not defined in project or user config");
}

async fn mcp_test(resolved: &ResolvedConfig, name: &str) -> Result<()> {
    let Some(cfg) = resolved.settings.mcp_servers.get(name) else {
        bail!("MCP server `{name}` is not configured");
    };
    let mut single: indexmap::IndexMap<String, McpServerConfig> = indexmap::IndexMap::new();
    single.insert(name.to_string(), cfg.clone());
    let (gw, report) = ConfiguredMcpGateway::connect_all(
        &single,
        &resolved.cwd,
        &resolved.settings.agent,
        resolved.settings.experimental.mcp_sampling,
    )
    .await;
    for warn in &report.warnings {
        eprintln!("warning: {warn}");
    }
    let tools = gw.list_tools();
    if tools.is_empty() {
        println!("{name}: connected, 0 tools advertised");
    } else {
        println!("{name}: connected, {} tool(s):", tools.len());
        for tool in tools {
            println!("  {}/{}", tool.server, tool.tool);
        }
    }
    for (server, line) in mcp_catalog_lines(gw.as_ref()) {
        if server == name {
            println!("  {line}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

/// Renders a static completion script for `program` from its clap [`ClapCommand`] tree.
///
/// Pure Rust (no `clap_complete`): traverses subcommands/arguments once at runtime.
///
/// # Errors
/// Returns an error for an unsupported shell name.
pub fn generate_completion(cmd: &ClapCommand, program: &str, shell: &str) -> Result<String> {
    let tree = collect_command_tree(cmd);
    let script = match shell {
        "bash" => render_bash(&tree, program),
        "zsh" => render_zsh(&tree, program),
        "fish" => render_fish(&tree, program),
        "powershell" => render_powershell(&tree, program),
        other => bail!("unsupported shell `{other}`; expected bash|zsh|fish|powershell"),
    };
    Ok(script)
}

/// One node of the flattened clap command tree.
#[derive(Debug, Clone)]
struct CmdNode {
    /// Subcommand path from the root (`""` for the root itself).
    path: String,
    /// Short help text.
    about: String,
    /// `--long` flag names.
    longs: Vec<String>,
    /// Immediate subcommand names.
    subs: Vec<String>,
}

fn collect_command_tree(root: &ClapCommand) -> Vec<CmdNode> {
    let mut out = Vec::new();
    fn walk(cmd: &ClapCommand, prefix: &str, out: &mut Vec<CmdNode>) {
        let mut longs: Vec<String> = cmd
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .collect();
        longs.sort();
        longs.dedup();
        let subs: Vec<String> = cmd
            .get_subcommands()
            .map(|sc| sc.get_name().to_string())
            .collect();
        let path = if prefix.is_empty() {
            String::new()
        } else {
            prefix.to_string()
        };
        out.push(CmdNode {
            path,
            about: cmd.get_about().unwrap_or_default().to_string(),
            longs,
            subs,
        });
        for sc in cmd.get_subcommands() {
            let child_prefix = if prefix.is_empty() {
                sc.get_name().to_string()
            } else {
                format!("{prefix} {}", sc.get_name())
            };
            walk(sc, &child_prefix, out);
        }
    }
    walk(root, "", &mut out);
    out
}

fn find<'a>(tree: &'a [CmdNode], path: &str) -> Option<&'a CmdNode> {
    tree.iter().find(|n| n.path == path)
}

fn opts_literal(node: &CmdNode, include_subs: bool) -> String {
    let mut parts: Vec<String> = node.longs.iter().map(|l| format!("--{l}")).collect();
    if include_subs {
        parts.extend(node.subs.iter().cloned());
    }
    parts.join(" ")
}

fn render_bash(tree: &[CmdNode], program: &str) -> String {
    let root = find(tree, "").expect("root node");
    let mut out = String::new();
    out.push_str(&format!(
        "# bash completion for {program} (generated by `tekvra completion bash`)\n"
    ));
    out.push_str(&format!("_{program}() {{\n"));
    out.push_str("    local cur sub opts\n");
    out.push_str("    cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n");
    out.push_str("    if [ \"$COMP_CWORD\" -eq 1 ]; then\n");
    out.push_str(&format!(
        "        COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n",
        opts_literal(root, true)
    ));
    out.push_str("        return 0\n");
    out.push_str("    fi\n");
    out.push_str("    sub=\"${COMP_WORDS[1]}\"\n");
    out.push_str("    case \"$sub\" in\n");
    for node in tree.iter().skip(1) {
        let key = node.path.split(' ').next_back().unwrap_or_default();
        out.push_str(&format!(
            "        {key}) opts=\"{}\" ;;\n",
            opts_literal(node, true)
        ));
    }
    out.push_str("        *) opts=\"\" ;;\n");
    out.push_str("    esac\n");
    out.push_str("    COMPREPLY=( $(compgen -W \"$opts\" -- \"$cur\") )\n");
    out.push_str("    return 0\n");
    out.push_str("}\n");
    out.push_str(&format!("complete -F _{program} {program}\n"));
    out
}

fn render_zsh(tree: &[CmdNode], program: &str) -> String {
    let root = find(tree, "").expect("root node");
    let mut out = String::new();
    out.push_str(&format!("#compdef {program}\n"));
    out.push_str(&format!(
        "# zsh completion for {program} (generated by `tekvra completion zsh`)\n"
    ));
    out.push_str(&format!("_{program}() {{\n"));
    out.push_str("    local -a cmds\n");
    out.push_str("    cmds=(\n");
    for name in &root.subs {
        let about = find(tree, name).map_or(String::new(), |n| n.about.replace('\'', ""));
        out.push_str(&format!("        '{name}:{about}'\n"));
    }
    out.push_str("    )\n");
    out.push_str("    _arguments -C \\\n");
    out.push_str("        '1: :->cmds' \\\n");
    out.push_str("        '*:: :->args'\n");
    out.push_str("    case $state in\n");
    out.push_str("        cmds) _describe 'command' cmds ;;\n");
    out.push_str("        args)\n");
    out.push_str("            case $line[1] in\n");
    for node in tree.iter().skip(1) {
        let key = node.path.split(' ').next_back().unwrap_or_default();
        out.push_str(&format!("                {key}) _arguments \\\n"));
        for (i, long) in node.longs.iter().enumerate() {
            let about = escape_zsh(&node.about);
            let continuation = if i + 1 == node.longs.len() { "" } else { " \\" };
            out.push_str(&format!(
                "                    '--{long}[{about}]'{continuation}\n"
            ));
        }
        if node.longs.is_empty() {
            out.push_str("                    '(*): :_files'\n");
        }
        out.push_str("                    ;;\n");
    }
    out.push_str("            esac ;;\n");
    out.push_str("    esac\n");
    out.push_str("}\n");
    out.push_str(&format!("_{program} \"$@\"\n"));
    out
}

fn escape_zsh(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

fn render_fish(tree: &[CmdNode], program: &str) -> String {
    let root = find(tree, "").expect("root node");
    let mut out = String::new();
    out.push_str(&format!(
        "# fish completion for {program} (generated by `tekvra completion fish`)\n"
    ));
    out.push_str(&format!(
        "complete -c {program} -n '__fish_use_subcommand' -a '{}' -d '{}'\n",
        root.subs.join(" "),
        escape_fish(&root.about)
    ));
    for node in tree.iter().skip(1) {
        let key = node.path.split(' ').next_back().unwrap_or_default();
        let guard = format!("__fish_seen_subcommand_from {key}");
        for long in &node.longs {
            out.push_str(&format!(
                "complete -c {program} -n '{guard}' -l {long} -d '{}'\n",
                escape_fish(&node.about)
            ));
        }
    }
    out
}

fn escape_fish(text: &str) -> String {
    text.replace('\'', "\\'")
}

fn render_powershell(tree: &[CmdNode], program: &str) -> String {
    let root = find(tree, "").expect("root node");
    let mut out = String::new();
    out.push_str(&format!(
        "# PowerShell completion for {program} (generated by `tekvra completion powershell`)\n"
    ));
    out.push_str(&format!(
        "Register-ArgumentCompleter -CommandName '{program}' -Native -ScriptBlock {{\n"
    ));
    out.push_str("    param($wordToComplete, $commandAst, $cursorPosition)\n");
    out.push_str("    $elements = $commandAst.CommandElements\n");
    out.push_str(&format!("    $rootOpts = '{}'\n", opts_literal(root, true)));
    out.push_str("    if ($elements.Count -le 2) {\n");
    out.push_str(
        "        $rootOpts | Where-Object { $_ -like \"$wordToComplete*\" } | ForEach-Object {\n",
    );
    out.push_str("            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)\n");
    out.push_str("        }\n");
    out.push_str("        return\n");
    out.push_str("    }\n");
    out.push_str("    $sub = $elements[1].ToString()\n");
    out.push_str("    $opts = switch ($sub) {\n");
    for node in tree.iter().skip(1) {
        let key = node.path.split(' ').next_back().unwrap_or_default();
        out.push_str(&format!(
            "        '{key}' {{ @('{}') }}\n",
            opts_literal(node, true)
        ));
    }
    out.push_str("        default { @() }\n");
    out.push_str("    }\n");
    out.push_str("    $opts | Where-Object { $_ -like \"$wordToComplete*\" } | ForEach-Object {\n");
    out.push_str("        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// worktree
// ---------------------------------------------------------------------------

/// Result of `ensure_worktree`.
#[derive(Debug, Clone)]
pub struct WorktreeOutcome {
    /// Absolute path of the worktree checkout.
    pub path: PathBuf,
    /// Branch checked out in the worktree (`tekvra/<name>`).
    pub branch: String,
    /// Whether this call created the worktree (vs reusing an existing one).
    pub created: bool,
}

/// Validates a managed worktree name (`[A-Za-z0-9._-]+`, no leading dot).
///
/// # Errors
/// Returns an error for invalid names.
pub fn ensure_worktree_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        bail!("invalid worktree name `{name}`; expected [A-Za-z0-9._-]+");
    }
    Ok(())
}

/// Creates (or reuses) the managed git worktree `<state_dir>/worktrees/<repo-slug>/<name>`
/// on branch `tekvra/<name>`.
///
/// # Errors
/// Returns an error for invalid names or git failures.
pub fn ensure_worktree(repo_root: &Path, state_dir: &Path, name: &str) -> Result<WorktreeOutcome> {
    ensure_worktree_name(name)?;
    let repo_slug = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| {
            let mut slug = s.to_ascii_lowercase();
            slug.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            slug
        })
        .filter(|s| !s.is_empty())
        .context("resolving repository slug from working directory")?;
    let branch = format!("tekvra/{name}");
    let dir = state_dir.join("worktrees").join(&repo_slug).join(name);

    if dir.join(".git").exists() {
        return Ok(WorktreeOutcome {
            path: dir,
            branch,
            created: false,
        });
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let branch_exists = StdCommand::new("git")
        .args(["-C", &repo_root.display().to_string()])
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let mut add = StdCommand::new("git");
    add.args(["-C", &repo_root.display().to_string()]);
    add.arg("worktree");
    add.arg("add");
    if !branch_exists {
        add.arg("-b");
        add.arg(&branch);
    }
    add.arg(dir.display().to_string());
    let output = add.output().context("running `git worktree add`")?;
    if !output.status.success() {
        bail!(
            "`git worktree add` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(WorktreeOutcome {
        path: dir,
        branch,
        created: true,
    })
}

// ---------------------------------------------------------------------------
// image attachments
// ---------------------------------------------------------------------------

/// Loads and validates `-i/--image` attachments into protocol [`InputImage`] values.
///
/// Accepts `png|jpg|jpeg|webp|gif` up to [`MAX_IMAGE_BYTES`]; content is inlined as base64.
///
/// # Errors
/// Returns an error for unsupported extensions, oversized files, or unreadable paths.
pub fn load_image_attachments(paths: &[PathBuf]) -> Result<Vec<InputImage>> {
    let mut images = Vec::new();
    for path in paths {
        let meta =
            std::fs::metadata(path).with_context(|| format!("stating image {}", path.display()))?;
        if meta.len() > MAX_IMAGE_BYTES {
            bail!(
                "image {} is {} bytes; the limit is {MAX_IMAGE_BYTES} bytes",
                path.display(),
                meta.len()
            );
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let media_type = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            other => bail!(
                "unsupported image extension `.{other}` on {}",
                path.display()
            ),
        };
        let data =
            std::fs::read(path).with_context(|| format!("reading image {}", path.display()))?;
        images.push(InputImage {
            media_type: media_type.to_string(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            source_path: Some(path.display().to_string()),
        });
    }
    Ok(images)
}

/// Injects the `--skill <NAME>` invocation prefix into `prompt`.
///
/// The session runner expands `/skill <NAME> ...` against discovered SKILL.md packages.
///
/// # Errors
/// Returns an error when `--skill` is passed without a prompt.
pub fn apply_skill_flag(skill: Option<&str>, prompt: Option<String>) -> Result<Option<String>> {
    match (skill, prompt) {
        (Some(name), Some(p)) => Ok(Some(format!("/skill {name} {p}"))),
        (Some(name), None) => {
            bail!("`--skill {name}` requires a prompt (positional or `--print`)")
        }
        (None, p) => Ok(p),
    }
}
