//! Unified `tekvra` CLI binary entrypoint (`tekvra`, `exec`, `resume`, `fork`, `review`, `app-server`, `acp`, `auth`).

mod exec_runner;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use exec_runner::{ExecRunOptions, SchemaValidationFailed, run_headless_exec};
use ignore::WalkBuilder;
use tekvra_acp::{AcpServerDeps, serve_acp_stdio};
use tekvra_app_server::{
    AppServerState, AppServerStateInit, parse_listen_endpoint, serve_endpoint,
};
use tekvra_config::{CliConfigOverrides, ConfigPaths, ResolvedConfig, load_config};
use tekvra_core::{
    CompactionConfig, PluginToolExecutor, ReviewTarget, SessionBootstrap, SessionHandle,
    run_code_review, spawn_session,
};
use tekvra_lua::{LuaHostOptions, PluginHost};
use tekvra_protocol::PermissionMode;
use tekvra_providers::{
    AccountCandidate, AccountPin, AccountPool, CredentialStore, HttpProviderDriver,
    LoopbackOAuthListener, ModelRouter, OAuthProviderEndpoints, ProviderDriver, ProviderRegistry,
    SecretToken, StoredOAuthAccount, build_oauth_http_client, exchange_oauth_token,
    generate_pkce_pair, poll_device_code_token, refresh_oauth_account, request_device_code,
};
use tekvra_storage::{RolloutLog, RolloutRecord, StateIndex, WorkspaceSnapshotStore, rollout_path};
use tekvra_tools::{ConfiguredMcpGateway, McpGateway, is_sensitive_credential_path};
use tekvra_tui::TuiRuntimeHooks;
use uuid::Uuid;

mod ops;

use ops::{
    ConfigScope, McpAction, McpAddSpec, TrustAction, UsageOptions, apply_skill_flag,
    ensure_worktree, ensure_worktree_name, generate_completion, handle_doctor_command,
    handle_mcp_command, handle_memory_command, handle_trust_command, handle_usage_command,
    load_image_attachments, parse_usage_group_by,
};

#[derive(Debug, Parser)]
#[command(
    name = "tekvra",
    version,
    about = "Fast Apache-2.0 native Rust AI coding agent (TUI, headless exec, Codex app-server v2, and ACP)"
)]
struct Cli {
    /// Override active `provider/model` (e.g., `openai/gpt-5.4-codex`, `anthropic/claude-sonnet-4-5`).
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,

    /// Configuration profile from `tekvra.toml`.
    #[arg(long, global = true)]
    profile: Option<String>,

    /// Dotted TOML configuration override (`-c key=value`).
    #[arg(short = 'c', long = "config", global = true)]
    config_overrides: Vec<String>,

    /// Enable experimental feature flag (`--enable lua_plugins`).
    #[arg(long = "enable", global = true)]
    enabled_features: Vec<String>,

    /// Suppress all external Luau plugin initialization for this invocation.
    #[arg(long, global = true)]
    no_plugins: bool,

    /// Disable Luau native JIT codegen and enable full debug line info.
    #[arg(long, global = true)]
    no_jit: bool,

    /// Pin credential selection to `PROVIDER:NAME`, disabling multi-account rotation for that provider.
    #[arg(long, global = true)]
    account: Option<String>,

    /// Initial permission mode (`plan`, `build`, `auto`, `yolo`).
    #[arg(long, global = true)]
    mode: Option<String>,

    /// Working directory override.
    #[arg(short = 'C', long = "cd", global = true)]
    cwd: Option<PathBuf>,

    /// Run a single non-interactive prompt and print the response (`--print <PROMPT>`).
    #[arg(short = 'p', long = "print")]
    print_prompt: Option<String>,

    /// Attach an image (repeatable; png/jpg/jpeg/webp/gif, max 10 MiB each) to the first turn.
    #[arg(short = 'i', long = "image", global = true)]
    image_paths: Vec<PathBuf>,

    /// Inject the discovered SKILL.md `<NAME>` into the prompt (`--skill review-api`).
    #[arg(long, global = true)]
    skill: Option<String>,

