//! Configuration loading, automatic global schema migration, CLI overrides, and trust filtering.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use tekvra_protocol::RuleAction;
use uuid::Uuid;

use crate::ConfigError;
use crate::discovery::{
    InstructionDoc, SkillMetadata, discover_instructions, discover_skills, find_git_root,
};
use crate::schema::{
    CURRENT_CONFIG_VERSION, ConfigNotice, FeatureFlags, PermissionRule, PermissionsToml,
    ProfileOverride, ProvidersToml, TekvraToml,
};
use crate::trust::{TrustStore, atomic_write_str, canonicalize_or_clean};

/// Explicit filesystem roots for loading Tekvra configuration and state.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Global config directory (e.g., `~/.config/tekvra` or `~/.config/tekvra-<namespace>`).
    pub global_config_dir: PathBuf,
    /// Global state directory (e.g., `~/.local/share/tekvra` or `~/.local/share/tekvra-<namespace>`).
    pub state_dir: PathBuf,
    /// User home directory, if known.
    pub home_dir: Option<PathBuf>,
}

impl ConfigPaths {
    /// Resolves default XDG/platform paths respecting `TEKVRA_CONFIG_DIR`, `TEKVRA_STATE_DIR`,
    /// and `TEKVRA_NAMESPACE`.
    #[must_use]
    pub fn from_env() -> Self {
        let home_dir = env::var_os("HOME").map(PathBuf::from);
        let namespace = env::var("TEKVRA_NAMESPACE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let folder_name = match &namespace {
            Some(ns) => format!("tekvra-{ns}"),
            None => "tekvra".to_string(),
        };

        let global_config_dir = env::var_os("TEKVRA_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join(&folder_name)))
            .or_else(|| {
                home_dir
                    .as_ref()
                    .map(|h| h.join(".config").join(&folder_name))
            })
            .unwrap_or_else(|| PathBuf::from(".tekvra-config"));

        let state_dir = env::var_os("TEKVRA_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_DATA_HOME").map(|p| PathBuf::from(p).join(&folder_name)))
            .or_else(|| {
                home_dir
                    .as_ref()
                    .map(|h| h.join(".local").join("share").join(&folder_name))
            })
            .unwrap_or_else(|| PathBuf::from(".tekvra-state"));

        Self {
            global_config_dir,
            state_dir,
            home_dir,
        }
    }

    /// Directory holding versioned backups created during automatic global TOML upgrades.
    #[must_use]
    pub fn backup_dir(&self) -> PathBuf {
        self.state_dir.join("backups")
    }
}

/// Command-line overrides applied on top of global and project TOML files.
#[derive(Debug, Clone, Default)]
pub struct CliConfigOverrides {
    /// Selected `--profile <name>`.
    pub profile: Option<String>,
    /// Dotted `-c key=value` overrides (parsed as TOML values, falling back to string).
    pub dotted_overrides: Vec<(String, String)>,
    /// Explicit `--enable <feature>` flags (e.g., `"lua_plugins"`).
    pub enabled_features: Vec<String>,
    /// Whether `--no-plugins` was passed on the command line.
    pub no_plugins: bool,
}

/// Fully resolved, validated configuration for a Tekvra process/session.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Resolved paths.
    pub paths: ConfigPaths,
    /// Canonical working directory.
    pub cwd: PathBuf,
    /// Canonical project root (Git root or `cwd`).
    pub project_root: PathBuf,
    /// Whether `project_root` is currently trusted.
    pub project_trusted: bool,
    /// Effective `tekvra.toml` settings.
    pub settings: TekvraToml,
    /// Effective permission rules (project `deny` rules included even when untrusted!).
    pub permission_rules: Vec<PermissionRule>,
    /// Effective provider configurations.
    pub providers: ProvidersToml,
    /// Environment variables loaded from trusted project `.env` (empty when untrusted).
    pub project_dotenv: BTreeMap<String, String>,
    /// Discovered `AGENTS.md` / `AGENTS.override.md` documents in root-to-leaf order.
    pub instructions: Vec<InstructionDoc>,
    /// Discovered `SKILL.md` metadata entries.
    pub skills: Vec<SkillMetadata>,
    /// Candidate Lua entrypoints eligible to load ONLY when `lua_plugins_active()` is true.
    pub eligible_lua_entrypoints: Vec<PathBuf>,
    /// Diagnostic notices emitted during loading, migration, or trust filtering.
    pub notices: Vec<ConfigNotice>,
}

