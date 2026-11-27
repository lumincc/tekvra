//! Apache-2.0 port of Workcell's RTK-derived command-aware `output-filter`:
//! single-row CR `\r` progress-bar decoding (`render_terminal`), ANSI escape stripping,
//! monotonic progress-frame collapsing, and non-zero-safe rule-based noise reduction.

use std::collections::BTreeMap;
use std::sync::{LazyLock, OnceLock};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

const ROW_CANVAS_CHARS: usize = 16_384;
const ROW_MARK_BYTES: usize = 4_096;
const MAX_ESCAPE_CHARS: usize = 64;
const MAX_OSC_CHARS: usize = 512;
const SCAN_WINDOW_CHARS: usize = MAX_OSC_CHARS + 2;
const MAX_INPUT_BYTES: usize = 1_048_576;
const MAX_INPUT_LINES: usize = 100_000;
const FALLBACK_MAX_LINES: usize = 180;

const EMBEDDED_RULES_TOML: &str = r#"
[filters.cargo-test]
description = "Compact cargo test output while retaining failing assertions and summaries"
match_command = "^cargo\\s+(nextest\\s+run|test)\\b"
strip_ansi = true
filter_stderr = true
strip_lines_matching = [
  "^\\s*Compiling\\s+\\S+\\s+v\\d+",
  "^\\s*Downloading\\s+crates",
  "^\\s*Downloaded\\s+\\S+",
  "^\\s*Locking\\s+\\d+\\s+package",
  "^\\s*Running\\s+unittests\\b",
  "^\\s*Doc-tests\\b",
]
max_lines = 120

[filters.cargo-build]
description = "Compact cargo build/check/clippy output while retaining warnings and errors"
match_command = "^cargo\\s+(build|check|clippy)\\b"
strip_ansi = true
filter_stderr = true
strip_lines_matching = [
  "^\\s*Compiling\\s+\\S+\\s+v\\d+",
  "^\\s*Checking\\s+\\S+\\s+v\\d+",
  "^\\s*Downloading\\s+crates",
  "^\\s*Downloaded\\s+\\S+",
]
max_lines = 120
on_empty = "cargo: ok"

[filters.pytest]
description = "Compact pytest output"
match_command = "^(pytest|python3?\\s+-m\\s+pytest)\\b"
strip_ansi = true
filter_stderr = true
strip_lines_matching = [
  "^cachedir:",
  "^rootdir:",
  "^plugins:",
  "^collecting \\.\\.\\.",
]
max_lines = 120

[filters.git-status]
description = "Compact git status hints"
match_command = "^git\\s+status\\b"
strip_ansi = true
strip_lines_matching = [
  "^\\s*\\(use \"git ",
]
max_lines = 100
on_empty = "git status: clean"
"#;

/// Incremental single-row terminal redraw decoder (`\r`, `\x08`, `\x1b[K`, cursor column moves).
#[derive(Debug, Default)]
pub struct RowRenderer {
    canvas: Vec<char>,
    marks: Vec<(usize, String)>,
    mark_bytes: usize,
    column: usize,
    row_redraws: u64,
    row_dirty: bool,
    partial: String,
    redraws: u64,
}

impl RowRenderer {
    /// Creates an empty [`RowRenderer`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total carriage-return redraw frames absorbed so far.
    #[must_use]
    pub const fn redraws(&self) -> u64 {
        self.redraws
    }

    /// Feeds a text chunk, appending completed rows to `out`.
    pub fn push(&mut self, text: &str, out: &mut String) {
        let source: Vec<char> = if self.partial.is_empty() {
            text.chars().collect()
        } else {
            let mut joined: Vec<char> = std::mem::take(&mut self.partial).chars().collect();
            joined.extend(text.chars());
            joined
        };

        let mut index = 0;
        while index < source.len() {
            match source[index] {
                '\n' => {
                    self.end_row(out);
                    index += 1;
                }
                '\r' => {
                    let Some(next) = source.get(index + 1) else {
                        self.partial.push('\r');
                        return;
                    };
                    if *next == '\n' {
                        index += 1;
                        continue;
                    }
                    self.carriage_return();
                    index += 1;
                }
                '\u{8}' => {
                    if self.column > 0 {
                        self.column -= 1;
                        self.row_dirty = true;
                    }
                    index += 1;
                }
                '\u{1b}' => match scan_escape(&source, index) {
                    Scan::Incomplete => {
                        self.partial = source[index..].iter().collect();
                        return;
                    }
                    Scan::Found(end) => {
                        let raw: String = source[index..end].iter().collect();
                        self.apply_escape(raw);
                        index = end;
                    }
                },
                ch => {
                    self.write(ch, out);
                    index += 1;
                }
            }
        }
    }

