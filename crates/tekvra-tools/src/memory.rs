//! Cross-session project memory (`<state_dir>/memory/<project-slug>/MEMORY.md`).
//!
//! Human-editable, git-diffable, zero schema risk: injection reads one file; the prompt-cache
//! prefix stays byte-stable for the whole session because the file is read once at spawn.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use tekvra_config::canonicalize_or_clean;

use crate::ToolsError;

/// Maximum accepted size for one `memory_save` entry (8 KiB).
pub const MEMORY_ENTRY_LIMIT: usize = 8 * 1024;
/// Maximum size of the whole `MEMORY.md` file (256 KiB).
pub const MEMORY_FILE_LIMIT: usize = 256 * 1024;

/// Per-project memory file handle (deterministic path derivation, no schema).
#[derive(Debug, Clone)]
pub struct MemoryStore {
    path: PathBuf,
}

impl MemoryStore {
    /// Opens (does not create) the memory file for `project_root` under `state_dir`.
    #[must_use]
    pub fn open(state_dir: &Path, project_root: &Path) -> Self {
        Self {
            path: state_dir
                .join("memory")
                .join(memory_slug(project_root))
                .join("MEMORY.md"),
        }
    }

    /// Memory file path (for `/memory` display and tool confirmations).
    #[must_use]
    pub const fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Reads the full memory file; empty string when it does not exist yet.
    #[must_use]
    pub fn load(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    /// Appends one dated entry (`- YYYY-MM-DD: <entry>`), enforcing the entry and file caps.
    ///
    /// # Errors
    /// Returns [`CoreError`] when the entry exceeds [`MEMORY_ENTRY_LIMIT`], appending would
    /// exceed [`MEMORY_FILE_LIMIT`], or the file cannot be written.
    pub fn append(&self, entry: &str) -> Result<(), ToolsError> {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(ToolsError::InvalidInput {
                message: "memory entry is empty".to_string(),
            });
        }
        if trimmed.len() > MEMORY_ENTRY_LIMIT {
            return Err(ToolsError::InvalidInput {
                message: format!(
                    "memory entry is {} bytes; the limit is {MEMORY_ENTRY_LIMIT}",
                    trimmed.len()
                ),
            });
        }
        let existing = self.load();
        // UTC calendar date (documented: no tz database in this build).
        let date = Timestamp::now()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .date()
            .to_string();
        let line = format!("- {date}: {trimmed}\n");
        if existing.len() + line.len() > MEMORY_FILE_LIMIT {
            return Err(ToolsError::InvalidInput {
                message: format!(
                    "memory file is full: {} of {MEMORY_FILE_LIMIT} bytes; trim MEMORY.md first",
                    existing.len()
                ),
            });
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ToolsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|source| ToolsError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            file.write_all(line.as_bytes())
                .map_err(|source| ToolsError::Io {
                    path: self.path.clone(),
                    source,
                })?;
        }
        Ok(())
    }
}

/// Deterministic filesystem-safe slug for a project root (mirrors the project-id keying:
/// canonical path lowercased, non `[a-z0-9_-]` collapsed to `_`).
#[must_use]
pub fn memory_slug(project_root: &Path) -> String {
    let canonical = canonicalize_or_clean(project_root);
    canonical
        .to_string_lossy()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
