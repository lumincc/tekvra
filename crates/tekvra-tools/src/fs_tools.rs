//! Bounded workspace filesystem tools (`file_read`, `file_write`, `file_edit`, `file_glob`,
//! `file_grep`, `view_image`) and native tree-sitter structural symbol outline (`file_index`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use tekvra_protocol::{FileChangeKind, FileDiffEntry, SearchMatch, SymbolOutline, ToolOutput};
use tekvra_storage::WorkspaceSnapshotStore;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tree_sitter::{Node, Parser};

use crate::ToolsError;
use crate::patch::{atomic_write_str, build_unified_diff, resolve_inside_workspace};

const MAX_READ_BYTES: usize = 256 * 1024;
const DEFAULT_READ_LINES: usize = 400;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Async per-path mutex registry preventing concurrent races across `apply_patch`, `file_edit`, and `file_write`.
#[derive(Debug, Clone, Default)]
pub struct WorkspacePathLocks {
    inner: Arc<std::sync::Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>>,
}

impl WorkspacePathLocks {
    /// Creates an empty path lock registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires locks on `sorted_paths` in deterministic lexicographical order.
    pub async fn acquire_many(&self, paths: &[PathBuf]) -> Vec<OwnedMutexGuard<()>> {
        let mut unique: Vec<PathBuf> = paths.to_vec();
        unique.sort();
        unique.dedup();

        let mutexes: Vec<Arc<Mutex<()>>> = {
            let mut map = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            unique
                .into_iter()
                .map(|p| Arc::clone(map.entry(p).or_insert_with(|| Arc::new(Mutex::new(())))))
                .collect()
        };

        let mut guards = Vec::with_capacity(mutexes.len());
        for m in mutexes {
            guards.push(m.lock_owned().await);
        }
        guards
    }
}

/// Returns `true` when `path` is a credential/secret file that MUST be skipped during workspace directory scans
/// (`.env`, `.env.*`, private keys, `.npmrc`, `.pypirc`, `.netrc`, `.ssh/*`).
#[must_use]
pub fn is_sensitive_credential_path(path: &Path) -> bool {
    for comp in path.components() {
        let Component::Normal(os_name) = comp else {
            continue;
        };
        let name = os_name.to_string_lossy();
        let lower = name.to_ascii_lowercase();
        if lower == ".ssh"
            || lower == ".aws"
            || lower == ".gnupg"
            || lower == ".env"
            || lower.starts_with(".env.")
            || lower == ".npmrc"
            || lower == ".pypirc"
            || lower == ".netrc"
            || lower == "id_rsa"
            || lower == "id_ed25519"
            || lower == "id_ecdsa"
            || lower == "credentials.json"
            || lower.ends_with(".pem")
            || lower.ends_with(".key")
            || lower.ends_with(".p12")
            || lower.ends_with(".pfx")
        {
            return true;
        }
    }
    false
}

/// Bounded `file_read` returning a 1-based line slice (`ToolOutput::FileSlice`).
///
/// # Errors
/// Returns [`ToolsError`] if `target` escapes `workspace_root` or cannot be read.
pub fn file_read(
    workspace_root: &Path,
    target: &Path,
    start_line: Option<usize>,
    max_lines: Option<usize>,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let (abs, rel) = resolve_inside_workspace(&canonical_ws, target)?;
    let raw = fs::read_to_string(&abs).map_err(|source| ToolsError::Io { path: abs, source })?;

    let all_lines: Vec<&str> = raw.lines().collect();
    let total_lines = all_lines.len();
    let start_1 = start_line.unwrap_or(1).max(1);
    let limit = max_lines.unwrap_or(DEFAULT_READ_LINES).max(1);
    let start_idx = (start_1 - 1).min(total_lines);
    let mut end_idx = (start_idx + limit).min(total_lines);

    let mut content = String::new();
    let mut byte_truncated = false;
    for (idx, line) in all_lines[start_idx..end_idx].iter().enumerate() {
        let formatted = format!("{:>5} | {line}\n", start_idx + idx + 1);
        if content.len() + formatted.len() > MAX_READ_BYTES {
            end_idx = start_idx + idx;
            byte_truncated = true;
            break;
        }
        content.push_str(&formatted);
    }

    let truncated = byte_truncated || end_idx < total_lines;
    Ok(ToolOutput::FileSlice {
        path: rel,
        start_line: if total_lines == 0 {
            0
        } else {
            (start_idx + 1) as u32
        },
        end_line: end_idx as u32,
        total_lines: total_lines as u32,
        content,
        truncated,
    })
}

