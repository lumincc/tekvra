//! Native structured tool output contracts shared across tools, storage, TUI, and app-server.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Per-file diff entry produced by `apply_patch`, `file_edit`, or `file_write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffEntry {
    /// Workspace-relative or canonical target path.
    pub path: PathBuf,
    /// Original path when moved/renamed.
    pub moved_from: Option<PathBuf>,
    /// Kind of file change.
    pub change_kind: FileChangeKind,
    /// Unified diff text for rendering and inspection.
    pub unified_diff: String,
    /// Full pre-mutation file text when retained (ACP structured `diff` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Full post-mutation file text when retained (ACP structured `diff` blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    /// Count of added lines.
    pub additions: u32,
    /// Count of deleted lines.
    pub deletions: u32,
}

/// Classification of a file mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// Newly created file.
    Add,
    /// Modified existing file.
    Update,
    /// Deleted file.
    Delete,
    /// Updated and moved file.
    Move,
}

/// Syntax-tree symbol outline entry produced by `file_index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolOutline {
    /// Symbol identifier.
    pub name: String,
    /// Kind of declaration (`fn`, `struct`, `impl`, `class`, `interface`, etc.).
    pub kind: String,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based inclusive end line.
    pub end_line: u32,
    /// Nested child declarations.
    #[serde(default)]
    pub children: Vec<SymbolOutline>,
}

/// Search match item produced by `file_grep` or `file_glob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Matched file path.
    pub path: PathBuf,
    /// Optional 1-based line number.
    pub line: Option<u32>,
    /// Optional matched line snippet.
    pub snippet: Option<String>,
}

/// Review finding produced by `/review` or `tekvra review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// Severity level (`critical`, `high`, `medium`, `low`).
    pub severity: String,
    /// Reviewed file path.
    pub path: PathBuf,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based end line.
    pub end_line: u32,
    /// Short finding title.
    pub title: String,
    /// Detailed explanation and concrete fix guidance.
    pub body: String,
}

/// Structured native Rust tool output rendered directly without Lua round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolOutput {
    /// Diff summary from `apply_patch`, `file_edit`, or `file_write`.
    Diff {
        /// Per-file diff entries.
        files: Vec<FileDiffEntry>,
        /// Snapshot identifier recorded prior to mutation.
        snapshot_id: Option<String>,
    },
    /// PTY or shell execution result with both filtered and retained raw output.
    Terminal {
        /// Executed command string.
        command: String,
        /// Active PTY session id if the process is still running or backgrounded.
        session_id: Option<String>,
        /// Exit code when terminated.
        exit_code: Option<i32>,
        /// Model-facing filtered output text.
        filtered_output: String,
        /// Retained unfiltered output (or inline preview) for TUI Raw/Filtered toggling.
        raw_output: String,
        /// Storage identifier for `tool_output` regex/line pagination.
        output_id: Option<String>,
        /// Whether noise reduction modified the raw output.
        was_filtered: bool,
    },
    /// Slice of a file returned by `file_read`.
    FileSlice {
        /// Read file path.
        path: PathBuf,
        /// 1-based start line.
        start_line: u32,
        /// 1-based end line.
        end_line: u32,
        /// Total lines in file.
        total_lines: u32,
        /// Line-numbered or raw slice content.
        content: String,
        /// Whether output was capped.
        truncated: bool,
    },
    /// Structural code outline returned by `file_index`.
    Outline {
        /// Indexed file path.
        path: PathBuf,
        /// Detected language name.
        language: String,
        /// Extracted top-level symbols.
        symbols: Vec<SymbolOutline>,
    },
    /// Workspace search results from `file_glob` or `file_grep`.
    Search {
        /// Collected matches.
        matches: Vec<SearchMatch>,
        /// Whether scan caps truncated traversal.
        truncated: bool,
        /// Count of credential/ignored files skipped for safety.
        skipped_sensitive_files: u32,
    },
    /// Updated session plan document.
    Plan {
        /// Markdown plan text.
        markdown: String,
    },
    /// Structured user question prompt and answers.
    Question {
        /// Prompted questions and selected answers.
        questions: Vec<(String, String)>,
    },
    /// Subagent task execution summary card.
    SubagentCard {
        /// Durable subagent task id.
        task_id: String,
        /// Assigned role or model label.
        role: String,
        /// Current lifecycle state (`running`, `completed`, `failed`).
        status: String,
        /// Latest summary text.
        summary: String,
    },
    /// Isolated Python `code` execution output.
    CodeExecution {
        /// Captured stdout/return summary.
        output: String,
        /// Number of authorized MCP tool calls bridged during execution.
        mcp_calls: u32,
    },
    /// Inspected workspace image delivered from `view_image` (multimodal-capable hosts).
    Image {
        /// Workspace-relative image path.
        path: PathBuf,
        /// Detected IANA media type (`image/png`, `image/jpeg`, `image/gif`, `image/webp`).
        media_type: String,
        /// Total image byte length.
        bytes_len: usize,
        /// Base64-encoded full image payload.
        data_base64: String,
    },
    /// Plain text or custom Lua plugin tool output.
    Text {
        /// Output body.
        text: String,
    },
}

