//! Permissive-only `syntect` syntax highlighter (MIT/BSD default syntaxes only; zero MPL-2.0 assets)
//! and unified diff styling for `ratatui`.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Cached permissive syntax highlighter.
pub struct CodeHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl CodeHighlighter {
    /// Returns the shared process-wide [`CodeHighlighter`] loaded from `syntect`'s permissive built-in pack.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<CodeHighlighter> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let syntax_set = SyntaxSet::load_defaults_newlines();
            let themes = ThemeSet::load_defaults();
            let theme = themes
                .themes
                .get("base16-ocean.dark")
                .or_else(|| themes.themes.values().next())
                .cloned()
                .unwrap_or_default();
            Self { syntax_set, theme }
        })
    }

    /// Returns `true` if any copyleft/excluded syntax (such as MPL-2.0 `Rhai`) is present.
    #[must_use]
    pub fn contains_excluded_syntax(&self) -> bool {
        self.syntax_set
            .syntaxes()
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case("rhai"))
    }

    /// Highlights `code` for `lang_token` (e.g., `"rust"`, `"rs"`, `"python"`, `"json"`, `"sh"`)
    /// into styled `ratatui` lines.
    #[must_use]
    pub fn highlight_block(&self, lang_token: Option<&str>, code: &str) -> Vec<Line<'static>> {
        let syntax = lang_token
            .and_then(|tok| {
                let clean = tok.trim();
                self.syntax_set
                    .find_syntax_by_token(clean)
                    .or_else(|| self.syntax_set.find_syntax_by_extension(clean))
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();
        for line in LinesWithEndings::from(code) {
            let trimmed = line.trim_end_matches(['\r', '\n']);
            match h.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let spans: Vec<Span<'static>> = ranges
                        .into_iter()
                        .map(|(st, piece)| {
                            let piece_clean = piece.trim_end_matches(['\r', '\n']).to_string();
                            let fg = Color::Rgb(st.foreground.r, st.foreground.g, st.foreground.b);
                            let mut style = Style::default().fg(fg);
                            if st.font_style.contains(FontStyle::BOLD) {
                                style = style.add_modifier(Modifier::BOLD);
                            }
                            if st.font_style.contains(FontStyle::ITALIC) {
                                style = style.add_modifier(Modifier::ITALIC);
                            }
                            Span::styled(piece_clean, style)
                        })
                        .filter(|s| !s.content.is_empty())
                        .collect();
                    lines.push(Line::from(spans));
                }
                Err(_) => {
                    lines.push(Line::from(Span::raw(trimmed.to_string())));
                }
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::raw(String::new())));
        }
        lines
    }
}

/// Renders a unified diff string with green additions, red deletions, and cyan hunk headers.
#[must_use]
pub fn highlight_unified_diff(diff: &str) -> Vec<Line<'static>> {
    diff.lines()
        .map(|line| {
            if line.starts_with("+++") || line.starts_with("---") {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if line.starts_with("@@") {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Cyan),
                ))
            } else if line.starts_with('+') {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Green),
                ))
            } else if line.starts_with('-') {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Red),
                ))
            } else {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Gray),
                ))
            }
        })
        .collect()
}