/// Writes `content` to `target` inside `workspace_root` after capturing a pre-mutation snapshot.
///
/// # Errors
/// Returns [`ToolsError`] if `target` escapes `workspace_root` or I/O fails.
pub async fn file_write(
    workspace_root: &Path,
    session_id: &str,
    turn_id: &str,
    target: &Path,
    content: &str,
    locks: &WorkspacePathLocks,
    snapshots: Option<&WorkspaceSnapshotStore>,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let (abs, rel) = resolve_inside_workspace(&canonical_ws, target)?;
    let _guards = locks.acquire_many(std::slice::from_ref(&abs)).await;

    let existed = abs.is_file();
    let old_text = if existed {
        fs::read_to_string(&abs).map_err(|source| ToolsError::Io {
            path: abs.clone(),
            source,
        })?
    } else {
        String::new()
    };

    let snapshot_id = if let Some(store) = snapshots {
        Some(
            store
                .capture_paths(
                    session_id,
                    turn_id,
                    &canonical_ws,
                    std::slice::from_ref(&rel),
                )?
                .snapshot_id,
        )
    } else {
        None
    };

    atomic_write_str(&abs, content)?;
    let (unified_diff, additions, deletions) = build_unified_diff(&rel, None, &old_text, content);
    Ok(ToolOutput::Diff {
        files: vec![FileDiffEntry {
            path: rel,
            moved_from: None,
            change_kind: if existed {
                FileChangeKind::Update
            } else {
                FileChangeKind::Add
            },
            unified_diff,
            old_text: if existed { Some(old_text) } else { None },
            new_text: Some(content.to_string()),
            additions,
            deletions,
        }],
        snapshot_id,
    })
}

/// Edit specification passed to [`file_edit`].
#[derive(Debug, Clone, Copy)]
pub struct FileEditSpec<'a> {
    /// Target path relative to or inside `workspace_root`.
    pub target: &'a Path,
    /// Non-empty exact substring to match.
    pub old_string: &'a str,
    /// Replacement substring.
    pub new_string: &'a str,
    /// Replace all occurrences when `true`, or require a unique match when `false`.
    pub replace_all: bool,
}

/// Performs an exact/unique (or `replace_all`) substring edit on `spec.target` (`file_edit` for non-`apply_patch` models).
///
/// # Errors
/// Returns [`ToolsError`] if `old_string` is not found, matches ambiguously when `!replace_all`, or I/O fails.
pub async fn file_edit(
    workspace_root: &Path,
    session_id: &str,
    turn_id: &str,
    spec: FileEditSpec<'_>,
    locks: &WorkspacePathLocks,
    snapshots: Option<&WorkspaceSnapshotStore>,
) -> Result<ToolOutput, ToolsError> {
    let FileEditSpec {
        target,
        old_string,
        new_string,
        replace_all,
    } = spec;
    if old_string.is_empty() {
        return Err(ToolsError::invalid(
            "`old_string` in `file_edit` must be non-empty",
        ));
    }
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let (abs, rel) = resolve_inside_workspace(&canonical_ws, target)?;
    let _guards = locks.acquire_many(std::slice::from_ref(&abs)).await;

    let original = fs::read_to_string(&abs).map_err(|source| ToolsError::Io {
        path: abs.clone(),
        source,
    })?;
    let occurrences = original.matches(old_string).count();
    if occurrences == 0 {
        return Err(ToolsError::invalid(format!(
            "`old_string` not found in `{}`",
            rel.display()
        )));
    }
    if occurrences > 1 && !replace_all {
        return Err(ToolsError::invalid(format!(
            "`old_string` matched {occurrences} locations in `{}`; provide more context or set `replace_all = true`",
            rel.display()
        )));
    }

    let snapshot_id = if let Some(store) = snapshots {
        Some(
            store
                .capture_paths(
                    session_id,
                    turn_id,
                    &canonical_ws,
                    std::slice::from_ref(&rel),
                )?
                .snapshot_id,
        )
    } else {
        None
    };

    let updated = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };
    atomic_write_str(&abs, &updated)?;

    let (unified_diff, additions, deletions) = build_unified_diff(&rel, None, &original, &updated);
    Ok(ToolOutput::Diff {
        files: vec![FileDiffEntry {
            path: rel,
            moved_from: None,
            change_kind: FileChangeKind::Update,
            unified_diff,
            old_text: Some(original),
            new_text: Some(updated),
            additions,
            deletions,
        }],
        snapshot_id,
    })
}

