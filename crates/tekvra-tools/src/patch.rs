//! Apache-2.0 port of Workcell's `patch.rs` (`*** Begin Patch` grammar and whitespace-tolerant
//! context seeker), integrated with per-path async locks, pre-mutation `tekvra-storage` snapshots,
//! and structured unified diff generation.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use similar::TextDiff;
use tekvra_protocol::{FileChangeKind, FileDiffEntry, ToolOutput};
use tekvra_storage::WorkspaceSnapshotStore;
use tempfile::NamedTempFile;

use crate::ToolsError;
use crate::fs_tools::WorkspacePathLocks;

/// Parsed file-level hunk inside an `apply_patch` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchHunk {
    /// `*** Add File: <path>`
    Add {
        /// Workspace-relative file path.
        path: String,
        /// New file contents.
        contents: String,
    },
    /// `*** Delete File: <path>`
    Delete {
        /// Workspace-relative file path.
        path: String,
    },
    /// `*** Update File: <path>` with optional `*** Move to: <dest>`.
    Update {
        /// Source workspace-relative file path.
        path: String,
        /// Optional destination path when moving/renaming.
        move_path: Option<String>,
        /// Ordered diff chunks.
        chunks: Vec<UpdateChunk>,
    },
}

/// Matched diff span within an updated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    /// 0-based start line in original file.
    pub old_start: usize,
    /// Matched line count in original file.
    pub old_len: usize,
    /// 0-based start line in updated file.
    pub new_start: usize,
    /// Replacement line count in updated file.
    pub new_len: usize,
}

/// Single `@@` update chunk inside `PatchHunk::Update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    context: Option<String>,
    end_of_file: bool,
}

/// Parses a `*** Begin Patch` ... `*** End Patch` document (unwrapping optional shell heredocs).
///
/// # Errors
/// Returns [`ToolsError::InvalidInput`] if markers, headers, or chunk prefixes are malformed.
pub fn parse_patch(patch_text: &str) -> Result<Vec<PatchHunk>, ToolsError> {
    let trimmed = patch_text.trim();
    let unwrapped = strip_heredoc(trimmed).unwrap_or(trimmed);
    let normalized = unwrapped.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    let begin = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch");
    let end = lines.iter().position(|line| line.trim() == "*** End Patch");
    let (Some(begin), Some(end)) = (begin, end) else {
        return Err(ToolsError::invalid(
            "Invalid patch format: missing `*** Begin Patch` / `*** End Patch` markers",
        ));
    };
    if begin >= end {
        return Err(ToolsError::invalid(
            "Invalid patch format: `*** Begin Patch` appears after `*** End Patch`",
        ));
    }

    let mut hunks = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let line = lines[index];
        if line.starts_with("*** Add File:") {
            let path = required_header_path(line, "*** Add File:")?;
            let mut content = Vec::new();
            index += 1;
            while index < end && !lines[index].starts_with("***") {
                let content_line = lines[index];
                let Some(stripped) = content_line.strip_prefix('+') else {
                    return Err(ToolsError::invalid(
                        "Add File content lines must start with `+`",
                    ));
                };
                content.push(stripped);
                index += 1;
            }
            hunks.push(PatchHunk::Add {
                path,
                contents: if content.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", content.join("\n"))
                },
            });
            continue;
        }
        if line.starts_with("*** Delete File:") {
            hunks.push(PatchHunk::Delete {
                path: required_header_path(line, "*** Delete File:")?,
            });
            index += 1;
            continue;
        }
        if line.starts_with("*** Update File:") {
            let path = required_header_path(line, "*** Update File:")?;
            index += 1;
            let mut move_path = None;
            if index < end && lines[index].starts_with("*** Move to:") {
                move_path = Some(required_header_path(lines[index], "*** Move to:")?);
                index += 1;
            }
            let mut chunks = Vec::new();
            while index < end && !lines[index].starts_with("***") {
                let marker = lines[index];
                let Some(marker_body) = marker.strip_prefix("@@") else {
                    return Err(ToolsError::invalid(format!(
                        "Expected `@@` patch chunk header, received: {marker}"
                    )));
                };
                let context = match marker_body.trim() {
                    "" => None,
                    value => Some(value.to_owned()),
                };
                index += 1;
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut end_of_file = false;
                while index < end
                    && !lines[index].starts_with("@@")
                    && (!lines[index].starts_with("***") || lines[index] == "*** End of File")
                {
                    let change = lines[index];
                    if change == "*** End of File" {
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    if let Some(value) = change.strip_prefix(' ') {
                        old_lines.push(value.to_owned());
                        new_lines.push(value.to_owned());
                    } else if let Some(value) = change.strip_prefix('-') {
                        old_lines.push(value.to_owned());
                    } else if let Some(value) = change.strip_prefix('+') {
                        new_lines.push(value.to_owned());
                    } else {
                        return Err(ToolsError::invalid(format!("Invalid patch line: {change}")));
                    }
                    index += 1;
                }
                chunks.push(UpdateChunk {
                    old_lines,
                    new_lines,
                    context,
                    end_of_file,
                });
            }
            if chunks.is_empty() {
                return Err(ToolsError::invalid(format!(
                    "Update File section has no chunks: {path}"
                )));
            }
            hunks.push(PatchHunk::Update {
                path,
                move_path,
                chunks,
            });
            continue;
        }
        if !line.trim().is_empty() {
            return Err(ToolsError::invalid(format!(
                "Invalid patch section header: {line}"
            )));
        }
        index += 1;
    }

    if hunks.is_empty() {
        return Err(ToolsError::invalid(
            "Patch must contain at least one file section",
        ));
    }
    Ok(hunks)
}