impl ResolvedConfig {
    /// Returns `true` ONLY when `experimental.lua_plugins` is enabled in global config (or CLI `--enable`)
    /// AND `--no-plugins` was not passed.
    #[must_use]
    pub const fn lua_plugins_active(&self) -> bool {
        self.settings.experimental.lua_plugins
    }

    /// Resolves the `provider/model` string for a given job role (`"main"`, `"plan"`, `"subagent"`,
    /// `"compact"`, `"review"`, `"goal"`, `"title"`, `"fast"`).
    #[must_use]
    pub fn model_for_job(&self, job: &str) -> &str {
        let routing = &self.settings.models;
        let bound = match job {
            "main" => routing.main.as_deref(),
            "plan" => routing.plan.as_deref(),
            "subagent" => routing.subagent.as_deref(),
            "compact" => routing.compact.as_deref(),
            "review" => routing.review.as_deref(),
            "goal" => routing.goal.as_deref(),
            "title" => routing.title.as_deref(),
            "fast" => routing.fast.as_deref(),
            _ => None,
        };
        bound
            .or(routing.main.as_deref())
            .unwrap_or(self.settings.model.as_str())
    }
}

/// Loads, migrates, and merges global + project TOML configurations without instantiating any Lua VM.
///
/// # Errors
/// Returns [`ConfigError`] if TOML syntax is invalid, an unsupported future schema version is declared,
/// or a requested `--profile` / `-c key=value` override is malformed.
pub fn load_config(
    paths: ConfigPaths,
    cwd: &Path,
    overrides: &CliConfigOverrides,
) -> Result<ResolvedConfig, ConfigError> {
    let mut notices = Vec::new();
    let canonical_cwd = canonicalize_or_clean(cwd);
    let project_root = find_git_root(&canonical_cwd).unwrap_or_else(|| canonical_cwd.clone());

    let trust_store = TrustStore::load(&paths.state_dir)?;
    let project_trusted = trust_store.is_trusted(&project_root);

    // 1. Load & self-migrate global TOML files
    let global_tekvra_path = paths.global_config_dir.join("tekvra.toml");
    let mut global_val = if global_tekvra_path.is_file() {
        read_and_migrate_global_toml(&global_tekvra_path, &paths.backup_dir(), &mut notices)?
    } else {
        toml::Value::try_from(TekvraToml::default()).expect("default TekvraToml serializes")
    };

    let global_experimental: FeatureFlags = global_val
        .get("experimental")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|err| ConfigError::ParseToml {
            path: global_tekvra_path.clone(),
            message: err.to_string(),
        })?
        .unwrap_or_default();

    let global_perms_path = paths.global_config_dir.join("permissions.toml");
    let global_perms: PermissionsToml = if global_perms_path.is_file() {
        let v =
            read_and_migrate_global_toml(&global_perms_path, &paths.backup_dir(), &mut notices)?;
        v.try_into()
            .map_err(|err: toml::de::Error| ConfigError::ParseToml {
                path: global_perms_path.clone(),
                message: err.to_string(),
            })?
    } else {
        PermissionsToml::default()
    };

    let global_providers_path = paths.global_config_dir.join("providers.toml");
    let mut providers: ProvidersToml = if global_providers_path.is_file() {
        let v = read_and_migrate_global_toml(
            &global_providers_path,
            &paths.backup_dir(),
            &mut notices,
        )?;
        v.try_into()
            .map_err(|err: toml::de::Error| ConfigError::ParseToml {
                path: global_providers_path.clone(),
                message: err.to_string(),
            })?
    } else {
        ProvidersToml::default()
    };

    // 2. Inspect project `.tekvra/` with strict trust gating
    let project_cfg_dir = project_root.join(".tekvra");
    let mut permission_rules = global_perms.rules;
    let mut project_dotenv = BTreeMap::new();

    let project_perms_path = project_cfg_dir.join("permissions.toml");
    if project_perms_path.is_file() {
        let raw = fs::read_to_string(&project_perms_path).map_err(|source| ConfigError::Io {
            path: project_perms_path.clone(),
            source,
        })?;
        let proj_perms: PermissionsToml =
            toml::from_str(&raw).map_err(|err| ConfigError::ParseToml {
                path: project_perms_path.clone(),
                message: err.to_string(),
            })?;
        if project_trusted {
            permission_rules.extend(proj_perms.rules);
        } else {
            let mut withheld = 0usize;
            for rule in proj_perms.rules {
                if rule.action == RuleAction::Deny {
                    permission_rules.push(rule);
                } else {
                    withheld += 1;
                }
            }
            if withheld > 0 {
                notices.push(ConfigNotice {
                    code: "untrusted_project_permissions".to_string(),
                    message: format!(
                        "Withheld {withheld} non-deny permission rule(s) from untrusted project; project deny rules remain enforced."
                    ),
                    path: Some(project_perms_path),
                });
            }
        }
    }

    let project_tekvra_path = project_cfg_dir.join("tekvra.toml");
    if project_tekvra_path.is_file() {
        let raw = fs::read_to_string(&project_tekvra_path).map_err(|source| ConfigError::Io {
            path: project_tekvra_path.clone(),
            source,
        })?;
        let mut proj_val: toml::Value =
            toml::from_str(&raw).map_err(|err| ConfigError::ParseToml {
                path: project_tekvra_path.clone(),
                message: err.to_string(),
            })?;

        // Global-only `[experimental]` enforcement: NEVER honor project-level `[experimental]`.
        if let Some(tbl) = proj_val.as_table_mut()
            && tbl.remove("experimental").is_some()
        {
            notices.push(ConfigNotice {
                code: "project_experimental_ignored".to_string(),
                message: "Project `.tekvra/tekvra.toml` cannot enable `[experimental]` flags; configure `[experimental]` in global `tekvra.toml` only.".to_string(),
                path: Some(project_tekvra_path.clone()),
            });
        }

        if project_trusted {
            merge_toml_tables(&mut global_val, proj_val);
        } else {
            notices.push(ConfigNotice {
                code: "untrusted_project_config".to_string(),
                message: "Withheld `.tekvra/tekvra.toml` (including project MCP servers) until folder trust is granted.".to_string(),
                path: Some(project_tekvra_path),
            });
        }
    }

    let project_providers_path = project_cfg_dir.join("providers.toml");
    if project_providers_path.is_file() {
        if project_trusted {
            let raw =
                fs::read_to_string(&project_providers_path).map_err(|source| ConfigError::Io {
                    path: project_providers_path.clone(),
                    source,
                })?;
            let proj_prov: ProvidersToml =
                toml::from_str(&raw).map_err(|err| ConfigError::ParseToml {
                    path: project_providers_path.clone(),
                    message: err.to_string(),
                })?;
            for (k, v) in proj_prov.providers {
                providers.providers.insert(k, v);
            }
        } else {
            notices.push(ConfigNotice {
                code: "untrusted_project_providers".to_string(),
                message: "Withheld `.tekvra/providers.toml` until folder trust is granted."
                    .to_string(),
                path: Some(project_providers_path),
            });
        }
    }

    let dotenv_path = project_root.join(".env");
    if dotenv_path.is_file() {
        if project_trusted {
            if let Ok(iter) = dotenvy::from_path_iter(&dotenv_path) {
                for item in iter.flatten() {
                    project_dotenv.insert(item.0, item.1);
                }
            }
        } else {
            notices.push(ConfigNotice {
                code: "untrusted_dotenv_withheld".to_string(),
                message: "Withheld project `.env` variables until folder trust is granted."
                    .to_string(),
                path: Some(dotenv_path),
            });
        }
    }

    // 3. Apply `-c key=value` dotted CLI overrides onto the merged TOML value before deserializing
    for (dotted_key, raw_val) in &overrides.dotted_overrides {
        apply_dotted_override(&mut global_val, dotted_key, raw_val)?;
    }

    let mut settings: TekvraToml =
        global_val
            .try_into()
            .map_err(|err: toml::de::Error| ConfigError::InvalidOverride {
                message: err.to_string(),
            })?;

    // 4. Apply `--profile <name>` (and then re-apply explicit `-c` overrides so `-c` wins over `--profile`!)
    if let Some(profile_name) = &overrides.profile {
        let profile = settings
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| ConfigError::UnknownProfile {
                profile: profile_name.clone(),
            })?;
        apply_profile(&mut settings, &profile);

        if !overrides.dotted_overrides.is_empty() {
            let mut val = toml::Value::try_from(&settings)
                .expect("TekvraToml serializes back to toml::Value");
            for (dotted_key, raw_val) in &overrides.dotted_overrides {
                apply_dotted_override(&mut val, dotted_key, raw_val)?;
            }
            settings =
                val.try_into()
                    .map_err(|err: toml::de::Error| ConfigError::InvalidOverride {
                        message: err.to_string(),
                    })?;
        }
    }

    // 5. Enforce global-only `[experimental]` + `--enable` + `--no-plugins`
    settings.experimental = global_experimental;
    for feat in &overrides.enabled_features {
        if feat == "lua_plugins" {
            settings.experimental.lua_plugins = true;
        }
    }
    if overrides.no_plugins {
        settings.experimental.lua_plugins = false;
    }

    // 6. Inspect Lua entrypoints without starting any Lua VM
    let mut eligible_lua_entrypoints = Vec::new();
    let global_init_lua = paths.global_config_dir.join("init.lua");
    let project_init_lua = project_cfg_dir.join("init.lua");

    if settings.experimental.lua_plugins {
        if global_init_lua.is_file() {
            eligible_lua_entrypoints.push(global_init_lua);
        }
        if project_init_lua.is_file() {
            if project_trusted {
                eligible_lua_entrypoints.push(project_init_lua);
            } else {
                notices.push(ConfigNotice {
                    code: "untrusted_project_lua_withheld".to_string(),
                    message: "Withheld `.tekvra/init.lua` until folder trust is granted."
                        .to_string(),
                    path: Some(project_init_lua),
                });
            }
        }
    } else {
        if global_init_lua.is_file() {
            notices.push(ConfigNotice {
                code: "lua_plugins_disabled_notice".to_string(),
                message: format!(
                    "Skipped `{}` because `experimental.lua_plugins` is false by default. Migrate settings to `tekvra.toml` or set `experimental.lua_plugins = true` in global `tekvra.toml`.",
                    global_init_lua.display()
                ),
                path: Some(global_init_lua),
            });
        }
        if project_init_lua.is_file() {
            notices.push(ConfigNotice {
                code: "lua_plugins_disabled_notice".to_string(),
                message: format!(
                    "Skipped `{}` because `experimental.lua_plugins` is disabled.",
                    project_init_lua.display()
                ),
                path: Some(project_init_lua),
            });
        }
    }

    // 7. Discover hierarchical `AGENTS.md` and `SKILL.md`
    let instructions = discover_instructions(&paths.global_config_dir, &canonical_cwd);
    let skills = discover_skills(
        &paths.global_config_dir,
        paths.home_dir.as_deref(),
        &project_root,
    );

    // 8. Per-server MCP timeout sanity: `0` would make the streaming budget fire
    //    instantly, so keep the built-in default and tell the user.
    for (name, server) in &settings.mcp_servers {
        for (key, value) in [
            ("http_header_timeout_sec", server.http_header_timeout_sec),
            ("http_idle_timeout_sec", server.http_idle_timeout_sec),
        ] {
            if value == Some(0) {
                notices.push(ConfigNotice {
                    code: "invalid_mcp_http_timeout".to_string(),
                    message: format!(
                        "`mcp_servers.{name}.{key} = 0` would disable the streaming budget; keeping the built-in default."
                    ),
                    path: None,
                });
            }
        }
    }

    Ok(ResolvedConfig {
        paths,
        cwd: canonical_cwd,
        project_root,
        project_trusted,
        settings,
        permission_rules,
        providers,
        project_dotenv,
        instructions,
        skills,
        eligible_lua_entrypoints,
        notices,
    })
}