/// Bounded `file_glob` skipping `.git` and sensitive credential files with explicit truncation metadata.
///
/// # Errors
/// Returns [`ToolsError`] if `pattern` is invalid or `workspace_root` cannot be walked.
pub fn file_glob(
    workspace_root: &Path,
    pattern: &str,
    max_matches: usize,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let glob = Glob::new(pattern)
        .map_err(|e| ToolsError::invalid(format!("invalid glob `{pattern}`: {e}")))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    let matcher = builder
        .build()
        .map_err(|e| ToolsError::invalid(format!("invalid glob set: {e}")))?;

    let mut matches = Vec::new();
    let mut truncated = false;
    let mut skipped_sensitive_files = 0u32;
    let cap = max_matches.max(1);

    let walker = WalkBuilder::new(&canonical_ws)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_name(std::cmp::Ord::cmp)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&canonical_ws) else {
            continue;
        };
        if is_sensitive_credential_path(rel) {
            skipped_sensitive_files += 1;
            continue;
        }
        if matcher.is_match(rel)
            || rel
                .file_name()
                .is_some_and(|fname| matcher.is_match(Path::new(fname)))
        {
            if matches.len() >= cap {
                truncated = true;
                break;
            }
            matches.push(SearchMatch {
                path: rel.to_path_buf(),
                line: None,
                snippet: None,
            });
        }
    }

    Ok(ToolOutput::Search {
        matches,
        truncated,
        skipped_sensitive_files,
    })
}

/// Bounded `file_grep` searching workspace files while skipping `.git` and credential files
/// and reporting explicit `truncated` + `skipped_sensitive_files` metadata.
///
/// # Errors
/// Returns [`ToolsError`] if `pattern` is not a valid regex or `workspace_root` fails to canonicalize.
pub fn file_grep(
    workspace_root: &Path,
    pattern: &str,
    subpath: Option<&Path>,
    max_matches: usize,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let search_root = match subpath {
        Some(sub) => resolve_inside_workspace(&canonical_ws, sub)?.0,
        None => canonical_ws.clone(),
    };
    let regex = Regex::new(pattern)
        .map_err(|e| ToolsError::invalid(format!("invalid regex `{pattern}`: {e}")))?;

    let mut matches = Vec::new();
    let mut truncated = false;
    let mut skipped_sensitive_files = 0u32;
    let cap = max_matches.max(1);

    let walker = WalkBuilder::new(&search_root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_name(std::cmp::Ord::cmp)
        .build();

    'files: for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&canonical_ws) else {
            continue;
        };
        if is_sensitive_credential_path(rel) {
            skipped_sensitive_files += 1;
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                if matches.len() >= cap {
                    truncated = true;
                    break 'files;
                }
                let snippet: String = line.chars().take(240).collect();
                matches.push(SearchMatch {
                    path: rel.to_path_buf(),
                    line: Some((idx + 1) as u32),
                    snippet: Some(snippet),
                });
            }
        }
    }

    Ok(ToolOutput::Search {
        matches,
        truncated,
        skipped_sensitive_files,
    })
}

