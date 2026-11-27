//! Unified `tekvra` CLI binary entrypoint (`tekvra`, `exec`, `review`, `app-server`, `acp`, `auth`).

mod exec_runner;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use exec_runner::{ExecRunOptions, SchemaValidationFailed, run_headless_exec};
use tekvra_acp::{AcpServerDeps, serve_acp_stdio};
use tekvra_app_server::{
    AppServerState, AppServerStateInit, parse_listen_endpoint, serve_endpoint,
};
use tekvra_config::{CliConfigOverrides, ConfigPaths, ResolvedConfig, load_config};
use tekvra_core::{
    CompactionConfig, PluginToolExecutor, ReviewTarget, SessionBootstrap, run_code_review,
    spawn_session,
};
use tekvra_lua::{LuaHostOptions, PluginHost};
use tekvra_protocol::PermissionMode;
use tekvra_providers::{
    AccountCandidate, AccountPin, AccountPool, CredentialStore, HttpProviderDriver,
    LoopbackOAuthListener, ModelRouter, OAuthProviderEndpoints, ProviderDriver, ProviderRegistry,
    SecretToken, StoredOAuthAccount, build_oauth_http_client, exchange_oauth_token,
    generate_pkce_pair, poll_device_code_token, refresh_oauth_account, request_device_code,
};
use tekvra_storage::{StateIndex, WorkspaceSnapshotStore};
use tekvra_tools::{ConfiguredMcpGateway, McpGateway};
use tekvra_tui::TuiRuntimeHooks;

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

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Execute a prompt headlessly with optional JSONL events, `stream-json`, and `--output-schema` validation.
    Exec(ExecArgs),
    /// Run a dedicated read-only code review over Git diffs.
    Review(ReviewArgs),
    /// Run the Codex-compatible v2 JSON-RPC 2.0 `app-server` (`stdio://`, `unix://`, `ws://`).
    AppServer(AppServerArgs),
    /// Run the Agent Client Protocol (`acp`) stdio server for Zed/Neovim/JetBrains.
    Acp,
    /// Manage OAuth and API key credentials (`login`, `status`, `logout`).
    Auth(AuthArgs),
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
    /// Input format (`text` or `stream-json`).
    #[arg(long, default_value = "text")]
    input_format: String,
    /// Output format (`text`, `json`, or `stream-json`).
    #[arg(long, default_value = "text")]
    output_format: String,
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

    let mut resolved = load_config(ConfigPaths::from_env(), &cwd, &overrides)?;
    if let Some(mode_str) = &cli.mode {
        resolved.settings.agent.default_mode = parse_mode(mode_str)?;
    }

    if let Some(Commands::Auth(auth_args)) = cli.command {
        return handle_auth_command(&resolved, auth_args).await;
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
        let handle = spawn_cli_session(
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
                prompt: Some(print_prompt),
                json_events: false,
                output_schema: None,
                input_format: "text".to_string(),
                output_format: "text".to_string(),
                mode,
            },
        )
        .await;
    }

    match cli.command {
        Some(Commands::Exec(args)) => {
            let mode = resolved.settings.agent.default_mode;
            let handle = spawn_cli_session(
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
                    prompt: args.prompt,
                    json_events: args.json,
                    output_schema: args.output_schema,
                    input_format: args.input_format,
                    output_format: args.output_format,
                    mode,
                },
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
        None => {
            let handle = spawn_cli_session(
                &resolved,
                &router,
                &driver,
                &state_index,
                mcp_gateway_opt.clone(),
                plugin_exec_opt,
                extra_tools,
            )?;
            let host_for_reload = plugin_host.clone();
            let host_for_cmd = plugin_host.clone();
            let mcp_for_tui = mcp_gateway_opt.clone();
            let review_cwd = resolved.cwd.clone();
            let review_router = router.clone();
            let review_driver = Arc::clone(&driver);
            let review_agent_cfg = resolved.settings.agent.clone();

            let hooks = TuiRuntimeHooks {
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
            };

            tekvra_tui::run_fullscreen_tui(handle, state_index, snapshots, hooks).await?;
            Ok(())
        }
    }
}

fn spawn_cli_session(
    resolved: &ResolvedConfig,
    router: &ModelRouter,
    driver: &Arc<dyn ProviderDriver>,
    state_index: &Arc<Mutex<StateIndex>>,
    mcp_gateway: Option<Arc<dyn McpGateway>>,
    plugin_executor: Option<Arc<dyn PluginToolExecutor>>,
    extra_plugin_tools: Vec<tekvra_providers::ToolDefinition>,
) -> Result<tekvra_core::SessionHandle> {
    Ok(spawn_session(SessionBootstrap {
        session_id: None,
        parent_session_id: None,
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
    #[cfg(not(target_os = "macos"))]
    let launcher = "xdg-open";

    let _ = std::process::Command::new(launcher)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
