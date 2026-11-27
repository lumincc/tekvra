//! Zero-roundtrip user-prompt context preprocessing: workspace `@path` / `@path#L<start>-L<end>`
//! attachment expansion, `@` image attachments, and on-demand `/skill <name>` instruction injection.
//!
//! Expansions only ever extend the *submitted user message* (`Frame` tail) — the cached
//! `developer_prefix`, historical messages, and tool roster stay byte-identical, so
//! [`crate::Frame::assert_append_only`] keeps holding across every expanded turn.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use tekvra_config::SkillMetadata;
use tekvra_providers::MessagePart;
use tekvra_tools::is_sensitive_credential_path;

/// Hard per-file byte cap for text attachments (64 KiB).
pub const MAX_TEXT_FILE_BYTES: usize = 64 * 1024;
/// Aggregate per-turn byte cap across all text attachments (256 KiB).
pub const MAX_TURN_TEXT_BYTES: usize = 256 * 1024;
/// Hard per-image byte cap (10 MiB).
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Maximum number of image attachments per turn.
pub const MAX_IMAGES_PER_TURN: usize = 4;

/// Result of preprocessing a submitted user prompt.
#[derive(Debug, Clone, Default)]
pub struct AttachmentExpansion {
    /// Prompt text after `@token` replacement (or the original text when nothing matched).
    pub text: String,
    /// Image parts converted from `@image.*` mentions, in first-mention order.
    pub images: Vec<MessagePart>,
    /// Non-fatal diagnostics (e.g., a sensitive credential path was withheld).
    pub warnings: Vec<String>,
}

/// Outcome of `/skill <name> [prompt]` parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillInvocation {
    /// Prompt does not start with `/skill`.
    None,
    /// Skill resolved; `expanded` carries the `<skill_instructions>` block plus the remaining prompt.
    Expanded { expanded: String },
    /// Skill name not found in `skills`; `name` is the unresolved identifier.
    Unknown { name: String },
}

/// Supported image extensions mapped to IANA media types.
const IMAGE_MEDIA_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// Expands workspace `@path` and `@path#L<start>-L<end>` mentions (and `@image.*` attachments)
/// in `prompt` relative to canonical `workspace_root`.
///
/// Rules:
/// - Only tokens at whitespace/line boundaries participate; `@@` escapes a literal `@`.
/// - Tokens inside fenced code blocks (``` fences) are left untouched.
/// - Paths escaping the workspace (`..`) or matching [`is_sensitive_credential_path`] are never
///   attached; sensitive skips emit a warning while escapes/missing files fail silently.
/// - Oversized, non-UTF-8, or budget-exhausted mentions are left as literal text.
#[must_use]
pub fn expand_prompt_attachments(prompt: &str, workspace_root: &Path) -> AttachmentExpansion {
    let mut out = AttachmentExpansion {
        text: String::with_capacity(prompt.len()),
        images: Vec::new(),
        warnings: Vec::new(),
    };
    let canonical_ws =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());

    let bytes = prompt.as_bytes();
    let mut replaced_ranges: Vec<(usize, usize, String)> = Vec::new();
    let mut in_fence = false;
    let mut remaining_text_budget = MAX_TURN_TEXT_BYTES;

    let mut i = 0usize;
    let mut line_start = true;
    while i < bytes.len() {
        let ch = prompt[i..].chars().next().unwrap_or('\0');
        let ch_len = ch.len_utf8();

        if ch == '`' {
            // Track ``` fence toggles (a fence opening must sit at line start).
            if prompt[i..].starts_with("```") {
                in_fence = !in_fence;
                i += 3;
                line_start = false;
                continue;
            }
        }

        let at_boundary = line_start || bytes[i - 1] == b'\n' || bytes[i - 1].is_ascii_whitespace();
        if ch == '@' && !in_fence && at_boundary {
            let rest = &prompt[i + 1..];
            let token_end_abs = i + 1 + rest.find(char::is_whitespace).unwrap_or(rest.len());
            let raw_token = &prompt[i + 1..token_end_abs];

            if raw_token == "@" {
                // `@@` -> literal `@`; strip one `@` from the replacement text.
                replaced_ranges.push((i, token_end_abs, "@".to_string()));
                i = token_end_abs;
                line_start = false;
                continue;
            }

            if let Some(expansion) = resolve_attachment_token(
                raw_token,
                &canonical_ws,
                &mut remaining_text_budget,
                &mut out,
            ) {
                replaced_ranges.push((i, token_end_abs, expansion));
                i = token_end_abs;
                line_start = false;
                continue;
            }
        }

        line_start = ch == '\n';
        i += ch_len;
    }

    // Materialize the final text with replacements applied in order.
    let mut cursor = 0usize;
    for (start, end, replacement) in &replaced_ranges {
        out.text.push_str(&prompt[cursor..*start]);
        out.text.push_str(replacement);
        cursor = *end;
    }
    out.text.push_str(&prompt[cursor..]);
    out
}