/// Applies `chunks` to `original` using whitespace-tolerant context seeking (`Exact` -> `TrimEnd` -> `Trim`).
///
/// # Errors
/// Returns [`ToolsError::InvalidInput`] if any context marker or expected line block cannot be matched.
pub fn apply_update_chunks(
    file_path: &str,
    chunks: &[UpdateChunk],
    original: &str,
) -> Result<(String, Vec<DiffSpan>), ToolsError> {
    let body = original.strip_suffix('\n').unwrap_or(original);
    let lines: Vec<String> = body.split('\n').map(ToOwned::to_owned).collect();
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut cursor = 0usize;

    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let Some(context_index) = seek(&lines, std::slice::from_ref(context), cursor, false)
            else {
                return Err(ToolsError::invalid(format!(
                    "Failed to find context `{context}` in {file_path}"
                )));
            };
            cursor = context_index + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let Some(found) = seek(&lines, &chunk.old_lines, cursor, chunk.end_of_file) else {
            return Err(ToolsError::invalid(format!(
                "Failed to find expected lines in {file_path}:\n{}",
                chunk.old_lines.join("\n")
            )));
        };
        replacements.push((found, chunk.old_lines.len(), chunk.new_lines.clone()));
        cursor = found + chunk.old_lines.len();
    }

    replacements.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut spans = Vec::with_capacity(replacements.len());
    let mut shift = 0isize;
    for (start, length, replacement) in replacements.iter().rev() {
        spans.push(DiffSpan {
            old_start: *start,
            old_len: *length,
            new_start: start.saturating_add_signed(shift),
            new_len: replacement.len(),
        });
        shift += replacement.len() as isize - *length as isize;
    }
    spans.sort_by_key(|s| (s.new_start, s.old_start));

    let mut next = lines;
    for (start, length, replacement) in replacements {
        next.splice(start..start + length, replacement);
    }
    Ok((format!("{}\n", next.join("\n")), spans))
}

