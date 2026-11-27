//! Append-only JSONL session rollout files, truncated-tail crash recovery, and single-writer leases.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tekvra_protocol::{TokenUsage, ToolOutput};
use tekvra_providers::ProviderMessage;

use crate::StorageError;

/// Canonical metadata header written as line 1 of every `<session_id>.jsonl` rollout file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutHeader {
    /// Rollout format version (`1`).
    pub format_version: u32,
    /// Unique session/thread identifier.
    pub session_id: String,
    /// Optional parent session id when created via fork or branch-preserving revert.
    pub parent_session_id: Option<String>,
    /// Optional turn id in `parent_session_id` where this fork branched.
    pub forked_from_turn_id: Option<String>,
    /// Canonical working directory.
    pub cwd: PathBuf,
    /// Initial `provider/model` spec.
    pub model: String,
    /// Creation timestamp (epoch seconds).
    pub created_at_epoch_secs: i64,
}

/// Append-only event record persisted inside `<session_id>.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum RolloutRecord {
    /// File header (always record 0).
    Header(RolloutHeader),
    /// Start of a logical user/agent turn.
    TurnStarted {
        /// Turn identifier.
        turn_id: String,
        /// Epoch seconds.
        started_at: i64,
    },
    /// Provider-level message appended to the conversation transcript.
    Message {
        /// Owning turn id.
        turn_id: String,
        /// Provider message (preserving `encrypted_content`, tool calls, and tool results).
        message: ProviderMessage,
    },
    /// Native structured tool output card for TUI/app-server replay without Lua.
    ToolCard {
        /// Owning turn id.
        turn_id: String,
        /// Tool call id.
        call_id: String,
        /// Tool name.
        tool_name: String,
        /// Structured native output.
        output: ToolOutput,
    },
    /// Pre-mutation workspace file snapshot reference bound to `turn_id`.
    SnapshotRecorded {
        /// Owning turn id.
        turn_id: String,
        /// Snapshot manifest id.
        snapshot_id: String,
    },
    /// Visible steering nudge or `/goal` update recorded in transcript.
    SteeringNote {
        /// Owning turn id.
        turn_id: String,
        /// Kind (`abandoned_turn`, `goal`, `mode_transition`, etc.).
        kind: String,
        /// Note content.
        text: String,
    },
    /// Turn completion marker with token usage.
    TurnCompleted {
        /// Turn identifier.
        turn_id: String,
        /// Status (`completed`, `interrupted`, `failed`).
        status: String,
        /// Turn token usage.
        usage: TokenUsage,
    },
}

/// Exclusive single-writer lease guard (`<state_dir>/leases/<session_id>.lock`).
#[derive(Debug)]
pub struct SessionLease {
    lock_path: PathBuf,
}