    /// Flushes any trailing unterminated row.
    pub fn finish(&mut self, out: &mut String) {
        let orphan = std::mem::take(&mut self.partial);
        if orphan.starts_with('\u{1b}') {
            self.push_mark(orphan);
        } else if orphan.starts_with('\r') {
            self.carriage_return();
        }
        if self.canvas.is_empty() && !self.row_dirty {
            return;
        }
        self.flush_row(out);
        self.reset_row();
    }

    fn carriage_return(&mut self) {
        if !self.canvas.is_empty() {
            self.row_redraws = self.row_redraws.saturating_add(1);
            self.row_dirty = true;
        }
        self.column = 0;
        self.marks.clear();
        self.mark_bytes = 0;
    }

    fn write(&mut self, ch: char, out: &mut String) {
        if self.column >= ROW_CANVAS_CHARS {
            self.flush_row(out);
            out.push('\n');
            self.reset_row();
        }
        while self.canvas.len() < self.column {
            self.canvas.push(' ');
        }
        if self.column < self.canvas.len() {
            self.canvas[self.column] = ch;
        } else {
            self.canvas.push(ch);
        }
        self.column += 1;
    }

    fn end_row(&mut self, out: &mut String) {
        self.flush_row(out);
        out.push('\n');
        self.reset_row();
    }

    fn reset_row(&mut self) {
        self.canvas.clear();
        self.marks.clear();
        self.mark_bytes = 0;
        self.column = 0;
        self.row_redraws = 0;
        self.row_dirty = false;
    }

    fn flush_row(&mut self, out: &mut String) {
        self.redraws = self.redraws.saturating_add(self.row_redraws);
        let mut end = self.canvas.len();
        if self.row_dirty {
            while end > 0 && self.canvas[end - 1] == ' ' {
                end -= 1;
            }
        }
        let mut mark = 0;
        for (column, ch) in self.canvas[..end].iter().enumerate() {
            while mark < self.marks.len() && self.marks[mark].0 <= column {
                out.push_str(&self.marks[mark].1);
                mark += 1;
            }
            out.push(*ch);
        }
        for (_, raw) in &self.marks[mark..] {
            out.push_str(raw);
        }
    }

    fn push_mark(&mut self, raw: String) {
        if self.mark_bytes.saturating_add(raw.len()) > ROW_MARK_BYTES {
            return;
        }
        self.mark_bytes += raw.len();
        let at = self.marks.partition_point(|(col, _)| *col <= self.column);
        self.marks.insert(at, (self.column, raw));
    }

    fn apply_escape(&mut self, raw: String) {
        let Some(body) = raw.strip_prefix("\u{1b}[") else {
            self.push_mark(raw);
            return;
        };
        let Some(final_byte) = body.chars().next_back() else {
            self.push_mark(raw);
            return;
        };
        let params = &body[..body.len() - final_byte.len_utf8()];
        if params.starts_with('?') || params.contains(':') {
            self.push_mark(raw);
            return;
        }
        let count = params.parse::<usize>().unwrap_or(1).max(1);
        match final_byte {
            'K' => match params {
                "" | "0" => {
                    if self.canvas.len() > self.column {
                        self.canvas.truncate(self.column);
                        self.row_dirty = true;
                    }
                }
                "1" => {
                    let end = self.column.min(self.canvas.len());
                    if end > 0 {
                        self.canvas[..end].fill(' ');
                        self.row_dirty = true;
                    }
                }
                "2" => {
                    if !self.canvas.is_empty() {
                        self.canvas.clear();
                        self.row_dirty = true;
                    }
                }
                _ => self.push_mark(raw),
            },
            'C' => self.column = self.column.saturating_add(count).min(ROW_CANVAS_CHARS),
            'D' => self.move_to(self.column.saturating_sub(count)),
            'G' => self.move_to(count.saturating_sub(1).min(ROW_CANVAS_CHARS)),
            'A' | 'B' | 'J' => {}
            'E' | 'F' | 'H' | 'f' => self.move_to(0),
            _ => self.push_mark(raw),
        }
    }

    fn move_to(&mut self, column: usize) {
        if column < self.column && column < self.canvas.len() {
            self.row_dirty = true;
        }
        self.column = column;
    }
}

