//! Content-addressed workspace file snapshot store, parent-linked session forking,
//! and atomic `conversation-only` / `files-only` / `conversation + files` revert.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tekvra_protocol::RevertScope;
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::StorageError;
use crate::index::{SessionSummary, StateIndex};
use crate::rollout::{RolloutHeader, RolloutLog, RolloutRecord, rollout_path};

const SNAPSHOT_EXCLUDED_NAMES: &[&str] = &[
    ".git", ".ssh", ".env", ".npmrc", ".pypirc", ".netrc", "target",
];

/// Pre-mutation state of a single workspace relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FilePreImage {
    /// File did not exist prior to the recorded mutation (must be deleted upon revert).
    Absent,
    /// Regular file existed with content stored at SHA-256 digest in `snapshots/blobs/`.
    Existed {
        /// Hex SHA-256 digest of original file bytes.
        sha256: String,
        /// Optional Unix permission mode (`0o644`, `0o755`, etc.).
        mode: Option<u32>,
    },
}

/// Persisted snapshot manifest recorded before mutating workspace files during a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Unique snapshot identifier (`snap_<uuidv7>`).
    pub snapshot_id: String,
    /// Owning session id.
    pub session_id: String,
    /// Owning turn id.
    pub turn_id: String,
    /// Monotonic sequence number within `session_id`.
    pub seq: u64,
    /// Canonical workspace root when captured.
    pub workspace_root: PathBuf,
    /// Workspace-relative POSIX path -> original pre-image state.
    pub entries: BTreeMap<String, FilePreImage>,
}

/// Summary returned after executing [`WorkspaceSnapshotStore::revert_session`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertOutcome {
    /// Restored/deleted workspace-relative paths.
    pub restored_paths: Vec<String>,
    /// Newly created archived session id holding the abandoned conversation branch (if conversation reverted).
    pub preserved_branch_session_id: Option<String>,
    /// Remaining record count in the active rollout log.
    pub remaining_records: usize,
}

/// Content-addressed workspace file snapshot manager under `<state_dir>/snapshots/`.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshotStore {
    state_dir: PathBuf,
}

impl WorkspaceSnapshotStore {
    /// Opens or creates the snapshot store rooted at `state_dir`.
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    fn blobs_dir(&self) -> PathBuf {
        self.state_dir.join("snapshots").join("blobs")
    }

    fn manifests_dir(&self, session_id: &str) -> PathBuf {
        self.state_dir
            .join("snapshots")
            .join("manifests")
            .join(session_id)
    }

    /// Captures pre-mutation state of `target_paths` (resolved relative to `workspace_root`)
    /// into content-addressed SHA-256 storage and writes a [`SnapshotManifest`].
    ///
    /// # Errors
    /// Returns [`StorageError`] if any path escapes `workspace_root` or disk I/O fails.
    pub fn capture_paths(
        &self,
        session_id: &str,
        turn_id: &str,
        workspace_root: &Path,
        target_paths: &[PathBuf],
    ) -> Result<SnapshotManifest, StorageError> {
        let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| StorageError::Io {
            path: workspace_root.to_path_buf(),
            source,
        })?;
        fs::create_dir_all(self.blobs_dir()).map_err(|source| StorageError::Io {
            path: self.blobs_dir(),
            source,
        })?;
        let manifest_dir = self.manifests_dir(session_id);
        fs::create_dir_all(&manifest_dir).map_err(|source| StorageError::Io {
            path: manifest_dir.clone(),
            source,
        })?;

        let existing_seq = self.list_manifests(session_id)?.len() as u64;
        let seq = existing_seq + 1;
        let snapshot_id = format!("snap_{seq:06}_{}", Uuid::now_v7().simple());

