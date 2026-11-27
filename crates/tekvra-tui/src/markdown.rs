//! Streaming Markdown-to-`ratatui` renderer powered by `pulldown-cmark` and `CodeHighlighter`.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::highlight::{CodeHighlighter, highlight_unified_diff};

/// Renders GitHub-flavored Markdown text (including streaming partial blocks and fenced diffs/code)
/// into styled `ratatui` lines.
#[must_use]
pub fn render_markdown_lines(markdown: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(markdown, opts);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    let mut bold = false;
    let mut italic = false;
    let mut in_heading = false;
    let mut in_blockquote = false;
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();

    let flush_line = |lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            lines.push(Line::from(std::mem::take(spans)));
        }
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    flush_line(&mut lines, &mut current_spans);
                    in_heading = true;
                    current_spans.push(Span::styled(
                        "▍ ".to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                Tag::BlockQuote(_) => {
                    flush_line(&mut lines, &mut current_spans);
                    in_blockquote = true;
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut lines, &mut current_spans);
                    in_code_block = true;
                    code_buf.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => {
                            let s = lang.trim().to_string();
                            (!s.is_empty()).then_some(s)
                        }
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::Item => {
                    flush_line(&mut lines, &mut current_spans);
                    current_spans.push(Span::styled(
                        "  • ".to_string(),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                Tag::Strong => bold = true,
                Tag::Emphasis => italic = true,
                _ => {}
            },
            Event::End(end_tag) => match end_tag {
                TagEnd::Heading(_) => {
                    in_heading = false;
                    flush_line(&mut lines, &mut current_spans);
                }
                TagEnd::Paragraph | TagEnd::Item => {
                    flush_line(&mut lines, &mut current_spans);
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                    flush_line(&mut lines, &mut current_spans);
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    let rendered = if matches!(code_lang.as_deref(), Some("diff" | "patch")) {
                        highlight_unified_diff(&code_buf)
                    } else {
                        CodeHighlighter::global().highlight_block(code_lang.as_deref(), &code_buf)
                    };
                    for mut l in rendered {
                        l.spans.insert(
                            0,
                            Span::styled("  │ ".to_string(), Style::default().fg(Color::DarkGray)),
                        );
                        lines.push(l);
                    }
                    code_lang = None;
                    code_buf.clear();
                }
                TagEnd::Strong => bold = false,
                TagEnd::Emphasis => italic = false,
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else {
                    let mut style = Style::default();
                    if in_heading {
                        style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                    } else if in_blockquote {
                        style = style.fg(Color::Gray).add_modifier(Modifier::ITALIC);
                    }
                    if bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }

                    for (i, seg) in text.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut lines, &mut current_spans);
                            if in_blockquote {
                                current_spans.push(Span::styled(
                                    "  ▎ ".to_string(),
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                        } else if current_spans.is_empty() && in_blockquote {
                            current_spans.push(Span::styled(
                                "  ▎ ".to_string(),
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        if !seg.is_empty() {
                            current_spans.push(Span::styled(seg.to_string(), style));
                        }
                    }
                }
            }
            Event::Code(inline_code) => {
                current_spans.push(Span::styled(
                    format!("`{inline_code}`"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" ".to_string()));
            }
            Event::HardBreak => {
                flush_line(&mut lines, &mut current_spans);
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                current_spans.push(Span::styled(
                    marker.to_string(),
                    Style::default().fg(if checked { Color::Green } else { Color::Yellow }),
                ));
            }
            _ => {}
        }
    }

    flush_line(&mut lines, &mut current_spans);
    if lines.is_empty() && !markdown.is_empty() {
        lines.push(Line::from(Span::raw(markdown.to_string())));
    }
    lines
}