/// Decodes carriage-return progress redraws in a complete captured stream.
#[must_use]
pub fn render_terminal(text: &str) -> (String, u64) {
    let mut renderer = RowRenderer::new();
    let mut out = String::with_capacity(text.len().min(64 * 1024));
    renderer.push(text, &mut out);
    renderer.finish(&mut out);
    (out, renderer.redraws())
}

enum Scan {
    Found(usize),
    Incomplete,
}

fn scan_escape(source: &[char], start: usize) -> Scan {
    let Some(introducer) = source.get(start + 1) else {
        return Scan::Incomplete;
    };
    match introducer {
        '[' => {
            let mut idx = start + 2;
            while let Some(ch) = source.get(idx) {
                if ('\u{40}'..='\u{7e}').contains(ch) {
                    return Scan::Found(idx + 1);
                }
                idx += 1;
                if idx - start > MAX_ESCAPE_CHARS {
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
        ']' => {
            let mut idx = start + 2;
            while let Some(ch) = source.get(idx) {
                if *ch == '\u{7}' {
                    return Scan::Found(idx + 1);
                }
                if *ch == '\u{1b}' && source.get(idx + 1) == Some(&'\\') {
                    return Scan::Found(idx + 2);
                }
                idx += 1;
                if idx - start > MAX_OSC_CHARS {
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
        _ => {
            let mut idx = start + 1;
            while let Some(ch) = source.get(idx) {
                if !('\u{20}'..='\u{2f}').contains(ch) {
                    return Scan::Found(idx + 1);
                }
                idx += 1;
                if idx - start > MAX_ESCAPE_CHARS {
                    return Scan::Found(start + 1);
                }
            }
            Scan::Incomplete
        }
    }
}

/// Strips ANSI CSI/OSC/nF escape sequences from `text`, returning `(stripped, removed_count)`.
#[must_use]
pub fn strip_escape_sequences(text: &str) -> (String, u64) {
    let Some(first) = text.find('\u{1b}') else {
        return (text.to_owned(), 0);
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut removed = 0u64;
    let mut rest = &text[first..];

    loop {
        let window: Vec<char> = rest.chars().take(SCAN_WINDOW_CHARS).collect();
        if let Some(bytes) = sequence_bytes(&window) {
            rest = &rest[bytes..];
            removed = removed.saturating_add(1);
        } else {
            out.push('\u{1b}');
            rest = &rest[1..];
        }
        match rest.find('\u{1b}') {
            Some(next) => {
                out.push_str(&rest[..next]);
                rest = &rest[next..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    (out, removed)
}

fn sequence_bytes(window: &[char]) -> Option<usize> {
    let Scan::Found(end) = scan_escape(window, 0) else {
        return None;
    };
    if end <= 1 || !('\u{20}'..='\u{7e}').contains(window.get(1)?) {
        return None;
    }
    Some(window[..end].iter().map(|c| c.len_utf8()).sum())
}

static PERCENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]{1,3}\s?%").expect("valid percent regex"));
static RATIO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]+\s?/\s?[0-9]+").expect("valid ratio regex"));
static RATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[0-9](\.[0-9]+)?\s?(it|ops|req|[kmgt]?i?b)/s").expect("valid rate regex")
});
static ETA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\beta:?\s+[0-9]").expect("valid eta regex"));

/// Collapses runs of >=3 monotonic line-by-line progress frames while preserving final frame.
#[must_use]
pub fn collapse_progress_lines(text: &str) -> (String, u64) {
    if text.is_empty() {
        return (String::new(), 0);
    }
    let lines: Vec<&str> = text.lines().collect();
    let eligible: Vec<bool> = lines.iter().map(|l| progress_signals(l) >= 2).collect();
    let shapes: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if eligible[i] {
                shape_of(l)
            } else {
                String::new()
            }
        })
        .collect();

    let mut out = Vec::with_capacity(lines.len());
    let mut removed = 0u64;
    let mut idx = 0;
    while idx < lines.len() {
        if !eligible[idx] {
            out.push(lines[idx].to_owned());
            idx += 1;
            continue;
        }
        let mut end = idx + 1;
        while end < lines.len() && eligible[end] && shapes[end] == shapes[idx] {
            end += 1;
        }
        let len = end - idx;
        if len < 3 {
            for l in &lines[idx..end] {
                out.push((*l).to_owned());
            }
            idx = end;
            continue;
        }
        let collapsed = len - 1;
        removed += collapsed as u64;
        out.push(format!("... ({collapsed} progress updates collapsed)"));
        out.push(lines[end - 1].to_owned());
        idx = end;
    }

    if removed == 0 {
        return (text.to_owned(), 0);
    }
    let mut rendered = out.join("\n");
    if text.ends_with('\n') {
        rendered.push('\n');
    }
    (rendered, removed)
}

fn is_bar_glyph(ch: char) -> bool {
    matches!(
        ch,
        '=' | '#'
            | '\u{2588}'..='\u{258f}'
            | '\u{2591}'..='\u{2593}'
            | '\u{25a0}'
            | '\u{25ac}'
            | '\u{25cf}'
            | '\u{2801}'..='\u{28ff}'
            | '\u{2500}'..='\u{257f}'
    )
}

fn progress_signals(line: &str) -> usize {
    if !line.bytes().any(|b| b.is_ascii_digit()) {
        return 0;
    }
    let mut count = 0;
    if PERCENT_RE.is_match(line) {
        count += 1;
    }
    if RATIO_RE.is_match(line) {
        count += 1;
    }
    if RATE_RE.is_match(line) {
        count += 1;
    }
    if ETA_RE.is_match(line) {
        count += 1;
    }
    let mut run = 0;
    for ch in line.chars() {
        if is_bar_glyph(ch) {
            run += 1;
            if run >= 3 {
                count += 1;
                break;
            }
        } else {
            run = 0;
        }
    }
    count
}

fn shape_of(line: &str) -> String {
    let mut shape = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            shape.push('#');
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
        } else if ch.is_whitespace() || is_bar_glyph(ch) {
            shape.push(' ');
            while chars
                .peek()
                .copied()
                .is_some_and(|c| c.is_whitespace() || is_bar_glyph(c))
            {
                chars.next();
            }
        } else {
            shape.push(ch);
        }
    }
    shape
}

#[derive(Debug, Deserialize)]
struct RuleDoc {
    #[allow(dead_code)]
    description: String,
    match_command: String,
    #[serde(default)]
    strip_ansi: bool,
    #[serde(default)]
    filter_stderr: bool,
    #[serde(default)]
    strip_lines_matching: Vec<String>,
    #[serde(default)]
    max_lines: Option<usize>,
    #[serde(default)]
    on_empty: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CorpusDoc {
    filters: BTreeMap<String, RuleDoc>,
}

/// Compiled command-aware filter rule.
#[derive(Debug)]
pub struct Rule {
    name: String,
    match_command: Regex,
    strip_ansi: bool,
    #[allow(dead_code)]
    filter_stderr: bool,
    strip_lines_matching: Vec<Regex>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
}

/// Built-in compiled command-filter corpus.
#[derive(Debug)]
pub struct Corpus {
    rules: Vec<Rule>,
}

impl Corpus {
    /// Returns the built-in static [`Corpus`].
    #[must_use]
    pub fn builtin() -> &'static Self {
        static CORPUS: OnceLock<Corpus> = OnceLock::new();
        CORPUS.get_or_init(|| {
            let parsed: CorpusDoc =
                toml::from_str(EMBEDDED_RULES_TOML).expect("valid embedded filter corpus");
            let mut rules = Vec::with_capacity(parsed.filters.len());
            for (name, doc) in parsed.filters {
                let compile_re = |p: &str| {
                    RegexBuilder::new(p)
                        .size_limit(64 * 1024)
                        .build()
                        .expect("valid embedded filter regex")
                };
                rules.push(Rule {
                    name,
                    match_command: compile_re(&doc.match_command),
                    strip_ansi: doc.strip_ansi,
                    filter_stderr: doc.filter_stderr,
                    strip_lines_matching: doc
                        .strip_lines_matching
                        .iter()
                        .map(|s| compile_re(s))
                        .collect(),
                    max_lines: doc.max_lines,
                    on_empty: doc.on_empty,
                });
            }
            Self { rules }
        })
    }

    /// Finds the first matching rule for a normalized command string.
    #[must_use]
    pub fn find(&self, normalized_command: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|r| r.match_command.is_match(normalized_command))
    }
}

/// Result of running the full RTK-inspired output reduction pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredOutput {
    /// Model-facing reduced output text (including `[filtered: ...]` summary when reduced).
    pub text: String,
    /// Whether any redraws, ANSI escapes, progress runs, or lines were reduced.
    pub was_filtered: bool,
    /// Matched rule name when a command-specific rule applied.
    pub matched_rule: Option<String>,
}

/// Decodes CR redraws, strips ANSI escapes, collapses monotonic progress lines, and applies
/// command-aware noise reduction while NEVER reporting non-zero exits as success.
#[must_use]
pub fn filter_command_output(
    command: &str,
    raw_output: &str,
    exit_code: Option<i32>,
    output_id: Option<&str>,
) -> FilteredOutput {
    let raw_lines = raw_output.lines().count();
    let (decoded, redraws) = render_terminal(raw_output);
    let (stripped, escapes_removed) = strip_escape_sequences(&decoded);
    let (collapsed, progress_collapsed) = collapse_progress_lines(&stripped);

    let bounded = bound_input(&collapsed);
    let mut lossy = redraws > 0 || escapes_removed > 0 || progress_collapsed > 0 || bounded.1;
    let mut lines: Vec<String> = bounded.0.lines().map(str::to_owned).collect();

    let normalized_cmd = normalize_command_scope(command);
    let succeeded = exit_code == Some(0);
    let mut matched_rule = None;

    if let Some(rule) = Corpus::builtin().find(&normalized_cmd) {
        matched_rule = Some(rule.name.clone());
        if rule.strip_ansi {
            lines = lines
                .into_iter()
                .map(|l| strip_escape_sequences(&l).0)
                .collect();
        }
        if !rule.strip_lines_matching.is_empty() {
            let before = lines.len();
            lines.retain(|l| !rule.strip_lines_matching.iter().any(|re| re.is_match(l)));
            lossy |= lines.len() != before;
        }
        let cap = rule.max_lines.unwrap_or(FALLBACK_MAX_LINES);
        if lines.len() > cap {
            cap_lines(&mut lines, cap, succeeded);
            lossy = true;
        }
        if succeeded
            && lines.iter().all(|l| l.trim().is_empty())
            && let Some(msg) = &rule.on_empty
        {
            lines = vec![msg.clone()];
            lossy = true;
        }
    } else if lines.len() > FALLBACK_MAX_LINES {
        cap_lines(&mut lines, FALLBACK_MAX_LINES, succeeded);
        lossy = true;
    }

    let kept_lines = lines.len();
    let mut text = lines.join("\n");
    if lossy {
        let mut summary = format!("[filtered: {raw_lines} -> {kept_lines} lines");
        if let Some(rname) = &matched_rule {
            summary.push_str(&format!(", rule={rname}"));
        }
        if redraws > 0 {
            summary.push_str(&format!(", redraws={redraws}"));
        }
        if let Some(oid) = output_id {
            summary.push_str(&format!(", output_id={oid}"));
        }
        summary.push(']');
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&summary);
    }

    FilteredOutput {
        text,
        was_filtered: lossy,
        matched_rule,
    }
}

