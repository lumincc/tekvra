//! Declarative TOML configuration, self-migration, project folder trust, and instruction/skill discovery.

pub mod discovery;
pub mod loader;
pub mod schema;
pub mod trust;

use std::path::PathBuf;

use thiserror::Error;

pub use discovery::{
    InstructionDoc, SkillMetadata, discover_instructions, discover_skills, find_git_root,
    load_skill_body, merge_instructions_markdown, strip_yaml_frontmatter,
};
pub use loader::{CliConfigOverrides, ConfigPaths, ResolvedConfig, load_config};
pub use schema::{
    AgentSettings, CURRENT_CONFIG_VERSION, ConfigNotice, FeatureFlags, LspServerConfig,
    McpServerConfig, ModelJobRouting, PermissionRule, PermissionsToml, ProfileOverride,
    ProviderAccountConfig, ProviderEntryConfig, ProvidersToml, SpendWindow, TekvraToml,
};
pub use trust::{TrustStore, canonicalize_or_clean};

/// Errors produced while loading, migrating, or validating Tekvra configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Filesystem I/O error with associated file path.
    #[error("failed I/O on `{path}`: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// TOML syntax or schema deserialization error.
    #[error("invalid TOML in `{path}`: {message}")]
    ParseToml {
        /// Path of the malformed TOML file.
        path: PathBuf,
        /// Parser diagnostic message.
        message: String,
    },
    /// Failed to serialize TOML document.
    #[error("failed to serialize TOML: {message}")]
    SerializeToml {
        /// Serializer diagnostic message.
        message: String,
    },
    /// Config file declares an unsupported schema version.
    #[error("unsupported config version {version} in `{path}` (max supported: {supported})")]
    UnsupportedVersion {
        /// Config file path.
        path: PathBuf,
        /// Version found in file.
        version: i64,
        /// Maximum supported schema version.
        supported: u32,
    },
    /// Requested `--profile <name>` is not defined in `tekvra.toml`.
    #[error("unknown configuration profile `{profile}`")]
    UnknownProfile {
        /// Missing profile name.
        profile: String,
    },
    /// Invalid `-c key=value` CLI override.
    #[error("invalid CLI config override: {message}")]
    InvalidOverride {
        /// Diagnostic message.
        message: String,
    },
}