impl SessionLease {
    /// Acquires an exclusive writer lease for `session_id`, returning [`StorageError::SessionLocked`]
    /// if another active handle holds the lease.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the lease is already held or cannot be created.
    pub fn acquire(state_dir: &Path, session_id: &str) -> Result<Self, StorageError> {
        let leases_dir = state_dir.join("leases");
        fs::create_dir_all(&leases_dir).map_err(|source| StorageError::Io {
            path: leases_dir.clone(),
            source,
        })?;
        let lock_path = leases_dir.join(format!("{session_id}.lock"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(Self { lock_path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StorageError::SessionLocked {
                    session_id: session_id.to_string(),
                })
            }
            Err(source) => Err(StorageError::Io {
                path: lock_path,
                source,
            }),
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Append-only JSONL rollout log with automatic truncated-tail crash recovery.
#[derive(Debug)]
pub struct RolloutLog {
    path: PathBuf,
    file: File,
    _lease: SessionLease,
}

impl RolloutLog {
    /// Creates a brand-new rollout log with `header` and acquires its single-writer lease.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the session is locked or disk I/O fails.
    pub fn create(state_dir: &Path, header: &RolloutHeader) -> Result<Self, StorageError> {
        let lease = SessionLease::acquire(state_dir, &header.session_id)?;
        let rollouts_dir = state_dir.join("rollouts");
        fs::create_dir_all(&rollouts_dir).map_err(|source| StorageError::Io {
            path: rollouts_dir.clone(),
            source,
        })?;
        let path = rollouts_dir.join(format!("{}.jsonl", header.session_id));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;

        let mut log = Self {
            path,
            file,
            _lease: lease,
        };
        log.append(&RolloutRecord::Header(header.clone()))?;
        Ok(log)
    }

    /// Opens an existing rollout log, recovering cleanly from any crash-truncated final line,
    /// and returns `(RolloutLog, valid_records)`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the session is locked, missing, or corrupted mid-file.
    pub fn open_and_recover(
        state_dir: &Path,
        session_id: &str,
    ) -> Result<(Self, Vec<RolloutRecord>), StorageError> {
        let lease = SessionLease::acquire(state_dir, session_id)?;
        let path = rollout_path(state_dir, session_id);
        if !path.is_file() {
            return Err(StorageError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;

        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;

        let (records, valid_byte_len) = parse_recovering_tail(&path, &raw)?;
        if valid_byte_len < raw.len() {
            file.set_len(valid_byte_len as u64)
                .map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
            file.sync_all().map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;

        Ok((
            Self {
                path,
                file,
                _lease: lease,
            },
            records,
        ))
    }

    /// Reads all valid records from `session_id` without acquiring a write lease (for read-only inspection/fork).
    ///
    /// # Errors
    /// Returns [`StorageError`] if the rollout file does not exist or has mid-file corruption.
    pub fn read_records_readonly(
        state_dir: &Path,
        session_id: &str,
    ) -> Result<Vec<RolloutRecord>, StorageError> {
        let path = rollout_path(state_dir, session_id);
        if !path.is_file() {
            return Err(StorageError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        }
        let raw = fs::read(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let (records, _) = parse_recovering_tail(&path, &raw)?;
        Ok(records)
    }

    /// Appends a single JSONL record and flushes it to the OS buffer.
    ///
    /// # Errors
    /// Returns [`StorageError`] if serialization or file write fails.
    pub fn append(&mut self, record: &RolloutRecord) -> Result<(), StorageError> {
        let mut line = serde_json::to_vec(record).map_err(|e| StorageError::Json {
            message: e.to_string(),
        })?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .and_then(|()| self.file.flush())
            .map_err(|source| StorageError::Io {
                path: self.path.clone(),
                source,
            })
    }

    /// Rewrites this rollout log atomically with `records` while keeping the active lease held.
    ///
    /// # Errors
    /// Returns [`StorageError`] if writing fails.
    pub fn rewrite_records(&mut self, records: &[RolloutRecord]) -> Result<(), StorageError> {
        self.file.set_len(0).map_err(|source| StorageError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|source| StorageError::Io {
                path: self.path.clone(),
                source,
            })?;
        for rec in records {
            self.append(rec)?;
        }
        self.file.sync_all().map_err(|source| StorageError::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// Path to the underlying `.jsonl` rollout file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Computes `<state_dir>/rollouts/<session_id>.jsonl`.
#[must_use]
pub fn rollout_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("rollouts")
        .join(format!("{session_id}.jsonl"))
}

fn parse_recovering_tail(
    path: &Path,
    raw: &[u8],
) -> Result<(Vec<RolloutRecord>, usize), StorageError> {
    let mut records = Vec::new();
    let mut offset = 0usize;

    while offset < raw.len() {
        let rel_nl = raw[offset..].iter().position(|&b| b == b'\n');
        match rel_nl {
            Some(nl_idx) => {
                let line_end = offset + nl_idx;
                let slice = &raw[offset..line_end];
                let is_last_chunk = line_end + 1 == raw.len();
                if slice.iter().all(u8::is_ascii_whitespace) {
                    offset = line_end + 1;
                    continue;
                }
                match serde_json::from_slice::<RolloutRecord>(slice) {
                    Ok(rec) => {
                        records.push(rec);
                        offset = line_end + 1;
                    }
                    Err(_err) if is_last_chunk => {
                        // Trailing partial JSON line written right before crash: truncate back to `offset`.
                        break;
                    }
                    Err(err) => {
                        return Err(StorageError::CorruptRollout {
                            path: path.to_path_buf(),
                            message: err.to_string(),
                        });
                    }
                }
            }
            None => {
                // Unterminated final line without `\n`: recover by truncating back to `offset`.
                break;
            }
        }
    }

    Ok((records, offset))
}
