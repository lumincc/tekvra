//! SQLite (`rusqlite` bundled + WAL + FTS5) session index, full-text search, usage ledger,
//! persisted permission grants, and retained raw tool-output paging.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tekvra_protocol::{ApprovalScope, GrantLifetime, TokenUsage};
use uuid::Uuid;

use crate::StorageError;

/// Summary row stored in SQLite `sessions` for fast pagination (`thread/list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session/thread identifier.
    pub id: String,
    /// Optional parent session id (forks and preserved revert branches).
    pub parent_id: Option<String>,
    /// Optional turn id in `parent_id` where forked.
    pub forked_from_turn_id: Option<String>,
    /// Canonical working directory.
    pub cwd: PathBuf,
    /// Project hash identifier.
    pub project_id: String,
    /// Active `provider/model` spec.
    pub model: String,
    /// Optional user-assigned session title.
    pub title: Option<String>,
    /// Short preview of first/latest prompt.
    pub preview: String,
    /// Whether archived.
    pub archived: bool,
    /// Created epoch seconds.
    pub created_at: i64,
    /// Last updated epoch seconds.
    pub updated_at: i64,
}

/// Full-text search hit returned by FTS5 `search_history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySearchHit {
    /// Matching session id.
    pub session_id: String,
    /// Matching turn id.
    pub turn_id: String,
    /// Message role (`user`, `assistant`, `tool`).
    pub role: String,
    /// Highlighted or extracted text snippet.
    pub snippet: String,
}

/// Persisted permission grant loaded from `state.db`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPermissionGrant {
    /// Unique grant id.
    pub id: String,
    /// Canonical project path when `lifetime == Project`, or `None` for `Global`.
    pub project_root: Option<PathBuf>,
    /// `Project` or `Global`.
    pub lifetime: GrantLifetime,
    /// Typed scope granted.
    pub scope: ApprovalScope,
}

/// Retained raw command output slice returned by `tool_output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedOutputPage {
    /// Output identifier.
    pub output_id: String,
    /// Total lines in retained raw output.
    pub total_lines: usize,
    /// 1-based start line returned (when line paging).
    pub start_line: usize,
    /// 1-based end line returned.
    pub end_line: usize,
    /// Selected or regex-matched lines.
    pub lines: Vec<String>,
}

/// Grouping key for [`StateIndex::usage_aggregate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGroupBy {
    /// One row per `usage_ledger.model`.
    Model,
    /// One row per local calendar day (`date(recorded_at, 'unixepoch', 'localtime')`).
    Day,
    /// One row per `usage_ledger.session_id`.
    Session,
}

/// One aggregated usage row: totals per group key.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    /// Group key (model name / local date / session id).
    pub key: String,
    /// Uncached + cached input tokens.
    pub input_tokens: u64,
    /// Tokens served from prompt cache.
    pub cached_input_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens included in output.
    pub reasoning_tokens: u64,
    /// Sum of `cost_usd`.
    pub cost_usd: f64,
}

/// One full per-turn ledger row (for CSV export).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecordRow {
    /// Owning session id.
    pub session_id: String,
    /// Turn id within the session.
    pub turn_id: String,
    /// Serving model spec.
    pub model: String,
    /// Uncached + cached input tokens.
    pub input_tokens: u64,
    /// Tokens served from prompt cache.
    pub cached_input_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens included in output.
    pub reasoning_tokens: u64,
    /// Turn cost in USD.
    pub cost_usd: f64,
    /// Unix-seconds write timestamp.
    pub recorded_at: i64,
}

/// Current unix time in seconds (wall clock; single source for window math).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// SQLite (`WAL` + `FTS5`) index backing fast session queries, search, ledgers, and raw output logs.
pub struct StateIndex {
    state_dir: PathBuf,
    conn: Connection,
}