/// Resolves a single `@token` to replacement text or an image part.
/// Returns the replacement text when the mention was expanded/replaced, `None` to keep literal.
fn resolve_attachment_token(
    raw_token: &str,
    canonical_ws: &Path,
    remaining_text_budget: &mut usize,
    out: &mut AttachmentExpansion,
) -> Option<String> {
    if raw_token.starts_with('#') || raw_token.is_empty() {
        return None;
    }

    // Split `<path>` from optional `#L<start>` / `#L<start>-L<end>` suffix.
    let (rel_raw, line_range) = match raw_token.split_once('#') {
        Some((p, suffix)) => {
            let parsed = parse_line_suffix(suffix)?;
            (p, Some(parsed))
        }
        None => (raw_token, None),
    };

    if rel_raw.is_empty() || rel_raw.contains('\\') || rel_raw.contains("..") {
        return None;
    }

    let rel_path = PathBuf::from(rel_raw);
    let joined = canonical_ws.join(&rel_path);
    let resolved = std::fs::canonicalize(&joined).ok()?;
    if !resolved.is_file() || !resolved.starts_with(canonical_ws) {
        return None;
    }

    if is_sensitive_credential_path(&resolved) {
        out.warnings.push(format!(
            "Withheld `@{rel_raw}` from context: matches a sensitive credential file pattern."
        ));
        // Replace the mention with an explicit denial so the model does not keep asking.
        return Some(format!(
            "<attached_file path=\"{rel_raw}\" withheld=\"sensitive_credential\" />"
        ));
    }

    let meta_len = fs::metadata(&resolved).ok()?.len();

    // Image attachments -> `MessagePart::Image` blocks.
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if let Some((_, media_type)) = IMAGE_MEDIA_TYPES.iter().find(|(e, _)| *e == ext) {
        if out.images.len() >= MAX_IMAGES_PER_TURN || meta_len > MAX_IMAGE_BYTES as u64 {
            return None;
        }
        let bytes = fs::read(&resolved).ok()?;
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        out.images.push(MessagePart::Image {
            media_type: (*media_type).to_string(),
            data_base64,
            source_path: Some(rel_raw.to_string()),
        });
        // Keep the textual mention so the model sees the attachment marker.
        return Some(format!(
            "<attached_image path=\"{rel_raw}\" media_type=\"{media_type}\" />"
        ));
    }

    // UTF-8 text attachments -> bounded `<attached_file>` blocks.
    if meta_len > MAX_TEXT_FILE_BYTES as u64 || meta_len > *remaining_text_budget as u64 {
        return None;
    }
    let raw = fs::read_to_string(&resolved).ok()?;
    let (block, consumed) = render_text_attachment(rel_raw, &raw, line_range)?;
    *remaining_text_budget = remaining_text_budget.saturating_sub(consumed);
    Some(block)
}

/// Parses `L<n>` or `L<n>-L<m>` suffixes into an inclusive `(start, end)` pair.
fn parse_line_suffix(suffix: &str) -> Option<(usize, usize)> {
    let body = suffix
        .strip_prefix('L')
        .or_else(|| suffix.strip_prefix('l'))?;
    if let Some((s, e)) = body.split_once("-L") {
        let start = s.parse::<usize>().ok()?;
        let end = e.parse::<usize>().ok()?;
        (start >= 1 && end >= start).then_some((start, end))
    } else {
        let start = body.parse::<usize>().ok()?;
        (start >= 1).then_some((start, start))
    }
}

/// Renders `<attached_file>` content limited to `line_range` (1-based inclusive).
/// Returns the block text and the exact number of source bytes consumed against the budget.
fn render_text_attachment(
    rel_raw: &str,
    raw: &str,
    line_range: Option<(usize, usize)>,
) -> Option<(String, usize)> {
    let (body, lines_label, consumed_bytes) = match line_range {
        Some((start, end)) => {
            let mut acc = 0usize;
            let mut selected: Vec<&str> = Vec::new();
            let mut found_any = false;
            for (idx, line) in raw.lines().enumerate() {
                let line_no = idx + 1;
                let line_bytes = line.len() + 1;
                if line_no >= start && line_no <= end {
                    found_any = true;
                    selected.push(line);
                    acc += line_bytes;
                }
            }
            if !found_any {
                return None;
            }
            (
                selected.join("\n"),
                format!("{start}-{end}"),
                acc.min(MAX_TEXT_FILE_BYTES),
            )
        }
        None => (
            raw.trim_end().to_string(),
            format!("{}", raw.lines().count()),
            raw.len(),
        ),
    };

    let block = format!(
        "<attached_file path=\"{rel_raw}\" lines=\"{lines_label}\">\n{body}\n</attached_file>"
    );
    Some((block, consumed_bytes))
}

/// Detects a leading `/skill <name> [prompt]` invocation and renders the skill body as a
/// `<skill_instructions>` block appended before the remaining prompt text.
#[must_use]
pub fn expand_skill_invocation(prompt: &str, skills: &[SkillMetadata]) -> SkillInvocation {
    let trimmed_start = prompt.trim_start();
    let Some(rest) = trimmed_start.strip_prefix("/skill") else {
        return SkillInvocation::None;
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return SkillInvocation::None;
    }
    let body = rest.trim_start();
    let (name, remaining) = match body.split_once(char::is_whitespace) {
        Some((n, r)) => (n.trim(), r.trim()),
        None => (body.trim(), ""),
    };
    if name.is_empty() {
        return SkillInvocation::None;
    }
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        return SkillInvocation::Unknown {
            name: name.to_string(),
        };
    };

    let instructions = match tekvra_config::load_skill_body(skill) {
        Ok(body_text) => body_text,
        Err(err) => {
            return SkillInvocation::Unknown {
                name: format!("{name} (load failed: {err})"),
            };
        }
    };

    let mut expanded = format!(
        "<skill_instructions name=\"{}\" path=\"{}\">\n{}\n</skill_instructions>",
        skill.name,
        skill.path.display(),
        instructions.trim()
    );
    if !remaining.is_empty() {
        expanded.push_str("\n\n");
        expanded.push_str(remaining);
    }
    SkillInvocation::Expanded { expanded }
}