/// Validates all target paths, acquires deterministic per-path locks, records pre-mutation snapshots,
/// applies all `Add`/`Update`/`Move to`/`Delete` hunks atomically per file, and returns [`ToolOutput::Diff`].
///
/// # Errors
/// Returns [`ToolsError`] before mutating disk if any path escapes `workspace_root`, a source file is missing,
/// or a hunk fails to match.
pub async fn apply_patch_to_workspace(
    workspace_root: &Path,
    session_id: &str,
    turn_id: &str,
    patch_text: &str,
    locks: &WorkspacePathLocks,
    snapshots: Option<&WorkspaceSnapshotStore>,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let hunks = parse_patch(patch_text)?;

    // Collect and validate every touched path up front before acquiring locks or mutating disk!
    let mut lock_keys = BTreeSet::new();
    let mut snapshot_targets = Vec::new();
    for hunk in &hunks {
        match hunk {
            PatchHunk::Add { path, .. } | PatchHunk::Delete { path } => {
                let (abs, rel) = resolve_inside_workspace(&canonical_ws, Path::new(path))?;
                lock_keys.insert(abs);
                snapshot_targets.push(rel);
            }
            PatchHunk::Update {
                path, move_path, ..
            } => {
                let (src_abs, src_rel) = resolve_inside_workspace(&canonical_ws, Path::new(path))?;
                lock_keys.insert(src_abs);
                snapshot_targets.push(src_rel);
                if let Some(dest) = move_path {
                    let (dst_abs, dst_rel) =
                        resolve_inside_workspace(&canonical_ws, Path::new(dest))?;
                    lock_keys.insert(dst_abs);
                    snapshot_targets.push(dst_rel);
                }
            }
        }
    }

    let ordered_paths: Vec<PathBuf> = lock_keys.into_iter().collect();
    let _guards = locks.acquire_many(&ordered_paths).await;

    enum StagedOp {
        Write {
            dest_abs: PathBuf,
            dest_rel: PathBuf,
            moved_from: Option<PathBuf>,
            remove_src: Option<PathBuf>,
            kind: FileChangeKind,
            old_text: String,
            new_text: String,
        },
        Delete {
            src_abs: PathBuf,
            src_rel: PathBuf,
            old_text: String,
        },
    }

    // Stage and validate all hunks in memory BEFORE touching any file on disk!
    let mut staged = Vec::with_capacity(hunks.len());
    for hunk in &hunks {
        match hunk {
            PatchHunk::Add { path, contents } => {
                let (abs, rel) = resolve_inside_workspace(&canonical_ws, Path::new(path))?;
                let old_text = if abs.is_file() {
                    fs::read_to_string(&abs).map_err(|source| ToolsError::Io {
                        path: abs.clone(),
                        source,
                    })?
                } else {
                    String::new()
                };
                staged.push(StagedOp::Write {
                    dest_abs: abs,
                    dest_rel: rel,
                    moved_from: None,
                    remove_src: None,
                    kind: FileChangeKind::Add,
                    old_text,
                    new_text: contents.clone(),
                });
            }
            PatchHunk::Delete { path } => {
                let (abs, rel) = resolve_inside_workspace(&canonical_ws, Path::new(path))?;
                if !abs.is_file() {
                    return Err(ToolsError::invalid(format!(
                        "Cannot delete missing file `{}`",
                        rel.display()
                    )));
                }
                let old_text = fs::read_to_string(&abs).map_err(|source| ToolsError::Io {
                    path: abs.clone(),
                    source,
                })?;
                staged.push(StagedOp::Delete {
                    src_abs: abs,
                    src_rel: rel,
                    old_text,
                });
            }
            PatchHunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let (src_abs, src_rel) = resolve_inside_workspace(&canonical_ws, Path::new(path))?;
                if !src_abs.is_file() {
                    return Err(ToolsError::invalid(format!(
                        "Cannot update missing file `{}`",
                        src_rel.display()
                    )));
                }
                let original = fs::read_to_string(&src_abs).map_err(|source| ToolsError::Io {
                    path: src_abs.clone(),
                    source,
                })?;
                let (updated, _) = apply_update_chunks(path, chunks, &original)?;

                if let Some(dest) = move_path {
                    let (dst_abs, dst_rel) =
                        resolve_inside_workspace(&canonical_ws, Path::new(dest))?;
                    let remove_src = (dst_abs != src_abs).then_some(src_abs);
                    staged.push(StagedOp::Write {
                        dest_abs: dst_abs,
                        dest_rel: dst_rel,
                        moved_from: Some(src_rel),
                        remove_src,
                        kind: FileChangeKind::Move,
                        old_text: original,
                        new_text: updated,
                    });
                } else {
                    staged.push(StagedOp::Write {
                        dest_abs: src_abs,
                        dest_rel: src_rel,
                        moved_from: None,
                        remove_src: None,
                        kind: FileChangeKind::Update,
                        old_text: original,
                        new_text: updated,
                    });
                }
            }
        }
    }

    let snapshot_id = if let Some(store) = snapshots {
        Some(
            store
                .capture_paths(session_id, turn_id, &canonical_ws, &snapshot_targets)?
                .snapshot_id,
        )
    } else {
        None
    };

    let mut diff_entries = Vec::with_capacity(staged.len());
    for op in staged {
        match op {
            StagedOp::Write {
                dest_abs,
                dest_rel,
                moved_from,
                remove_src,
                kind,
                old_text,
                new_text,
            } => {
                atomic_write_str(&dest_abs, &new_text)?;
                if let Some(old_src) = remove_src
                    && old_src.is_file()
                {
                    fs::remove_file(&old_src).map_err(|source| ToolsError::Io {
                        path: old_src,
                        source,
                    })?;
                }
                let (unified_diff, additions, deletions) =
                    build_unified_diff(&dest_rel, moved_from.as_deref(), &old_text, &new_text);
                diff_entries.push(FileDiffEntry {
                    path: dest_rel,
                    moved_from,
                    change_kind: kind,
                    unified_diff,
                    additions,
                    deletions,
                });
            }
            StagedOp::Delete {
                src_abs,
                src_rel,
                old_text,
            } => {
                fs::remove_file(&src_abs).map_err(|source| ToolsError::Io {
                    path: src_abs,
                    source,
                })?;
                let (unified_diff, additions, deletions) =
                    build_unified_diff(&src_rel, None, &old_text, "");
                diff_entries.push(FileDiffEntry {
                    path: src_rel,
                    moved_from: None,
                    change_kind: FileChangeKind::Delete,
                    unified_diff,
                    additions,
                    deletions,
                });
            }
        }
    }

    Ok(ToolOutput::Diff {
        files: diff_entries,
        snapshot_id,
    })
}