    /// Create/reuse the managed git worktree `<NAME>` (`tekvra/<NAME>`) for this run.
    #[arg(long, global = true)]
    worktree: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Execute a prompt headlessly with optional JSONL events, `stream-json`, and `--output-schema` validation.
    Exec(ExecArgs),
    /// Resume an existing session (`--last` or `<SESSION_ID>`) in TUI or headlessly with `--prompt`.
    Resume(ResumeArgs),
    /// Fork a new session branch from `<SESSION_ID>` in TUI or headlessly with `--prompt`.
    Fork(ForkArgs),
    /// Run a dedicated read-only code review over Git diffs.
    Review(ReviewArgs),
    /// Run the Codex-compatible v2 JSON-RPC 2.0 `app-server` (`stdio://`, `unix://`, `ws://`).
    AppServer(AppServerArgs),
    /// Run the Agent Client Protocol (`acp`) stdio server for Zed/Neovim/JetBrains.
    Acp,
    /// Manage OAuth and API key credentials (`login`, `status`, `logout`).
    Auth(AuthArgs),
    /// Manage configured MCP servers (`list`, `add`, `remove`, `test`).
    Mcp(McpArgs),
    /// Inspect or toggle workspace folder trust (`status`, `allow`, `revoke`).
    Trust(TrustArgs),
    /// Emit a shell completion script (`bash`, `zsh`, `fish`, `powershell`).
    Completion(CompletionArgs),
    /// Create or reuse a managed git worktree without starting a session.
    Worktree(WorktreeArgs),
    /// Report aggregated token/cost usage from the local ledger.
    Usage(UsageArgs),
    /// Print the cross-session project memory (optionally append an entry).
    Memory(MemoryArgs),
    /// Run read-only health checks (config, trust, providers, MCP, state).
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
struct ExecArgs {
    /// Prompt string (reads stdin when omitted).
    prompt: Option<String>,
    /// Stream newline-delimited JSONL `SessionEvent` frames.
    #[arg(long)]
    json: bool,
    /// Validate final assistant JSON output against `<PATH>` JSON Schema.
    #[arg(long)]
    output_schema: Option<PathBuf>,
    /// Write final assistant message text/JSON to `<PATH>`.
    #[arg(short = 'o', long = "output-last-message")]
    output_file: Option<PathBuf>,
    /// Input format (`text` or `stream-json`).
    #[arg(long, default_value = "text")]
    input_format: String,
    /// Output format (`text`, `json`, or `stream-json`).
    #[arg(long, default_value = "text")]
    output_format: String,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    /// Explicit session ID to resume (omit when `--last` is passed).
    session_id: Option<String>,
    /// Resume the most recently updated non-archived session.
    #[arg(long)]
    last: bool,
    /// Optional follow-up prompt to execute headlessly on the resumed session.
    #[arg(long)]
    prompt: Option<String>,
    /// Stream newline-delimited JSONL `SessionEvent` frames when `--prompt` is given.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ForkArgs {
    /// Parent session ID to fork from.
    session_id: String,
    /// Optional follow-up prompt to execute headlessly on the forked session.
    #[arg(long)]
    prompt: Option<String>,
    /// Stream newline-delimited JSONL `SessionEvent` frames when `--prompt` is given.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReviewArgs {
    /// Review staged, unstaged, and untracked working tree changes.
    #[arg(long)]
    uncommitted: bool,
    /// Review changes against `<BRANCH>`.
    #[arg(long)]
    base: Option<String>,
    /// Review a specific commit `<SHA>`.
    #[arg(long)]
    commit: Option<String>,
    /// Emit structured JSON findings instead of Markdown text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AppServerArgs {
    /// Transport endpoint (`stdio://`, `unix:///path/to.sock`, or `ws://127.0.0.1:4500`).
    #[arg(long, default_value = "stdio://")]
    listen: String,
    /// Optional Bearer capability token required on `ws://` connections.
    #[arg(long, env = "TEKVRA_APP_SERVER_TOKEN")]
    auth_token: Option<String>,
}

#[derive(Debug, Args)]
struct AuthArgs {
    #[command(subcommand)]
    action: AuthAction,
}

#[derive(Debug, Subcommand)]
enum AuthAction {
    /// Store an API key or complete interactive PKCE / device-code OAuth login.
    Login {
        /// Provider slug (default: `openai`).
        #[arg(long, default_value = "openai")]
        provider: String,
        /// Account alias (default: `default`).
        #[arg(long, default_value = "default")]
        account_name: String,
        /// Direct API key or OAuth bearer token to store (`0600` `credentials.json`).
        #[arg(long)]
        api_key: Option<String>,
        /// Use headless RFC 8628 device-code OAuth flow instead of browser loopback PKCE.
        #[arg(long)]
        device_code: bool,
        /// Do not attempt to launch a desktop browser automatically during loopback login.
        #[arg(long)]
        no_browser: bool,
        /// Maximum poll iterations when waiting for `--device-code` completion.
        #[arg(long, default_value_t = 120)]
        max_polls: u32,
    },
    /// Show configured accounts and environment credentials.
    Status,
    /// Remove a stored credential from `credentials.json`.
    Logout {
        /// Provider slug.
        #[arg(long, default_value = "openai")]
        provider: String,
        /// Account alias.
        #[arg(long, default_value = "default")]
        account_name: String,
    },
}

#[derive(Debug, Args)]
struct McpArgs {
    #[command(subcommand)]
    action: McpSubAction,
}

#[derive(Debug, Subcommand)]
enum McpSubAction {
    /// List configured MCP servers (`name  kind  state`).
    List,
    /// Add or overwrite an MCP server entry in `tekvra.toml` (comment-preserving).
    Add {
        /// Registry key under `[mcp_servers.<NAME>]`.
        name: String,
        /// Stdio server executable (mutually exclusive with `--url`).
        #[arg(long)]
        command: Option<String>,
        /// Stdio server arguments (`tekvra mcp add fs --command npx -- -y @modelcontextprotocol/server-fs`).
        #[arg(last = true)]
        command_args: Vec<String>,
        /// HTTP/SSE endpoint URL (mutually exclusive with `--command`).
        #[arg(long)]
        url: Option<String>,
        /// Environment variable holding the Bearer token for HTTP servers.
        #[arg(long = "bearer-env")]
        bearer_env: Option<String>,
        /// Edit the user-global config instead of `<workspace>/tekvra.toml`.
        #[arg(long, default_value = "project")]
        scope: String,
    },
    /// Remove an MCP server entry from `tekvra.toml`.
    Remove {
        /// Registry key to remove.
        name: String,
    },
    /// Run a one-shot handshake against `<NAME>` and list its advertised tools.
    Test {
        /// Registry key to test.
        name: String,
    },
}

#[derive(Debug, Args)]
struct TrustArgs {
    #[command(subcommand)]
    action: TrustSubAction,
}

#[derive(Debug, Subcommand)]
enum TrustSubAction {
    /// Print whether the workspace root is trusted.
    Status,
    /// Grant folder trust for the workspace root.
    Allow,
    /// Revoke folder trust for the workspace root.
    Revoke,
}

#[derive(Debug, Args)]
struct CompletionArgs {
    /// Target shell (`bash`, `zsh`, `fish`, `powershell`).
    shell: String,
}

#[derive(Debug, Args)]
struct WorktreeArgs {
    #[command(subcommand)]
    action: WorktreeSubAction,
}

#[derive(Debug, Args)]
struct UsageArgs {
    /// Limit to the trailing N days (omit for the whole ledger).
    #[arg(long)]
    days: Option<u32>,
    /// Group rows by `model` (default), `day`, or `session`.
    #[arg(long)]
    by: Option<String>,
    /// Print machine-readable JSON instead of a table.
    #[arg(long, conflicts_with = "csv")]
    json: bool,
    /// Export full per-turn rows as CSV to `<PATH>`.
    #[arg(long, value_name = "PATH")]
    csv: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct MemoryArgs {
    /// Append one dated entry instead of printing the file.
    #[arg(long, value_name = "TEXT")]
    append: Option<String>,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Print the check array as JSON.
    #[arg(long)]
    json: bool,
    /// Probe builtin provider endpoints with bounded TCP connects.
    #[arg(long)]
    net: bool,
}

#[derive(Debug, Subcommand)]
enum WorktreeSubAction {
    /// List managed worktrees for the current repository.
    List,
    /// Create (or reuse) the managed worktree `tekvra/<NAME>`.
    Create {
        /// Worktree name `[A-Za-z0-9._-]+`; checkout lands under `<state_dir>/worktrees/<repo-slug>/<NAME>`.
        name: String,
    },
    /// Remove the managed worktree `<NAME>` (its branch `tekvra/<NAME>` is kept).
    Remove {
        /// Worktree name to remove.
        name: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match try_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if err.downcast_ref::<SchemaValidationFailed>().is_some() {
                eprintln!("error: {err}");
                ExitCode::from(2)
            } else {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = match &cli.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("failed resolving current working directory")?,
    };

    let mut dotted = Vec::new();
    for raw_kv in &cli.config_overrides {
        let Some((k, v)) = raw_kv.split_once('=') else {
            bail!("invalid `-c` override `{raw_kv}`; expected `key=value`");
        };
        dotted.push((k.trim().to_string(), v.trim().to_string()));
    }
    if let Some(m) = &cli.model {
        dotted.push(("model".to_string(), m.clone()));
    }

    let overrides = CliConfigOverrides {
        profile: cli.profile.clone(),
        dotted_overrides: dotted,
        enabled_features: cli.enabled_features.clone(),
        no_plugins: cli.no_plugins,
    };

    // `tekvra doctor` runs read-only checks and exits non-zero on failures. It must run
    // BEFORE the strict config load: a broken global config is exactly what it diagnoses.
    if let Some(Commands::Doctor(doctor_args)) = &cli.command {
        return handle_doctor_command(
            &ConfigPaths::from_env(),
            &cwd,
            &overrides,
            ops::DoctorOptions {
                json: doctor_args.json,
                net: doctor_args.net,
            },
        )
        .await;
    }

    let mut resolved = load_config(ConfigPaths::from_env(), &cwd, &overrides)?;
    if let Some(mode_str) = &cli.mode {
        resolved.settings.agent.default_mode = parse_mode(mode_str)?;
    }

    // Managed worktree: run inside `<state_dir>/worktrees/<repo-slug>/<name>` on `tekvra/<name>`.
    if let Some(name) = &cli.worktree {
        let outcome = ensure_worktree(&resolved.cwd, &resolved.paths.state_dir, name)?;
        // Worktrees are Tekvra-managed checkouts: trust them like the primary workspace.
        let mut store = tekvra_config::TrustStore::load(&resolved.paths.state_dir)?;
        store.grant_trust(&outcome.path)?;
        eprintln!(
            "worktree: {} on {} ({})",
            outcome.path.display(),
            outcome.branch,
            if outcome.created { "created" } else { "reused" }
        );
        resolved.cwd = outcome.path;
    }

    if let Some(Commands::Auth(auth_args)) = cli.command {
        return handle_auth_command(&resolved, auth_args).await;
    }

    // `tekvra usage` is a stateless ledger read: short-circuit before plugin/session init.
    if let Some(Commands::Usage(usage_args)) = &cli.command {
        let by = match usage_args.by.as_deref() {
            Some(by) => parse_usage_group_by(by)?,
            None => tekvra_storage::UsageGroupBy::Model,
        };
        return handle_usage_command(
            &ConfigPaths::from_env(),
            &UsageOptions {
                days: usage_args.days,
                by,
                json: usage_args.json,
                csv: usage_args.csv.clone(),
            },
        );
    }

    // `tekvra memory` is a stateless project-memory read/append: same short-circuit rules.
    if let Some(Commands::Memory(memory_args)) = &cli.command {
        return handle_memory_command(
            &ConfigPaths::from_env(),
            &cwd,
            memory_args.append.as_deref(),
        );
    }

    // Local trust/mcp/completion/worktree operations short-circuit before plugin/MCP/session init.
    if let Some(Commands::Trust(trust_args)) = &cli.command {
        let action = match trust_args.action {
            TrustSubAction::Status => TrustAction::Status,
            TrustSubAction::Allow => TrustAction::Allow,
            TrustSubAction::Revoke => TrustAction::Revoke,
        };
        return handle_trust_command(&ConfigPaths::from_env(), &resolved.cwd, action);
    }
    if let Some(Commands::Mcp(mcp_args)) = &cli.command {
        let action = match &mcp_args.action {
            McpSubAction::List => McpAction::List,
            McpSubAction::Add {
                name,
                command,
                command_args,
                url,
                bearer_env,
                scope,
            } => {
                let scope = match scope.as_str() {
                    "project" => ConfigScope::Project,
                    "user" => ConfigScope::User,
                    other => anyhow::bail!("invalid --scope `{other}`; expected project|user"),
                };
                McpAction::Add(McpAddSpec {
                    name: name.clone(),
                    command: command.clone(),
                    command_args: command_args.clone(),
                    url: url.clone(),
                    bearer_env: bearer_env.clone(),
                    scope,
                })
            }
            McpSubAction::Remove { name } => McpAction::Remove { name: name.clone() },
            McpSubAction::Test { name } => McpAction::Test { name: name.clone() },
        };
        return handle_mcp_command(&ConfigPaths::from_env(), &resolved, action).await;
    }
    if let Some(Commands::Completion(completion_args)) = &cli.command {
        let script =
            generate_completion(&Cli::command(), "tekvra", completion_args.shell.as_str())?;
        print!("{script}");
        return Ok(());
    }
    if let Some(Commands::Worktree(worktree_args)) = &cli.command {
        match &worktree_args.action {
            WorktreeSubAction::Create { name } => {
                let outcome = ensure_worktree(&resolved.cwd, &resolved.paths.state_dir, name)?;
                println!(
                    "{} ({}) {}",
                    outcome.path.display(),
                    outcome.branch,
                    if outcome.created { "created" } else { "reused" }
                );
            }
            WorktreeSubAction::List => {
                let slug = resolved
                    .cwd
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| {
                        let mut slug = s.to_ascii_lowercase();
                        slug.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                        slug
                    })
                    .unwrap_or_default();
                let root = resolved.paths.state_dir.join("worktrees").join(&slug);
                let mut entries: Vec<PathBuf> = if root.is_dir() {
                    std::fs::read_dir(&root)
                        .expect("read worktrees dir")
                        .filter_map(std::result::Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.join(".git").exists())
                        .collect()
                } else {
                    Vec::new()
                };
                let mut found = false;
                entries.sort();
                for path in entries {
                    found = true;
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    println!("{}\t{}\ttekvra/{name}", name, path.display());
                }
                if !found {
                    println!("no managed worktrees for `{slug}`");
                }
            }
            WorktreeSubAction::Remove { name } => {
                let slug = resolved
                    .cwd
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| {
                        let mut slug = s.to_ascii_lowercase();
                        slug.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                        slug
                    })
                    .unwrap_or_default();
                // Reject invalid names without any create side effect.
                ensure_worktree_name(name)?;
                let path = resolved
                    .paths
                    .state_dir
                    .join("worktrees")
                    .join(&slug)
                    .join(name);
                if !path.join(".git").exists() {
                    bail!("worktree `{name}` does not exist");
                }
                let output = std::process::Command::new("git")
                    .args(["-C", &resolved.cwd.display().to_string()])
                    .args(["worktree", "remove", &path.display().to_string()])
                    .output()
                    .context("running `git worktree remove`")?;
                if !output.status.success() {
                    bail!(
                        "`git worktree remove` failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                println!("removed worktree {} (tekvra/{name})", path.display());
            }
        }
        return Ok(());
    }

    // 1. Initialize opt-in Luau plugin host (`vm_initialized == false` by default and under `--no-plugins`!)
    let (plugin_host, plugin_report) = PluginHost::initialize(
        resolved.clone(),
        LuaHostOptions {
            no_plugins: cli.no_plugins,
            no_jit: cli.no_jit,
        },
    )?;
    for notice in &plugin_report.notices {
        eprintln!("notice: {notice}");
    }

    // 2. Connect configured MCP servers (stdio + HTTP JSON-RPC) from trusted `tekvra.toml`
    let mcp_gateway_opt: Option<Arc<dyn McpGateway>> =
        if resolved.settings.mcp_servers.values().any(|s| s.enabled) {
            let (gw, mcp_report) = ConfiguredMcpGateway::connect_all(
                &resolved.settings.mcp_servers,
                &resolved.cwd,
                &resolved.settings.agent,
                resolved.settings.experimental.mcp_sampling,
            )
            .await;
            for warn in &mcp_report.warnings {
                eprintln!("notice: {warn}");
            }
            (!gw.is_empty()).then_some(gw as Arc<dyn McpGateway>)
        } else {
            None
        };

    let mut registry = ProviderRegistry::new();
    registry.apply_custom_configs(&resolved.providers.providers);
    let router = ModelRouter::from_config(&resolved, registry.clone());
    let available_models: Vec<(String, String)> = registry
        .iter()
        .flat_map(|def| {
            def.featured_models
                .iter()
                .map(move |model| (format!("{}/{}", def.id, model), def.display_name.clone()))
        })
        .collect();

    let account_pool = build_account_pool(&resolved, &registry, cli.account.as_deref()).await?;
    let driver: Arc<dyn ProviderDriver> = Arc::new(HttpProviderDriver::new(account_pool));
    let state_index = Arc::new(Mutex::new(StateIndex::open(&resolved.paths.state_dir)?));
    let snapshots = WorkspaceSnapshotStore::new(&resolved.paths.state_dir);

    let plugin_exec_opt: Option<Arc<dyn PluginToolExecutor>> = plugin_host
        .is_vm_initialized()
        .then(|| Arc::new(plugin_host.clone()) as Arc<dyn PluginToolExecutor>);
    let extra_tools = plugin_host.registered_tools();

    if let Some(print_prompt) = cli.print_prompt {
        let mode = resolved.settings.agent.default_mode;
        let images = load_image_attachments(&cli.image_paths)?;
        let prompt = apply_skill_flag(cli.skill.as_deref(), Some(print_prompt))?;
        let handle = spawn_cli_session(
            None,
            None,
            &resolved,
            &router,
            &driver,
            &state_index,
            mcp_gateway_opt,
            plugin_exec_opt,
            extra_tools,
        )?;
        return run_headless_exec(
            handle,
            ExecRunOptions {
                prompt,
                json_events: false,
                output_schema: None,
                output_file: None,
                input_format: "text".to_string(),
                output_format: "text".to_string(),
                mode,
                images,
            },
        )
        .await;
    }

    match cli.command {
        Some(Commands::Exec(args)) => {
            let mode = resolved.settings.agent.default_mode;
            let images = load_image_attachments(&cli.image_paths)?;
            let prompt = apply_skill_flag(cli.skill.as_deref(), args.prompt)?;
            let handle = spawn_cli_session(
                None,
                None,
                &resolved,
                &router,
                &driver,
                &state_index,
                mcp_gateway_opt,
                plugin_exec_opt,
                extra_tools,
            )?;
            run_headless_exec(
                handle,
                ExecRunOptions {
                    prompt,
                    json_events: args.json,
                    output_schema: args.output_schema,
                    output_file: args.output_file,
                    input_format: args.input_format,
                    output_format: args.output_format,
                    mode,
                    images,
                },
            )
            .await
        }
        Some(Commands::Resume(args)) => {
            let target_id =
                if let Some(explicit) = args.session_id {
                    explicit
                } else if args.last {
                    let latest = state_index
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .list_sessions(false, 1, 0)?;
                    latest.into_iter().next().map(|s| s.id).ok_or_else(|| {
                        anyhow::anyhow!("no saved sessions found to `--last` resume")
                    })?
                } else {
                    bail!("pass a `<SESSION_ID>` or `--last` to `tekvra resume`");
                };

            let (handle, records) = open_resumed_or_forked_session(
                &target_id,
                false,
                &resolved,
                &router,
                &driver,
                &state_index,
                mcp_gateway_opt.clone(),
                plugin_exec_opt.clone(),
                extra_tools.clone(),
            )?;

            if let Some(prompt) = args.prompt {
                let images = load_image_attachments(&cli.image_paths)?;
                let prompt =
                    apply_skill_flag(cli.skill.as_deref(), Some(prompt))?.expect("prompt present");
                return run_headless_exec(
                    handle,
                    ExecRunOptions {
                        prompt: Some(prompt),
                        json_events: args.json,
                        output_schema: None,
                        output_file: None,
                        input_format: "text".to_string(),
                        output_format: "text".to_string(),
                        mode: resolved.settings.agent.default_mode,
                        images,
                    },
                )
                .await;
            }

            launch_interactive_tui(
                handle,
                records,
                resolved,
                router,
                driver,
                state_index,
                snapshots,
                plugin_host,
                mcp_gateway_opt,
                plugin_exec_opt,
                extra_tools,
                available_models,
            )
            .await
        }
        Some(Commands::Fork(args)) => {
            let (handle, records) = open_resumed_or_forked_session(
                &args.session_id,
                true,
                &resolved,
                &router,
                &driver,
                &state_index,
                mcp_gateway_opt.clone(),
                plugin_exec_opt.clone(),
                extra_tools.clone(),
            )?;

            if let Some(prompt) = args.prompt {
                println!("Forked session: {}", handle.session_id());
                let images = load_image_attachments(&cli.image_paths)?;
                let prompt =
                    apply_skill_flag(cli.skill.as_deref(), Some(prompt))?.expect("prompt present");
                return run_headless_exec(
                    handle,
                    ExecRunOptions {
                        prompt: Some(prompt),
                        json_events: args.json,
                        output_schema: None,
                        output_file: None,
                        input_format: "text".to_string(),
                        output_format: "text".to_string(),
                        mode: resolved.settings.agent.default_mode,
                        images,
                    },
                )
                .await;
            }

            launch_interactive_tui(
                handle,
                records,
                resolved,
                router,
                driver,
                state_index,
                snapshots,
                plugin_host,
                mcp_gateway_opt,
                plugin_exec_opt,
                extra_tools,
                available_models,
            )
            .await
        }
        Some(Commands::Review(args)) => {
            let target = if let Some(branch) = args.base {
                ReviewTarget::BaseBranch { branch }
            } else if let Some(sha) = args.commit {
                ReviewTarget::Commit { sha }
            } else {
                let _ = args.uncommitted;
                ReviewTarget::UncommittedChanges
            };
            let outcome = run_code_review(
                "cli_review",
                &resolved.cwd,
                &target,
                &router,
                driver.as_ref(),
                &resolved.settings.agent,
            )
            .await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "summary": outcome.summary,
                        "findings": outcome.findings,
                    }))?
                );
            } else {
                println!("{}", outcome.summary);
                for f in &outcome.findings {
                    println!(
                        "- [{}] {}:{}-{} — {}: {}",
                        f.severity,
                        f.path.display(),
                        f.start_line,
                        f.end_line,
                        f.title,
                        f.body
                    );
                }
            }
            Ok(())
        }
        Some(Commands::AppServer(args)) => {
            let endpoint = parse_listen_endpoint(&args.listen)?;
            let app_state = AppServerState::new(AppServerStateInit {
                config: resolved,
                provider_registry: registry,
                router,
                driver,
                state_index,
                snapshots,
                mcp_gateway: mcp_gateway_opt,
                plugin_executor: plugin_exec_opt,
                extra_plugin_tools: extra_tools,
            });
            serve_endpoint(endpoint, args.auth_token, app_state).await?;
            Ok(())
        }
        Some(Commands::Acp) => {
            serve_acp_stdio(AcpServerDeps {
                config: resolved,
                router,
                driver,
                state_index,
                mcp_gateway: mcp_gateway_opt,
                plugin_executor: plugin_exec_opt,
                extra_plugin_tools: extra_tools,
            })
            .await?;
            Ok(())
        }
        Some(Commands::Auth(_)) => unreachable!("handled above"),
        Some(Commands::Mcp(_))
        | Some(Commands::Trust(_))
        | Some(Commands::Completion(_))
        | Some(Commands::Worktree(_))
        | Some(Commands::Usage(_))
        | Some(Commands::Memory(_))
        | Some(Commands::Doctor(_)) => unreachable!("handled above"),
        None => {
            let handle = spawn_cli_session(
                None,
                None,
                &resolved,
                &router,
                &driver,
                &state_index,
                mcp_gateway_opt.clone(),
                plugin_exec_opt.clone(),
                extra_tools.clone(),
            )?;
            launch_interactive_tui(
                handle,
                Vec::new(),
                resolved,
                router,
                driver,
                state_index,
                snapshots,
                plugin_host,
                mcp_gateway_opt,
                plugin_exec_opt,
                extra_tools,
                available_models,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn launch_interactive_tui(
    handle: SessionHandle,
    initial_records: Vec<RolloutRecord>,
    resolved: ResolvedConfig,
    router: ModelRouter,
    driver: Arc<dyn ProviderDriver>,
    state_index: Arc<Mutex<StateIndex>>,
    snapshots: WorkspaceSnapshotStore,
    plugin_host: PluginHost,
    mcp_gateway_opt: Option<Arc<dyn McpGateway>>,
    plugin_exec_opt: Option<Arc<dyn PluginToolExecutor>>,
    extra_tools: Vec<tekvra_providers::ToolDefinition>,
    available_models: Vec<(String, String)>,
) -> Result<()> {
    let workspace_files = scan_workspace_mention_files(&resolved.cwd, 4_000);
    let extra_slash_commands: Vec<(String, String)> = plugin_host
        .registered_commands()
        .into_iter()
        .map(|c| (c.name, c.description))
        .collect();
    let discovered_skills: Vec<(String, String)> = tekvra_config::discover_skills(
        &resolved.paths.global_config_dir,
        resolved.paths.home_dir.as_deref(),
        &resolved.cwd,
    )
    .into_iter()
    .map(|s| (s.name, s.description))
    .collect();
    let trust_state_dir = resolved.paths.state_dir.clone();
    let trust_cwd = resolved.cwd.clone();

    let host_for_reload = plugin_host.clone();
    let host_for_cmd = plugin_host;
    let mcp_for_tui = mcp_gateway_opt.clone();
    let review_cwd = resolved.cwd.clone();
    let review_router = router.clone();
    let review_driver = Arc::clone(&driver);
    let review_agent_cfg = resolved.settings.agent.clone();

    let sw_resolved = resolved;
    let sw_router = router;
    let sw_driver = driver;
    let sw_index = Arc::clone(&state_index);
    let sw_mcp = mcp_gateway_opt;
    let sw_plug = plugin_exec_opt;
    let sw_tools = extra_tools;
    // `/memory` viewer state — captured before `sw_resolved` moves into a hook closure.
    let mem_state = sw_resolved.paths.state_dir.clone();
    let mem_cwd = sw_resolved.cwd.clone();

    let hooks = TuiRuntimeHooks {
        initial_records,
        workspace_files,
        extra_slash_commands,
        discovered_skills,
        available_models,
        on_trust_command: Some(Box::new(move |action| {
            let mut store =
                tekvra_config::TrustStore::load(&trust_state_dir).map_err(|e| e.to_string())?;
            let canonical = tekvra_config::canonicalize_or_clean(&trust_cwd);
            match action {
                tekvra_tui::TrustUiCommand::Status => {
                    if store.is_trusted(&canonical) {
                        Ok(format!("trusted: {}", canonical.display()))
                    } else {
                        Ok(format!(
                            "not trusted: {} (`/trust allow` to grant)",
                            canonical.display()
                        ))
                    }
                }
                tekvra_tui::TrustUiCommand::Allow => {
                    store.grant_trust(&canonical).map_err(|e| e.to_string())?;
                    Ok(format!("granted trust: {}", canonical.display()))
                }
                tekvra_tui::TrustUiCommand::Revoke => {
                    let removed = store.revoke_trust(&canonical).map_err(|e| e.to_string())?;
                    Ok(if removed {
                        format!("revoked trust: {}", canonical.display())
                    } else {
                        format!("was not trusted: {}", canonical.display())
                    })
                }
            }
        })),
        on_reload_plugins: Some(Box::new(move || {
            let rep = host_for_reload.reload().map_err(|e| e.to_string())?;
            Ok(format!(
                "Reloaded {} plugin(s) (vm_active={})",
                rep.loaded_plugins.len(),
                rep.vm_initialized
            ))
        })),
        on_plugin_command: Some(Box::new(move |name, args| {
            if !host_for_cmd.has_command(name) {
                return Ok(None);
            }
            host_for_cmd
                .invoke_command(name, args)
                .map(Some)
                .map_err(|e| e.to_string())
        })),
        on_mcp_status: Some(Box::new(move || {
            let Some(gw) = &mcp_for_tui else {
                return "No MCP servers connected.".to_string();
            };
            let tools = gw.list_tools();
            if tools.is_empty() {
                return "MCP gateway connected (0 tools advertised).".to_string();
            }
            let summary: Vec<String> = tools
                .iter()
                .map(|d| format!("{}/{}", d.server, d.tool))
                .collect();
            format!(
                "Connected MCP tools ({}): {}",
                tools.len(),
                summary.join(", ")
            )
        })),
        on_run_review: Some(Box::new(move |target| {
            let cwd_cl = review_cwd.clone();
            let router_cl = review_router.clone();
            let driver_cl = Arc::clone(&review_driver);
            let agent_cl = review_agent_cfg.clone();
            Box::pin(async move {
                let out = run_code_review(
                    "tui_review",
                    &cwd_cl,
                    &target,
                    &router_cl,
                    driver_cl.as_ref(),
                    &agent_cl,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok((out.summary, out.findings))
            })
        })),
        on_switch_session: Some(Box::new(move |target_id, fork| {
            open_resumed_or_forked_session(
                target_id,
                fork,
                &sw_resolved,
                &sw_router,
                &sw_driver,
                &sw_index,
                sw_mcp.clone(),
                sw_plug.clone(),
                sw_tools.clone(),
            )
            .map_err(|e| e.to_string())
        })),
        on_memory_view: {
            Some(Box::new(move || {
                let store = tekvra_tools::MemoryStore::open(&mem_state, &mem_cwd);
                let content = store.load();
                let body = if content.trim().is_empty() {
                    "(empty — the model can add entries with the `memory_save` tool)"
                } else {
                    content.trim_end()
                };
                format!("{}\n\n{body}", store.path().display())
            }))
        },
    };

    tekvra_tui::run_fullscreen_tui(handle, state_index, snapshots, hooks).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn open_resumed_or_forked_session(
    source_session_id: &str,
    fork: bool,
    resolved: &ResolvedConfig,
    router: &ModelRouter,
    driver: &Arc<dyn ProviderDriver>,
    state_index: &Arc<Mutex<StateIndex>>,
    mcp_gateway: Option<Arc<dyn McpGateway>>,
    plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    extra_plugin_tools: Vec<tekvra_providers::ToolDefinition>,
) -> Result<(SessionHandle, Vec<RolloutRecord>)> {
    let source_path = rollout_path(&resolved.paths.state_dir, source_session_id);
    if !source_path.is_file() {
        bail!("session `{source_session_id}` rollout file does not exist");
    }
    let records = RolloutLog::read_records_readonly(&resolved.paths.state_dir, source_session_id)?;

    let (effective_id, parent_id) = if fork {
        let new_id = format!("thr_{}", Uuid::now_v7().simple());
        let dst = rollout_path(&resolved.paths.state_dir, &new_id);
        if let Some(parent_dir) = dst.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }
        let mut body = String::new();
        for rec in &records {
            if let Ok(line) = serde_json::to_string(rec) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        std::fs::write(&dst, body)?;
        (new_id, Some(source_session_id.to_string()))
    } else {
        (source_session_id.to_string(), None)
    };

    let handle = spawn_cli_session(
        Some(effective_id),
        parent_id,
        resolved,
        router,
        driver,
        state_index,
        mcp_gateway,
        plugin_executor,
        extra_plugin_tools,
    )?;
    Ok((handle, records))
}

fn scan_workspace_mention_files(root: &Path, max_files: usize) -> Vec<String> {
    if !root.exists() {
        return Vec::new();
    }
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != "target")
        .build();

    let mut out = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() || is_sensitive_credential_path(path) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !rel_str.is_empty() && !tekvra_tui::is_excluded_mention_path(&rel_str) {
                out.push(rel_str);
                if out.len() >= max_files {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}

#[allow(clippy::too_many_arguments)]
fn spawn_cli_session(
    session_id: Option<String>,
    parent_session_id: Option<String>,
    resolved: &ResolvedConfig,
    router: &ModelRouter,
    driver: &Arc<dyn ProviderDriver>,
    state_index: &Arc<Mutex<StateIndex>>,
    mcp_gateway: Option<Arc<dyn McpGateway>>,
    plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    extra_plugin_tools: Vec<tekvra_providers::ToolDefinition>,
) -> Result<SessionHandle> {
    Ok(spawn_session(SessionBootstrap {
        session_id,
        parent_session_id,
        workspace_root: resolved.cwd.clone(),
        state_dir: resolved.paths.state_dir.clone(),
        config: resolved.clone(),
        router: router.clone(),
        driver: Arc::clone(driver),
        state_index: Some(Arc::clone(state_index)),
        compaction: CompactionConfig::default(),
        mcp_gateway,
        plugin_executor,
        extra_plugin_tools,
        allow_subagents: true,
        subagent_depth: 0,
    })?)
}

fn parse_mode(raw: &str) -> Result<PermissionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "plan" => Ok(PermissionMode::Plan),
        "readonly" | "read-only" => Ok(PermissionMode::ReadOnly),
        "build" => Ok(PermissionMode::Build),
        "auto" => Ok(PermissionMode::Auto),
        "yolo" => Ok(PermissionMode::Yolo),
        other => bail!("unknown `--mode {other}`; expected `plan`, `build`, `auto`, or `yolo`"),
    }
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

async fn build_account_pool(
    resolved: &ResolvedConfig,
    registry: &ProviderRegistry,
    pin_raw: Option<&str>,
) -> Result<AccountPool> {
    let mut pool = AccountPool::new(0.02);
    if let Some(p) = pin_raw {
        let parsed_pin: AccountPin = p.parse()?;
        pool.set_pin(Some(parsed_pin));
    }

    let mut store = CredentialStore::load(&resolved.paths.state_dir)?;
    let now_secs = now_epoch_secs();

    // Auto-refresh any expired or expiring stored OAuth tokens before populating AccountPool!
    let snapshot_accounts: Vec<StoredOAuthAccount> = store.all_accounts().to_vec();
    for acct in snapshot_accounts {
        let is_expired = acct
            .expires_at_epoch_secs
            .is_some_and(|exp| exp <= now_secs.saturating_add(60));
        if is_expired
            && acct.refresh_token.is_some()
            && let Ok(endpoints) = OAuthProviderEndpoints::for_provider(&acct.provider)
            && let Ok(http) = build_oauth_http_client(&endpoints.token_url)
        {
            match refresh_oauth_account(
                &http,
                &endpoints.token_url,
                &endpoints.client_id,
                &acct,
                now_secs,
            )
            .await
            {
                Ok(renewed) => {
                    let _ = store.upsert(renewed);
                }
                Err(err) => {
                    eprintln!(
                        "notice: OAuth token refresh for `{}:{}` failed ({err}); keeping cached token",
                        acct.provider, acct.account_name
                    );
                }
            }
        }
    }

    for prov in registry.iter() {
        // 1. Stored OAuth / credential store entries
        for acct in store.list_for_provider(&prov.id) {
            pool.add_account(AccountCandidate {
                provider: prov.id.clone(),
                name: acct.account_name.clone(),
                token: acct.access_token.clone(),
                account_id: acct.account_id.clone(),
                remaining_quota_ratio: acct.remaining_quota_ratio,
                exhausted: false,
            });
        }

        // 2. Named accounts in `providers.toml`
        if let Some(custom_entry) = resolved.providers.providers.get(&prov.id) {
            for named in &custom_entry.accounts {
                let direct_or_env = named
                    .api_key
                    .clone()
                    .or_else(|| {
                        named.api_key_env.as_deref().and_then(|k| {
                            resolved
                                .project_dotenv
                                .get(k)
                                .cloned()
                                .or_else(|| std::env::var(k).ok())
                        })
                    })
                    .filter(|s| !s.trim().is_empty());
                if let Some(tok) = direct_or_env {
                    pool.add_account(AccountCandidate {
                        provider: prov.id.clone(),
                        name: named.name.clone(),
                        token: SecretToken::new(tok),
                        account_id: None,
                        remaining_quota_ratio: 1.0,
                        exhausted: false,
                    });
                }
            }
        }

        // 3. Default environment variable (or keyless local provider like `ollama` / `llamacpp`)
        if let Some(env_name) = &prov.default_api_key_env {
            if let Some(val) = resolved
                .project_dotenv
                .get(env_name)
                .cloned()
                .or_else(|| std::env::var(env_name).ok())
                .filter(|s| !s.trim().is_empty())
            {
                pool.add_account(AccountCandidate {
                    provider: prov.id.clone(),
                    name: "env".to_string(),
                    token: SecretToken::new(val),
                    account_id: None,
                    remaining_quota_ratio: 1.0,
                    exhausted: false,
                });
            }
        } else {
            pool.add_account(AccountCandidate {
                provider: prov.id.clone(),
                name: "local".to_string(),
                token: SecretToken::new("local-no-auth"),
                account_id: None,
                remaining_quota_ratio: 1.0,
                exhausted: false,
            });
        }
    }

    Ok(pool)
}

async fn handle_auth_command(resolved: &ResolvedConfig, args: AuthArgs) -> Result<()> {
    let mut store = CredentialStore::load(&resolved.paths.state_dir)?;
    match args.action {
        AuthAction::Login {
            provider,
            account_name,
            api_key,
            device_code,
            no_browser,
            max_polls,
        } => {
            if let Some(key) = api_key {
                store.upsert(StoredOAuthAccount {
                    provider: provider.clone(),
                    account_name: account_name.clone(),
                    account_id: None,
                    plan_type: None,
                    access_token: SecretToken::new(key),
                    refresh_token: None,
                    expires_at_epoch_secs: None,
                    remaining_quota_ratio: 1.0,
                })?;
                println!("Saved credential `{provider}:{account_name}`.");
                return Ok(());
            }

            let endpoints = OAuthProviderEndpoints::for_provider(&provider)?;
            let http = build_oauth_http_client(&endpoints.token_url)?;

            if device_code {
                let session = request_device_code(
                    &http,
                    &endpoints.device_code_url,
                    &endpoints.client_id,
                    &endpoints.scope,
                )
                .await?;
                println!(
                    "Open {} and enter verification code: {}",
                    session.verification_uri, session.user_code
                );
                let _ = std::io::stdout().flush();

                let account = poll_device_code_token(
                    &http,
                    &endpoints.token_url,
                    &provider,
                    &account_name,
                    &endpoints.client_id,
                    &session,
                    max_polls,
                )
                .await?;
                let plan_badge = account
                    .plan_type
                    .as_deref()
                    .map(|p| format!(" (plan={p})"))
                    .unwrap_or_default();
                store.upsert(account)?;
                println!("Authenticated `{provider}:{account_name}`{plan_badge}.");
            } else {
                let (verifier, challenge) = generate_pkce_pair();
                let (listener, auth_url) = LoopbackOAuthListener::bind(
                    &endpoints.authorize_url,
                    &endpoints.client_id,
                    &challenge,
                    &endpoints.scope,
                )
                .await?;
                let redirect_uri = listener.redirect_uri().to_string();

                println!("Authorize in browser: {auth_url}");
                let _ = std::io::stdout().flush();

                if !no_browser {
                    try_open_browser_quietly(&auth_url);
                }

                let code = listener.accept_authorization_code().await?;
                let account = exchange_oauth_token(
                    &http,
                    &endpoints.token_url,
                    &provider,
                    &account_name,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", &redirect_uri),
                        ("client_id", &endpoints.client_id),
                        ("code_verifier", &verifier),
                    ],
                    now_epoch_secs(),
                )
                .await?;
                let plan_badge = account
                    .plan_type
                    .as_deref()
                    .map(|p| format!(" (plan={p})"))
                    .unwrap_or_default();
                store.upsert(account)?;
                println!("Authenticated `{provider}:{account_name}`{plan_badge}.");
            }
        }
        AuthAction::Status => {
            for p in ["openai", "anthropic", "gemini", "github-copilot"] {
                let list = store.list_for_provider(p);
                let names: Vec<String> = list
                    .iter()
                    .map(|a| match &a.plan_type {
                        Some(plan) => format!("{}({plan})", a.account_name),
                        None => a.account_name.clone(),
                    })
                    .collect();
                if names.is_empty() {
                    println!("{p}: 0 stored account(s)");
                } else {
                    println!(
                        "{p}: {} stored account(s) [{}]",
                        list.len(),
                        names.join(", ")
                    );
                }
            }
        }
        AuthAction::Logout {
            provider,
            account_name,
        } => {
            let removed = store.remove(&provider, &account_name)?;
            println!("Removed `{provider}:{account_name}`: {removed}");
        }
    }
    Ok(())
}

fn try_open_browser_quietly(url: &str) {
    #[cfg(target_os = "macos")]
    let launcher = "open";
    #[cfg(windows)]
    let launcher = "rundll32";
    #[cfg(not(any(target_os = "macos", windows)))]
    let launcher = "xdg-open";

    let mut command = std::process::Command::new(launcher);
    #[cfg(windows)]
    {
        // rundll32 expects the DLL entry point as one argument:
        // `rundll32 url.dll,FileProtocolHandler <url>`.
        command.arg("url.dll,FileProtocolHandler").arg(url);
    }
    #[cfg(not(windows))]
    {
        command.arg(url);
    }
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