        let mut entries = BTreeMap::new();
        for raw_target in target_paths {
            let rel = normalize_workspace_rel(&canonical_ws, raw_target)?;
            if is_excluded_rel(&rel) {
                continue;
            }
            let abs = canonical_ws.join(&rel);
            let rel_key = rel.to_string_lossy().replace('\\', "/");
            if abs.is_file() {
                let bytes = fs::read(&abs).map_err(|source| StorageError::Io {
                    path: abs.clone(),
                    source,
                })?;
                let sha256 = self.write_blob(&bytes)?;
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    fs::metadata(&abs).ok().map(|m| m.permissions().mode())
                };
                #[cfg(not(unix))]
                let mode = None;
                entries.insert(rel_key, FilePreImage::Existed { sha256, mode });
            } else if !abs.exists() {
                entries.insert(rel_key, FilePreImage::Absent);
            }
        }

        let manifest = SnapshotManifest {
            snapshot_id: snapshot_id.clone(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            seq,
            workspace_root: canonical_ws,
            entries,
        };
        let manifest_path = manifest_dir.join(format!("{snapshot_id}.json"));
        let json = serde_json::to_vec_pretty(&manifest).map_err(|e| StorageError::Json {
            message: e.to_string(),
        })?;
        atomic_write_bytes(&manifest_path, &json)?;
        Ok(manifest)
    }

    /// Lists all recorded [`SnapshotManifest`]s for `session_id` in ascending `seq` order.
    ///
    /// # Errors
    /// Returns [`StorageError`] if reading or parsing manifests fails.
    pub fn list_manifests(&self, session_id: &str) -> Result<Vec<SnapshotManifest>, StorageError> {
        let dir = self.manifests_dir(session_id);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|source| StorageError::Io {
            path: dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| StorageError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read(&path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
            let m: SnapshotManifest =
                serde_json::from_slice(&raw).map_err(|e| StorageError::Json {
                    message: e.to_string(),
                })?;
            manifests.push(m);
        }
        manifests.sort_by_key(|m| m.seq);
        Ok(manifests)
    }

    /// Copies snapshot manifests up to (and including) `up_to_turn_id` from `parent_session_id`
    /// to `child_session_id` when forking a session (reusing content-addressed blobs without copying file bytes).
    ///
    /// # Errors
    /// Returns [`StorageError`] on I/O failure.
    pub fn inherit_snapshots_for_fork(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        allowed_turn_ids: &[String],
    ) -> Result<usize, StorageError> {
        let parent_manifests = self.list_manifests(parent_session_id)?;
        let child_dir = self.manifests_dir(child_session_id);
        fs::create_dir_all(&child_dir).map_err(|source| StorageError::Io {
            path: child_dir.clone(),
            source,
        })?;
        let mut count = 0usize;
        for mut m in parent_manifests {
            if allowed_turn_ids.iter().any(|t| t == &m.turn_id) {
                m.session_id = child_session_id.to_string();
                let out_path = child_dir.join(format!("{}.json", m.snapshot_id));
                let json = serde_json::to_vec_pretty(&m).map_err(|e| StorageError::Json {
                    message: e.to_string(),
                })?;
                atomic_write_bytes(&out_path, &json)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Forks `parent_session_id` after `up_to_turn_id` (or full session if `None`), writing a new
    /// parent-linked rollout file and SQLite summary row, and returns the new `child_session_id`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if `parent_session_id` or `up_to_turn_id` does not exist.
    pub fn fork_session(
        &self,
        index: &StateIndex,
        parent_session_id: &str,
        up_to_turn_id: Option<&str>,
        now_epoch_secs: i64,
    ) -> Result<String, StorageError> {
        let parent_records = RolloutLog::read_records_readonly(&self.state_dir, parent_session_id)?;
        let Some(RolloutRecord::Header(parent_hdr)) = parent_records.first().cloned() else {
            return Err(StorageError::CorruptRollout {
                path: rollout_path(&self.state_dir, parent_session_id),
                message: "missing rollout header".to_string(),
            });
        };

        let sliced = slice_records_through_turn(&parent_records, up_to_turn_id)?;
        let child_id = format!("thr_{}", Uuid::now_v7().simple());
        let child_hdr = RolloutHeader {
            format_version: 1,
            session_id: child_id.clone(),
            parent_session_id: Some(parent_session_id.to_string()),
            forked_from_turn_id: up_to_turn_id.map(str::to_string),
            cwd: parent_hdr.cwd.clone(),
            model: parent_hdr.model.clone(),
            created_at_epoch_secs: now_epoch_secs,
        };

        let mut child_log = RolloutLog::create(&self.state_dir, &child_hdr)?;
        let mut kept_turns = Vec::new();
        for rec in sliced.into_iter().skip(1) {
            if let RolloutRecord::TurnStarted { turn_id, .. } = &rec {
                kept_turns.push(turn_id.clone());
            }
            child_log.append(&rec)?;
        }
        drop(child_log);

        self.inherit_snapshots_for_fork(parent_session_id, &child_id, &kept_turns)?;

        let parent_summary = index.get_session(parent_session_id)?;
        index.upsert_session(&SessionSummary {
            id: child_id.clone(),
            parent_id: Some(parent_session_id.to_string()),
            forked_from_turn_id: up_to_turn_id.map(str::to_string),
            cwd: parent_hdr.cwd,
            project_id: parent_summary
                .as_ref()
                .map(|p| p.project_id.clone())
                .unwrap_or_else(|| "default".to_string()),
            model: parent_hdr.model,
            title: parent_summary
                .as_ref()
                .and_then(|p| p.title.as_ref().map(|t| format!("{t} (fork)"))),
            preview: parent_summary
                .map(|p| p.preview)
                .unwrap_or_else(|| "Forked session".to_string()),
            archived: false,
            created_at: now_epoch_secs,
            updated_at: now_epoch_secs,
        })?;

        Ok(child_id)
    }

    /// Reverts `rollout` (an active leased [`RolloutLog`]) to the state **before** `from_turn_id`
    /// (undoing `from_turn_id` and every subsequent turn) according to `scope`:
    /// - `FilesOnly`: restores workspace files without altering conversation history.
    /// - `ConversationOnly`: preserves the abandoned branch as an archived parent-linked fork and rewinds `rollout`.
    /// - `ConversationAndFiles`: atomically restores workspace files AND rewinds `rollout` (preserving the abandoned branch).
    ///
    /// # Errors
    /// Returns [`StorageError`] if `from_turn_id` is not found or snapshot blob integrity fails.
    pub fn revert_from_turn(
        &self,
        index: &StateIndex,
        rollout: &mut RolloutLog,
        current_records: &[RolloutRecord],
        from_turn_id: &str,
        scope: RevertScope,
        now_epoch_secs: i64,
    ) -> Result<RevertOutcome, StorageError> {
        let Some(RolloutRecord::Header(hdr)) = current_records.first().cloned() else {
            return Err(StorageError::CorruptRollout {
                path: rollout.path().to_path_buf(),
                message: "missing rollout header".to_string(),
            });
        };

        // Partition turns into `kept_before` and `reverted_turns` starting at `from_turn_id`.
        let split_idx = current_records
            .iter()
            .position(
                |r| matches!(r, RolloutRecord::TurnStarted { turn_id, .. } if turn_id == from_turn_id),
            )
            .ok_or_else(|| StorageError::TurnNotFound {
                turn_id: from_turn_id.to_string(),
            })?;

        let mut reverted_turn_ids = Vec::new();
        for rec in &current_records[split_idx..] {
            if let RolloutRecord::TurnStarted { turn_id, .. } = rec {
                reverted_turn_ids.push(turn_id.clone());
            }
        }

        let mut restored_paths = Vec::new();
        if matches!(
            scope,
            RevertScope::FilesOnly | RevertScope::ConversationAndFiles
        ) {
            let manifests = self.list_manifests(&hdr.session_id)?;
            // Walk manifests belonging to reverted turns in REVERSE seq order so the earliest
            // pre-image for each touched path wins!
            let mut target_preimages: BTreeMap<String, (&Path, &FilePreImage)> = BTreeMap::new();
            for m in manifests
                .iter()
                .rev()
                .filter(|m| reverted_turn_ids.iter().any(|t| t == &m.turn_id))
            {
                for (rel_path, pre) in &m.entries {
                    target_preimages.insert(rel_path.clone(), (&m.workspace_root, pre));
                }
            }

            struct StagedRestore {
                dest: PathBuf,
                payload: Option<(Vec<u8>, Option<u32>)>,
            }

            // Verify all required blobs exist and pass SHA-256 integrity BEFORE mutating any workspace file!
            let mut staged_bytes: BTreeMap<String, StagedRestore> = BTreeMap::new();
            for (rel_path, (ws_root, pre)) in target_preimages {
                let dest = ws_root.join(&rel_path);
                match pre {
                    FilePreImage::Absent => {
                        staged_bytes.insert(
                            rel_path,
                            StagedRestore {
                                dest,
                                payload: None,
                            },
                        );
                    }
                    FilePreImage::Existed { sha256, mode } => {
                        let data = self.read_verified_blob(sha256)?;
                        staged_bytes.insert(
                            rel_path,
                            StagedRestore {
                                dest,
                                payload: Some((data, *mode)),
                            },
                        );
                    }
                }
            }

            // Apply all restorations atomically per file
            for (rel_path, StagedRestore { dest, payload }) in staged_bytes {
                match payload {
                    None => {
                        if dest.is_file() || dest.is_symlink() {
                            fs::remove_file(&dest).map_err(|source| StorageError::Io {
                                path: dest.clone(),
                                source,
                            })?;
                        }
                        cleanup_empty_parents(dest.parent(), &hdr.cwd);
                    }
                    Some((bytes, mode)) => {
                        atomic_write_bytes(&dest, &bytes)?;
                        #[cfg(unix)]
                        if let Some(m) = mode {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(m));
                        }
                        #[cfg(not(unix))]
                        let _ = mode;
                    }
                }
                restored_paths.push(rel_path);
            }
        }

        let mut preserved_branch_session_id = None;
        let remaining_records = if matches!(
            scope,
            RevertScope::ConversationOnly | RevertScope::ConversationAndFiles
        ) {
            // Save the full pre-revert transcript as an archived parent-linked branch so nothing is ever lost!
            let branch_id = format!("thr_abandoned_{}", Uuid::now_v7().simple());
            let mut branch_hdr = hdr.clone();
            branch_hdr.session_id.clone_from(&branch_id);
            branch_hdr.parent_session_id = Some(hdr.session_id.clone());
            branch_hdr.forked_from_turn_id = Some(from_turn_id.to_string());
            branch_hdr.created_at_epoch_secs = now_epoch_secs;

            let mut branch_log = RolloutLog::create(&self.state_dir, &branch_hdr)?;
            for rec in current_records.iter().skip(1) {
                branch_log.append(rec)?;
            }
            drop(branch_log);

            index.upsert_session(&SessionSummary {
                id: branch_id.clone(),
                parent_id: Some(hdr.session_id.clone()),
                forked_from_turn_id: Some(from_turn_id.to_string()),
                cwd: hdr.cwd.clone(),
                project_id: "abandoned-branch".to_string(),
                model: hdr.model.clone(),
                title: Some(format!("Reverted branch from {from_turn_id}")),
                preview: "Preserved pre-revert branch".to_string(),
                archived: true,
                created_at: now_epoch_secs,
                updated_at: now_epoch_secs,
            })?;
            preserved_branch_session_id = Some(branch_id);

            let kept = &current_records[..split_idx];
            rollout.rewrite_records(kept)?;
            kept.len()
        } else {
            current_records.len()
        };

        Ok(RevertOutcome {
            restored_paths,
            preserved_branch_session_id,
            remaining_records,
        })
    }

    fn blob_path(&self, sha256: &str) -> PathBuf {
        let prefix = &sha256[..2.min(sha256.len())];
        self.blobs_dir().join(prefix).join(sha256)
    }

    /// Hex-encodes a SHA-256 digest as 64 lowercase hex characters.
    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn write_blob(&self, bytes: &[u8]) -> Result<String, StorageError> {
        let hex = Self::sha256_hex(bytes);
        let path = self.blob_path(&hex);
        if !path.is_file() {
            atomic_write_bytes(&path, bytes)?;
        }
        Ok(hex)
    }

    fn read_verified_blob(&self, expected_sha256: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.blob_path(expected_sha256);
        let bytes = fs::read(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let actual = Self::sha256_hex(&bytes);
        if actual != expected_sha256 {
            return Err(StorageError::SnapshotIntegrity {
                expected: expected_sha256.to_string(),
                actual,
            });
        }
        Ok(bytes)
    }
}

fn slice_records_through_turn(
    records: &[RolloutRecord],
    up_to_turn_id: Option<&str>,
) -> Result<Vec<RolloutRecord>, StorageError> {
    let Some(target_turn) = up_to_turn_id else {
        return Ok(records.to_vec());
    };
    let mut out = Vec::new();
    let mut seen_target = false;
    let mut past_target = false;

    for rec in records {
        if let RolloutRecord::TurnStarted { turn_id, .. } = rec {
            if seen_target {
                past_target = true;
            }
            if turn_id == target_turn {
                seen_target = true;
            }
        }
        if past_target {
            break;
        }
        out.push(rec.clone());
    }

    if !seen_target {
        return Err(StorageError::TurnNotFound {
            turn_id: target_turn.to_string(),
        });
    }
    Ok(out)
}

fn normalize_workspace_rel(
    workspace_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, StorageError> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    };

    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(seg) => normalized.push(seg),
        }
    }

    let rel = normalized.strip_prefix(workspace_root).map_err(|_| {
        StorageError::PathOutsideWorkspace {
            path: candidate.to_path_buf(),
        }
    })?;
    Ok(rel.to_path_buf())
}

fn is_excluded_rel(rel: &Path) -> bool {
    rel.components().any(|c| match c {
        Component::Normal(s) => {
            let name = s.to_string_lossy();
            SNAPSHOT_EXCLUDED_NAMES.iter().any(|ex| name == *ex) || name.starts_with(".env.")
        }
        _ => false,
    })
}

fn cleanup_empty_parents(mut dir: Option<&Path>, stop_at: &Path) {
    while let Some(d) = dir {
        if d == stop_at || !d.starts_with(stop_at) {
            break;
        }
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn atomic_write_bytes(target: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| StorageError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| StorageError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(bytes).map_err(|source| StorageError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    tmp.persist(target).map_err(|e| StorageError::Io {
        path: target.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}