pub(crate) fn build_unified_diff(
    dest_rel: &Path,
    moved_from: Option<&Path>,
    old_text: &str,
    new_text: &str,
) -> (String, u32, u32) {
    let diff = TextDiff::from_lines(old_text, new_text);
    let mut additions = 0u32;
    let mut deletions = 0u32;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let old_hdr = moved_from
        .unwrap_or(dest_rel)
        .to_string_lossy()
        .replace('\\', "/");
    let new_hdr = dest_rel.to_string_lossy().replace('\\', "/");
    let unified = diff
        .unified_diff()
        .header(&format!("a/{old_hdr}"), &format!("b/{new_hdr}"))
        .to_string();
    (unified, additions, deletions)
}

pub(crate) fn resolve_inside_workspace(
    canonical_ws: &Path,
    candidate: &Path,
) -> Result<(PathBuf, PathBuf), ToolsError> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        canonical_ws.join(candidate)
    };
    let mut lexical = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::Prefix(p) => lexical.push(p.as_os_str()),
            Component::RootDir => lexical.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            Component::Normal(seg) => lexical.push(seg),
        }
    }
    let rel = lexical
        .strip_prefix(canonical_ws)
        .map_err(|_| ToolsError::PathViolation {
            path: candidate.to_path_buf(),
            message: format!("escapes workspace root `{}`", canonical_ws.display()),
        })?
        .to_path_buf();

    // Prevent symlink escape when parent or target already exists on disk!
    if let Some(existing_anc) = lexical.ancestors().find(|a| a.exists()) {
        let canon_anc = fs::canonicalize(existing_anc).map_err(|source| ToolsError::Io {
            path: existing_anc.to_path_buf(),
            source,
        })?;
        if !canon_anc.starts_with(canonical_ws) {
            return Err(ToolsError::PathViolation {
                path: candidate.to_path_buf(),
                message: "resolves via symlink outside workspace root".to_string(),
            });
        }
    }
    Ok((lexical, rel))
}

pub(crate) fn atomic_write_str(dest: &Path, content: &str) -> Result<(), ToolsError> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ToolsError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| ToolsError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(content.as_bytes())
        .map_err(|source| ToolsError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
    tmp.persist(dest).map_err(|e| ToolsError::Io {
        path: dest.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.len() > lines.len() {
        return None;
    }
    for comparison in [Comparison::Exact, Comparison::TrimEnd, Comparison::Trim] {
        let first = if eof {
            lines.len().saturating_sub(pattern.len())
        } else {
            start
        };
        let last = if eof {
            first
        } else {
            lines.len().saturating_sub(pattern.len())
        };
        if first > last {
            continue;
        }
        for index in first..=last {
            if index >= start
                && pattern.iter().enumerate().all(|(offset, expected)| {
                    comparison.matches(
                        lines.get(index + offset).map(String::as_str).unwrap_or(""),
                        expected,
                    )
                })
            {
                return Some(index);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Comparison {
    Exact,
    TrimEnd,
    Trim,
}

impl Comparison {
    fn matches(self, left: &str, right: &str) -> bool {
        match self {
            Self::Exact => left == right,
            Self::TrimEnd => left.trim_end() == right.trim_end(),
            Self::Trim => left.trim() == right.trim(),
        }
    }
}

fn required_header_path(line: &str, prefix: &str) -> Result<String, ToolsError> {
    let value = line[prefix.len()..].trim();
    if value.is_empty() {
        return Err(ToolsError::invalid(format!("{prefix} requires a path")));
    }
    Ok(value.to_owned())
}

fn strip_heredoc(input: &str) -> Option<&str> {
    let newline = input.find('\n')?;
    let mut header = input[..newline].trim();
    if let Some(rest) = header.strip_prefix("cat") {
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        header = rest.trim_start();
    }
    let raw_delimiter = header.strip_prefix("<<")?.trim();
    let delimiter = raw_delimiter
        .strip_prefix(['\'', '"'])
        .and_then(|value| value.strip_suffix(['\'', '"']))
        .unwrap_or(raw_delimiter);
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    let body_and_footer = &input[newline + 1..];
    let footer_start = body_and_footer.rfind('\n')?;
    if body_and_footer[footer_start + 1..].trim() != delimiter {
        return None;
    }
    Some(&body_and_footer[..footer_start])
}