/// Inspects an image file inside `workspace_root` (`view_image`) and returns dimensions/MIME/base64 preview metadata.
///
/// # Errors
/// Returns [`ToolsError`] if `target` escapes `workspace_root`, is missing, or exceeds size limits.
pub fn view_image(workspace_root: &Path, target: &Path) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let (abs, rel) = resolve_inside_workspace(&canonical_ws, target)?;
    let bytes = fs::read(&abs).map_err(|source| ToolsError::Io { path: abs, source })?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ToolsError::invalid(format!(
            "image `{}` exceeds {MAX_IMAGE_BYTES} byte limit",
            rel.display()
        )));
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(ToolsError::invalid(format!(
            "`{}` is not a supported PNG/JPEG/GIF/WEBP image",
            rel.display()
        )));
    };
    let encoded_prefix: String =
        base64::engine::general_purpose::STANDARD.encode(&bytes[..bytes.len().min(64)]);
    Ok(ToolOutput::Text {
        text: format!(
            "image={} mime={mime} bytes={} base64_head={encoded_prefix}",
            rel.display(),
            bytes.len()
        ),
    })
}

/// Native tree-sitter symbol outline extractor (`file_index`) supporting Rust, Python, Go,
/// JavaScript, TypeScript/TSX, and Bash with zero Lua runtime dependency.
///
/// # Errors
/// Returns [`ToolsError`] if `target` escapes `workspace_root`, has an unsupported extension, or fails to parse.
pub fn extract_file_outline(
    workspace_root: &Path,
    target: &Path,
) -> Result<ToolOutput, ToolsError> {
    let canonical_ws = fs::canonicalize(workspace_root).map_err(|source| ToolsError::Io {
        path: workspace_root.to_path_buf(),
        source,
    })?;
    let (abs, rel) = resolve_inside_workspace(&canonical_ws, target)?;
    let source = fs::read_to_string(&abs).map_err(|source| ToolsError::Io { path: abs, source })?;

    let ext = rel
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (lang_name, ts_lang): (&str, tree_sitter::Language) = match ext.as_str() {
        "rs" => ("rust", tree_sitter_rust::LANGUAGE.into()),
        "py" => ("python", tree_sitter_python::LANGUAGE.into()),
        "go" => ("go", tree_sitter_go::LANGUAGE.into()),
        "js" | "mjs" | "cjs" | "jsx" => ("javascript", tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "mts" | "cts" => (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        "tsx" => ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        "sh" | "bash" => ("bash", tree_sitter_bash::LANGUAGE.into()),
        other => {
            return Err(ToolsError::invalid(format!(
                "unsupported language extension `.{other}` for `file_index`"
            )));
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|e| ToolsError::invalid(format!("tree-sitter language error: {e}")))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ToolsError::invalid("tree-sitter failed to parse source"))?;

    let mut symbols = Vec::new();
    collect_symbols(tree.root_node(), source.as_bytes(), &mut symbols);

    Ok(ToolOutput::Outline {
        path: rel,
        language: lang_name.to_string(),
        symbols,
    })
}

fn collect_symbols(node: Node<'_>, source: &[u8], out: &mut Vec<SymbolOutline>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((kind, name)) = classify_symbol_node(child, source) {
            let mut nested = Vec::new();
            collect_symbols(child, source, &mut nested);
            out.push(SymbolOutline {
                name,
                kind: kind.to_string(),
                start_line: (child.start_position().row + 1) as u32,
                end_line: (child.end_position().row + 1) as u32,
                children: nested,
            });
        } else {
            collect_symbols(child, source, out);
        }
    }
}

fn classify_symbol_node(node: Node<'_>, source: &[u8]) -> Option<(&'static str, String)> {
    let mapped_kind = match node.kind() {
        "function_item" | "function_definition" | "function_declaration" => "fn",
        "method_declaration" | "method_definition" => "method",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "class_definition" | "class_declaration" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" | "type_item" | "type_spec" => "type",
        _ => return None,
    };

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))?;
    let text = name_node.utf8_text(source).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    Some((mapped_kind, text.lines().next().unwrap_or(text).to_string()))
}