fn apply_profile(settings: &mut TekvraToml, profile: &ProfileOverride) {
    if let Some(m) = &profile.model {
        settings.model.clone_from(m);
    }
    if let Some(mode) = profile.default_mode {
        settings.agent.default_mode = mode;
    }
    if let Some(effort) = &profile.reasoning_effort {
        settings.agent.reasoning_effort = Some(effort.clone());
    }
    macro_rules! merge_job {
        ($field:ident) => {
            if let Some(v) = &profile.models.$field {
                settings.models.$field = Some(v.clone());
            }
        };
    }
    merge_job!(main);
    merge_job!(plan);
    merge_job!(subagent);
    merge_job!(compact);
    merge_job!(review);
    merge_job!(goal);
    merge_job!(title);
    merge_job!(fast);
}

fn read_and_migrate_global_toml(
    path: &Path,
    backup_dir: &Path,
    notices: &mut Vec<ConfigNotice>,
) -> Result<toml::Value, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut doc: toml_edit::DocumentMut =
        raw.parse()
            .map_err(|err: toml_edit::TomlError| ConfigError::ParseToml {
                path: path.to_path_buf(),
                message: err.to_string(),
            })?;

    let declared_version = doc
        .get("version")
        .and_then(toml_edit::Item::as_integer)
        .unwrap_or(1);

    if declared_version <= 0 || declared_version as u32 > CURRENT_CONFIG_VERSION {
        return Err(ConfigError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: declared_version,
            supported: CURRENT_CONFIG_VERSION,
        });
    }

    if (declared_version as u32) < CURRENT_CONFIG_VERSION {
        fs::create_dir_all(backup_dir).map_err(|source| ConfigError::Io {
            path: backup_dir.to_path_buf(),
            source,
        })?;
        let stem = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("config.toml");
        let backup_name = format!("{stem}.v{declared_version}.{}.bak", Uuid::now_v7());
        let backup_path = backup_dir.join(backup_name);
        atomic_write_str(&backup_path, &raw)?;

        // Migrate v1 -> v2: rename legacy `default_model` -> `model`, stamp `version = 2`
        if declared_version == 1 {
            if let Some(legacy_model) = doc.remove("default_model")
                && !doc.contains_key("model")
            {
                doc.insert("model", legacy_model);
            }
            doc["version"] = toml_edit::value(i64::from(CURRENT_CONFIG_VERSION));
        }

        let upgraded = doc.to_string();
        atomic_write_str(path, &upgraded)?;
        notices.push(ConfigNotice {
            code: "global_config_migrated".to_string(),
            message: format!(
                "Upgraded `{}` from schema v{declared_version} to v{CURRENT_CONFIG_VERSION} (backup saved at `{}`).",
                path.display(),
                backup_path.display()
            ),
            path: Some(backup_path),
        });
        return toml::from_str(&upgraded).map_err(|err| ConfigError::ParseToml {
            path: path.to_path_buf(),
            message: err.to_string(),
        });
    }

    toml::from_str(&raw).map_err(|err| ConfigError::ParseToml {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn merge_toml_tables(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_tbl), toml::Value::Table(over_tbl)) => {
            for (k, v) in over_tbl {
                match base_tbl.get_mut(&k) {
                    Some(existing) => merge_toml_tables(existing, v),
                    None => {
                        base_tbl.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn apply_dotted_override(
    root: &mut toml::Value,
    dotted_key: &str,
    raw_val: &str,
) -> Result<(), ConfigError> {
    let segments: Vec<&str> = dotted_key
        .split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Err(ConfigError::InvalidOverride {
            message: format!("empty override key in `{dotted_key}={raw_val}`"),
        });
    }

    let parsed_value = parse_override_scalar(raw_val);
    let mut cursor = root;
    for seg in &segments[..segments.len() - 1] {
        let tbl = cursor
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidOverride {
                message: format!("cannot traverse non-table segment `{seg}` in `{dotted_key}`"),
            })?;
        cursor = tbl
            .entry((*seg).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let leaf = segments[segments.len() - 1];
    let tbl = cursor
        .as_table_mut()
        .ok_or_else(|| ConfigError::InvalidOverride {
            message: format!("cannot set `{leaf}` on non-table in `{dotted_key}`"),
        })?;
    tbl.insert(leaf.to_string(), parsed_value);
    Ok(())
}

fn parse_override_scalar(raw: &str) -> toml::Value {
    let wrapped = format!("v = {raw}");
    if let Ok(mut tbl) = toml::from_str::<toml::map::Map<String, toml::Value>>(&wrapped)
        && let Some(val) = tbl.remove("v")
    {
        return val;
    }
    toml::Value::String(raw.to_string())
}