fn cap_lines(lines: &mut Vec<String>, max: usize, succeeded: bool) {
    let dropped = lines.len().saturating_sub(max);
    if dropped == 0 {
        return;
    }
    if succeeded {
        lines.truncate(max);
        lines.push(format!("... ({dropped} lines truncated)"));
    } else {
        // Failures keep BOTH head and tail so error diagnostics and summaries at the end survive!
        let head = max / 2;
        lines.drain(head..head + dropped);
        lines.insert(head, format!("... ({dropped} lines omitted)"));
    }
}

fn bound_input(source: &str) -> (String, bool) {
    let mut trimmed = false;
    let mut bounded = source;
    if bounded.len() > MAX_INPUT_BYTES {
        let start = bounded.len() - MAX_INPUT_BYTES;
        let start = (start..bounded.len())
            .find(|idx| bounded.is_char_boundary(*idx))
            .unwrap_or(bounded.len());
        bounded = &bounded[start..];
        trimmed = true;
    }
    let count = bounded.lines().count();
    if count > MAX_INPUT_LINES {
        let skip = count - MAX_INPUT_LINES;
        return (
            bounded.lines().skip(skip).collect::<Vec<_>>().join("\n"),
            true,
        );
    }
    (bounded.to_owned(), trimmed)
}

fn normalize_command_scope(command: &str) -> String {
    let Ok(tokens) = shell_words::split(command.trim()) else {
        return command.trim().to_string();
    };
    let mut iter = tokens.into_iter().skip_while(|t| {
        t.contains('=') && !t.starts_with('-') && !t.starts_with('/') && !t.starts_with('.')
    });
    let Some(first) = iter.next() else {
        return String::new();
    };
    let base_prog = std::path::Path::new(&first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&first);
    let mut out = base_prog.to_string();
    for arg in iter {
        out.push(' ');
        out.push_str(&arg);
    }
    out
}