impl ToolOutput {
    /// Formats the model-facing string representation placed in tool-result messages.
    #[must_use]
    pub fn to_model_text(&self) -> String {
        match self {
            Self::Diff { files, .. } => {
                let mut out = String::new();
                for f in files {
                    out.push_str(&format!(
                        "{:?} {} (+{} -{} lines)\n",
                        f.change_kind,
                        f.path.display(),
                        f.additions,
                        f.deletions
                    ));
                }
                out
            }
            Self::Terminal {
                filtered_output,
                exit_code,
                session_id,
                output_id,
                ..
            } => {
                let mut out = filtered_output.clone();
                if let Some(sid) = session_id {
                    out.push_str(&format!("\n[running session_id={sid}]"));
                }
                if let Some(code) = exit_code
                    && *code != 0
                {
                    out.push_str(&format!("\n[exit_code={code}]"));
                }
                if let Some(oid) = output_id {
                    out.push_str(&format!("\n[output_id={oid}]"));
                }
                out
            }
            Self::FileSlice { content, .. } => content.clone(),
            Self::Outline {
                path,
                language,
                symbols,
            } => {
                let mut out = format!("{} ({language}):\n", path.display());
                for sym in symbols {
                    out.push_str(&format!(
                        "  {} {} [L{}-L{}]\n",
                        sym.kind, sym.name, sym.start_line, sym.end_line
                    ));
                }
                out
            }
            Self::Search {
                matches,
                truncated,
                skipped_sensitive_files,
            } => {
                let mut out = String::new();
                for m in matches {
                    match (&m.line, &m.snippet) {
                        (Some(line), Some(snip)) => {
                            out.push_str(&format!("{}:{line}: {snip}\n", m.path.display()));
                        }
                        _ => out.push_str(&format!("{}\n", m.path.display())),
                    }
                }
                if *truncated || *skipped_sensitive_files > 0 {
                    out.push_str(&format!(
                        "[truncated={truncated}, skipped_sensitive_files={skipped_sensitive_files}]"
                    ));
                }
                out
            }
            Self::Plan { markdown } => markdown.clone(),
            Self::Question { questions } => questions
                .iter()
                .map(|(q, a)| format!("Q: {q}\nA: {a}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::SubagentCard {
                task_id,
                status,
                summary,
                ..
            } => format!("[task {task_id} ({status})] {summary}"),
            Self::CodeExecution { output, .. } => output.clone(),
            Self::Image {
                path,
                media_type,
                bytes_len,
                ..
            } => {
                format!(
                    "Viewed image {} ({media_type}, {bytes_len} bytes)",
                    path.display()
                )
            }
            Self::Text { text } => text.clone(),
        }
    }
}