impl std::fmt::Debug for StateIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateIndex")
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl StateIndex {
    /// Runs SQLite `PRAGMA integrity_check` against `state.db`.
    ///
    /// # Errors
    /// Returns [`StorageError`] when the pragma reports corruption or the query fails.
    pub fn integrity_check(&self) -> Result<String, StorageError> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        if result == "ok" {
            Ok("integrity_check: ok".to_string())
        } else {
            Err(StorageError::Sqlite {
                message: format!("integrity_check: {result}"),
            })
        }
    }

    /// Opens or initializes `<state_dir>/state.db` in WAL mode with FTS5 enabled.
    ///
    /// # Errors
    /// Returns [`StorageError`] if directory creation or SQLite schema initialization fails.
    pub fn open(state_dir: &Path) -> Result<Self, StorageError> {
        fs::create_dir_all(state_dir).map_err(|source| StorageError::Io {
            path: state_dir.to_path_buf(),
            source,
        })?;
        let db_path = state_dir.join("state.db");
        let conn = Connection::open(&db_path).map_err(|e| StorageError::Sqlite {
            message: e.to_string(),
        })?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                forked_from_turn_id TEXT,
                cwd TEXT NOT NULL,
                project_id TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                preview TEXT NOT NULL DEFAULT '',
                archived INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_sessions_updated
                ON sessions(archived, updated_at DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
                session_id UNINDEXED,
                turn_id UNINDEXED,
                role UNINDEXED,
                content
            );

            CREATE TABLE IF NOT EXISTS usage_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                cached_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                reasoning_tokens INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                recorded_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_usage_recorded
                ON usage_ledger(recorded_at);

            CREATE TABLE IF NOT EXISTS permission_grants (
                id TEXT PRIMARY KEY,
                project_root TEXT,
                lifetime TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS retained_outputs (
                output_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                command TEXT NOT NULL,
                raw_path TEXT NOT NULL,
                line_count INTEGER NOT NULL,
                byte_len INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            ",
        )
        .map_err(|e| StorageError::Sqlite {
            message: e.to_string(),
        })?;

        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            conn,
        })
    }

    /// Inserts or updates a session summary record.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn upsert_session(&self, s: &SessionSummary) -> Result<(), StorageError> {
        self.conn
            .execute(
                "
                INSERT INTO sessions (
                    id, parent_id, forked_from_turn_id, cwd, project_id, model,
                    title, preview, archived, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(id) DO UPDATE SET
                    model = excluded.model,
                    title = COALESCE(excluded.title, sessions.title),
                    preview = CASE WHEN excluded.preview != '' THEN excluded.preview ELSE sessions.preview END,
                    archived = excluded.archived,
                    updated_at = excluded.updated_at
                ",
                params![
                    s.id,
                    s.parent_id,
                    s.forked_from_turn_id,
                    s.cwd.to_string_lossy().as_ref(),
                    s.project_id,
                    s.model,
                    s.title,
                    s.preview,
                    i32::from(s.archived),
                    s.created_at,
                    s.updated_at,
                ],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(())
    }

    /// Fetches a single session summary by id.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionSummary>, StorageError> {
        self.conn
            .query_row(
                "SELECT id, parent_id, forked_from_turn_id, cwd, project_id, model, title, preview, archived, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![session_id],
                row_to_session_summary,
            )
            .optional()
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })
    }

    /// Lists sessions ordered by `updated_at DESC` with `limit` and `offset` pagination.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn list_sessions(
        &self,
        include_archived: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let sql = if include_archived {
            "SELECT id, parent_id, forked_from_turn_id, cwd, project_id, model, title, preview, archived, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC, id DESC LIMIT ?1 OFFSET ?2"
        } else {
            "SELECT id, parent_id, forked_from_turn_id, cwd, project_id, model, title, preview, archived, created_at, updated_at
             FROM sessions WHERE archived = 0 ORDER BY updated_at DESC, id DESC LIMIT ?1 OFFSET ?2"
        };
        let mut stmt = self.conn.prepare(sql).map_err(|e| StorageError::Sqlite {
            message: e.to_string(),
        })?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], row_to_session_summary)
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?);
        }
        Ok(out)
    }

    /// Updates archive state for `session_id`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn set_archived(&self, session_id: &str, archived: bool) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "UPDATE sessions SET archived = ?2 WHERE id = ?1",
                params![session_id, i32::from(archived)],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(changed > 0)
    }

    /// Updates user-visible title for `session_id`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn set_session_title(&self, session_id: &str, title: &str) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                params![session_id, title],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(changed > 0)
    }

    /// Deletes a session from `sessions` and `history_fts` (and removes its `.jsonl` rollout if present).
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, StorageError> {
        let _ = self.conn.execute(
            "DELETE FROM history_fts WHERE session_id = ?1",
            params![session_id],
        );
        let removed = self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        let rollout = crate::rollout::rollout_path(&self.state_dir, session_id);
        if rollout.exists() {
            let _ = fs::remove_file(rollout);
        }
        Ok(removed > 0)
    }

    /// Indexes searchable message text in FTS5.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn index_message_text(
        &self,
        session_id: &str,
        turn_id: &str,
        role: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        if content.trim().is_empty() {
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO history_fts (session_id, turn_id, role, content) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, turn_id, role, content],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(())
    }

    /// Searches conversation history via SQLite FTS5 (`MATCH`), ordered by BM25 rank.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the FTS5 query fails.
    pub fn search_history(
        &self,
        fts_query: &str,
        limit: usize,
    ) -> Result<Vec<HistorySearchHit>, StorageError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, turn_id, role, snippet(history_fts, 3, '[', ']', '...', 16)
                 FROM history_fts
                 WHERE history_fts MATCH ?1
                 ORDER BY bm25(history_fts)
                 LIMIT ?2",
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        let rows = stmt
            .query_map(params![fts_query, limit as i64], |row| {
                Ok(HistorySearchHit {
                    session_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    role: row.get(2)?,
                    snippet: row.get(3)?,
                })
            })
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        let mut hits = Vec::new();
        for r in rows {
            hits.push(r.map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?);
        }
        Ok(hits)
    }

    /// Appends a turn's token/cost usage to `usage_ledger` and returns cumulative usage for `session_id`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn record_usage(
        &self,
        session_id: &str,
        turn_id: &str,
        model: &str,
        usage: &TokenUsage,
        recorded_at: i64,
    ) -> Result<TokenUsage, StorageError> {
        self.conn
            .execute(
                "INSERT INTO usage_ledger (
                    session_id, turn_id, model, input_tokens, cached_input_tokens,
                    output_tokens, reasoning_tokens, cost_usd, recorded_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    turn_id,
                    model,
                    usage.input_tokens as i64,
                    usage.cached_input_tokens as i64,
                    usage.output_tokens as i64,
                    usage.reasoning_tokens as i64,
                    usage.cost_usd,
                    recorded_at,
                ],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        self.session_usage_totals(session_id)
    }

    /// Queries cumulative [`TokenUsage`] for `session_id`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn session_usage_totals(&self, session_id: &str) -> Result<TokenUsage, StorageError> {
        self.conn
            .query_row(
                "SELECT
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(reasoning_tokens), 0),
                    COALESCE(SUM(cost_usd), 0.0)
                 FROM usage_ledger WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(TokenUsage {
                        input_tokens: row.get::<_, i64>(0)? as u64,
                        cached_input_tokens: row.get::<_, i64>(1)? as u64,
                        output_tokens: row.get::<_, i64>(2)? as u64,
                        reasoning_tokens: row.get::<_, i64>(3)? as u64,
                        cost_usd: row.get::<_, f64>(4)?,
                    })
                },
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })
    }

    /// Queries cumulative [`TokenUsage`] recorded since `since_epoch` (unix seconds).
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn usage_totals_since(&self, since_epoch: i64) -> Result<TokenUsage, StorageError> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(cached_input_tokens),0),
                        COALESCE(SUM(output_tokens),0), COALESCE(SUM(reasoning_tokens),0),
                        COALESCE(SUM(cost_usd),0.0)
                 FROM usage_ledger WHERE recorded_at >= ?1",
                params![since_epoch],
                |row| {
                    Ok(TokenUsage {
                        input_tokens: row.get::<_, i64>(0)? as u64,
                        cached_input_tokens: row.get::<_, i64>(1)? as u64,
                        output_tokens: row.get::<_, i64>(2)? as u64,
                        reasoning_tokens: row.get::<_, i64>(3)? as u64,
                        cost_usd: row.get::<_, f64>(4)?,
                    })
                },
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })
    }

    /// Aggregates [`usage_ledger`] rows over an optional window, grouped by
    /// model / local day / session.
    ///
    /// `None` window means the whole ledger. Rows are ordered by descending cost.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn usage_aggregate(
        &self,
        window_days: Option<u32>,
        group_by: UsageGroupBy,
    ) -> Result<Vec<UsageRow>, StorageError> {
        let group_expr = match group_by {
            UsageGroupBy::Model => "model",
            UsageGroupBy::Day => "date(recorded_at, 'unixepoch', 'localtime')",
            UsageGroupBy::Session => "session_id",
        };
        let sql = format!(
            "SELECT {group_expr} AS key,
                    COALESCE(SUM(input_tokens),0)   AS input_tokens,
                    COALESCE(SUM(cached_input_tokens),0) AS cached_input_tokens,
                    COALESCE(SUM(output_tokens),0)  AS output_tokens,
                    COALESCE(SUM(reasoning_tokens),0) AS reasoning_tokens,
                    COALESCE(SUM(cost_usd),0.0)     AS cost_usd
             FROM usage_ledger {where_clause}
             GROUP BY key
             ORDER BY cost_usd DESC, key ASC",
            where_clause = match window_days {
                Some(days) => format!(
                    "WHERE recorded_at >= {}",
                    now_epoch().saturating_sub(i64::from(days) * 86_400)
                ),
                None => String::new(),
            },
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| StorageError::Sqlite {
            message: e.to_string(),
        })?;
        let rows = stmt
            .query_map([], |row| {
                Ok(UsageRow {
                    key: row.get::<_, String>(0)?,
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    cached_input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    reasoning_tokens: row.get::<_, i64>(4)? as u64,
                    cost_usd: row.get::<_, f64>(5)?,
                })
            })
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(rows)
    }

    /// Returns full per-turn ledger rows (newest first) within an optional window.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite failure.
    pub fn usage_rows_since(
        &self,
        window_days: Option<u32>,
    ) -> Result<Vec<UsageRecordRow>, StorageError> {
        let sql = format!(
            "SELECT session_id, turn_id, model, input_tokens, cached_input_tokens,
                    output_tokens, reasoning_tokens, cost_usd, recorded_at
             FROM usage_ledger {where_clause}
             ORDER BY recorded_at DESC, id DESC",
            where_clause = match window_days {
                Some(days) => format!(
                    "WHERE recorded_at >= {}",
                    now_epoch().saturating_sub(i64::from(days) * 86_400)
                ),
                None => String::new(),
            },
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| StorageError::Sqlite {
            message: e.to_string(),
        })?;
        let rows = stmt
            .query_map([], |row| {
                Ok(UsageRecordRow {
                    session_id: row.get(0)?,
                    turn_id: row.get(1)?,
                    model: row.get(2)?,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    cached_input_tokens: row.get::<_, i64>(4)? as u64,
                    output_tokens: row.get::<_, i64>(5)? as u64,
                    reasoning_tokens: row.get::<_, i64>(6)? as u64,
                    cost_usd: row.get(7)?,
                    recorded_at: row.get(8)?,
                })
            })
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(rows)
    }

    /// Persists a `Project` or `Global` permission grant.
    ///
    /// # Errors
    /// Returns [`StorageError`] on serialization or SQLite failure.
    pub fn save_permission_grant(
        &self,
        project_root: Option<&Path>,
        lifetime: GrantLifetime,
        scope: &ApprovalScope,
        created_at: i64,
    ) -> Result<String, StorageError> {
        let id = format!("grant_{}", Uuid::now_v7().simple());
        let scope_json = serde_json::to_string(scope).map_err(|e| StorageError::Json {
            message: e.to_string(),
        })?;
        let lifetime_str = match lifetime {
            GrantLifetime::Global => "global",
            GrantLifetime::Project => "project",
            GrantLifetime::Conversation | GrantLifetime::Once => "ephemeral",
        };
        self.conn
            .execute(
                "INSERT INTO permission_grants (id, project_root, lifetime, scope_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    project_root.map(|p| p.to_string_lossy().into_owned()),
                    lifetime_str,
                    scope_json,
                    created_at,
                ],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(id)
    }

    /// Loads persisted `Global` grants plus `Project` grants matching `project_root`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on SQLite or JSON failure.
    pub fn load_permission_grants(
        &self,
        project_root: &Path,
    ) -> Result<Vec<StoredPermissionGrant>, StorageError> {
        let proj_str = project_root.to_string_lossy().into_owned();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project_root, lifetime, scope_json
                 FROM permission_grants
                 WHERE lifetime = 'global' OR (lifetime = 'project' AND project_root = ?1)
                 ORDER BY created_at ASC",
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        let rows = stmt
            .query_map(params![proj_str], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;

        let mut out = Vec::new();
        for r in rows {
            let (id, root_opt, lt_str, scope_json) = r.map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
            let scope: ApprovalScope =
                serde_json::from_str(&scope_json).map_err(|e| StorageError::Json {
                    message: e.to_string(),
                })?;
            let lifetime = if lt_str == "global" {
                GrantLifetime::Global
            } else {
                GrantLifetime::Project
            };
            out.push(StoredPermissionGrant {
                id,
                project_root: root_opt.map(PathBuf::from),
                lifetime,
                scope,
            });
        }
        Ok(out)
    }

    /// Stores raw unfiltered shell output on disk and indexes it in `retained_outputs` for `tool_output`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on disk or SQLite failure.
    pub fn store_retained_output(
        &self,
        session_id: &str,
        command: &str,
        raw_output: &str,
        created_at: i64,
    ) -> Result<String, StorageError> {
        let out_dir = self.state_dir.join("outputs");
        fs::create_dir_all(&out_dir).map_err(|source| StorageError::Io {
            path: out_dir.clone(),
            source,
        })?;
        let output_id = format!("out_{}", Uuid::now_v7().simple());
        let raw_path = out_dir.join(format!("{output_id}.log"));
        fs::write(&raw_path, raw_output.as_bytes()).map_err(|source| StorageError::Io {
            path: raw_path.clone(),
            source,
        })?;
        let line_count = raw_output.lines().count();
        self.conn
            .execute(
                "INSERT INTO retained_outputs (output_id, session_id, command, raw_path, line_count, byte_len, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    output_id,
                    session_id,
                    command,
                    raw_path.to_string_lossy().as_ref(),
                    line_count as i64,
                    raw_output.len() as i64,
                    created_at,
                ],
            )
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?;
        Ok(output_id)
    }

    /// Pages or regex-searches a retained raw tool output log (`tool_output` native tool).
    ///
    /// # Errors
    /// Returns [`StorageError`] if `output_id` is unknown, `pattern` is an invalid regex, or I/O fails.
    pub fn query_retained_output(
        &self,
        output_id: &str,
        offset_line: usize,
        limit_lines: usize,
        regex_pattern: Option<&str>,
    ) -> Result<RetainedOutputPage, StorageError> {
        let raw_path_str: String = self
            .conn
            .query_row(
                "SELECT raw_path FROM retained_outputs WHERE output_id = ?1",
                params![output_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| StorageError::Sqlite {
                message: e.to_string(),
            })?
            .ok_or_else(|| StorageError::OutputNotFound {
                output_id: output_id.to_string(),
            })?;

        let raw_path = PathBuf::from(raw_path_str);
        let raw = fs::read_to_string(&raw_path).map_err(|source| StorageError::Io {
            path: raw_path,
            source,
        })?;
        let all_lines: Vec<&str> = raw.lines().collect();
        let total_lines = all_lines.len();

        if let Some(pat) = regex_pattern {
            let re = Regex::new(pat).map_err(|e| StorageError::InvalidQuery {
                message: format!("invalid regex `{pat}`: {e}"),
            })?;
            let matched: Vec<String> = all_lines
                .iter()
                .enumerate()
                .filter(|(_, line)| re.is_match(line))
                .skip(offset_line.saturating_sub(1))
                .take(limit_lines.max(1))
                .map(|(idx, line)| format!("{}: {line}", idx + 1))
                .collect();
            return Ok(RetainedOutputPage {
                output_id: output_id.to_string(),
                total_lines,
                start_line: 1,
                end_line: total_lines,
                lines: matched,
            });
        }

        let start_idx = offset_line.saturating_sub(1).min(total_lines);
        let end_idx = (start_idx + limit_lines.max(1)).min(total_lines);
        let slice: Vec<String> = all_lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}: {l}", start_idx + i + 1))
            .collect();

        Ok(RetainedOutputPage {
            output_id: output_id.to_string(),
            total_lines,
            start_line: if slice.is_empty() { 0 } else { start_idx + 1 },
            end_line: end_idx,
            lines: slice,
        })
    }
}

fn row_to_session_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        forked_from_turn_id: row.get(2)?,
        cwd: PathBuf::from(row.get::<_, String>(3)?),
        project_id: row.get(4)?,
        model: row.get(5)?,
        title: row.get(6)?,
        preview: row.get(7)?,
        archived: row.get::<_, i32>(8)? != 0,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}
