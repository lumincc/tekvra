//! Canonical project folder trust store (`trusted_folders.toml`).

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::ConfigError;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    trusted_paths: BTreeSet<PathBuf>,
}

/// Manages user-approved canonical project roots in `<state_dir>/trusted_folders.toml`.
#[derive(Debug, Clone)]
pub struct TrustStore {
    file_path: PathBuf,
    trusted: BTreeSet<PathBuf>,
}

impl TrustStore {
    /// Loads the trust store from `state_dir/trusted_folders.toml` (empty if absent).
    ///
    /// # Errors
    /// Returns [`ConfigError`] if an existing trust store file cannot be read or parsed.
    pub fn load(state_dir: &Path) -> Result<Self, ConfigError> {
        let file_path = state_dir.join("trusted_folders.toml");
        if !file_path.exists() {
            return Ok(Self {
                file_path,
                trusted: BTreeSet::new(),
            });
        }
        let raw = fs::read_to_string(&file_path).map_err(|source| ConfigError::Io {
            path: file_path.clone(),
            source,
        })?;
        let parsed: TrustFile = toml::from_str(&raw).map_err(|err| ConfigError::ParseToml {
            path: file_path.clone(),
            message: err.to_string(),
        })?;
        Ok(Self {
            file_path,
            trusted: parsed.trusted_paths,
        })
    }

    /// Returns `true` if `project_dir` (or an ancestor directory) has been explicitly trusted.
    #[must_use]
    pub fn is_trusted(&self, project_dir: &Path) -> bool {
        let canonical = canonicalize_or_clean(project_dir);
        self.trusted
            .iter()
            .any(|trusted_root| canonical.starts_with(trusted_root))
    }

    /// Grants trust to `project_dir` (canonicalized) and persists atomically to disk.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if persisting the trust store fails.
    pub fn grant_trust(&mut self, project_dir: &Path) -> Result<PathBuf, ConfigError> {
        let canonical = canonicalize_or_clean(project_dir);
        self.trusted.insert(canonical.clone());
        self.persist()?;
        Ok(canonical)
    }

    /// Revokes trust for `project_dir` and persists atomically to disk.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if persisting the trust store fails.
    pub fn revoke_trust(&mut self, project_dir: &Path) -> Result<bool, ConfigError> {
        let canonical = canonicalize_or_clean(project_dir);
        let removed = self.trusted.remove(&canonical);
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> Result<(), ConfigError> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let doc = TrustFile {
            trusted_paths: self.trusted.clone(),
        };
        let encoded = toml::to_string_pretty(&doc).map_err(|err| ConfigError::SerializeToml {
            message: err.to_string(),
        })?;
        atomic_write_str(&self.file_path, &encoded)
    }
}

/// Canonicalizes `path` when it exists on disk, falling back to the original path otherwise.
#[must_use]
pub fn canonicalize_or_clean(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Writes `contents` atomically to `target` via a sibling temporary file.
pub(crate) fn atomic_write_str(target: &Path, contents: &str) -> Result<(), ConfigError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(contents.as_bytes())
        .map_err(|source| ConfigError::Io {
            path: target.to_path_buf(),
            source,
        })?;
    tmp.persist(target).map_err(|err| ConfigError::Io {
        path: target.to_path_buf(),
        source: err.error,
    })?;
    Ok(())
}
