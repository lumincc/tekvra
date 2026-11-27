//! Native Rust `ToolOutput` card renderer with collapsible bodies and Raw/Filtered terminal toggling.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tekvra_protocol::ToolOutput;

use crate::highlight::highlight_unified_diff;
use crate::markdown::render_markdown_lines;

/// Interactive display state for a single native tool card in the transcript.
#[derive(Debug, Clone)]
pub struct ToolCardState {
    /// Tool call item id.
    pub item_id: String,
    /// Tool name (`apply_patch`, `exec_command`, `file_read`, etc.).
    pub tool_name: String,
    /// Native structured output payload.
    pub output: ToolOutput,
    /// Whether the card body is collapsed to its single-line header.
    pub collapsed: bool,
    /// Whether `ToolOutput::Terminal` displays unfiltered `raw_output` (`true`) or `filtered_output` (`false`).
    pub show_raw_output: bool,
}

impl ToolCardState {
    /// Creates an expanded [`ToolCardState`] defaulting to filtered terminal output.
    #[must_use]
    pub fn new(
        item_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: ToolOutput,
    ) -> Self {
        Self {
            item_id: item_id.into(),
            tool_name: tool_name.into(),
            output,
            collapsed: false,
            show_raw_output: false,
        }
    }

    /// Toggles Raw vs Filtered terminal view.
    pub const fn toggle_raw_output(&mut self) {
        self.show_raw_output = !self.show_raw_output;
    }

    /// Toggles card collapse state.
    pub const fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
    }
}

/// Renders a [`ToolCardState`] into styled `ratatui` lines without invoking Lua.
#[must_use]
pub fn render_tool_card(card: &ToolCardState, selected: bool) -> Vec<Line<'static>> {
    let border_color = if selected {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let fold_glyph = if card.collapsed { "▸" } else { "▾" };
    let mut lines = Vec::new();

    match &card.output {
        ToolOutput::Terminal {
            command,
            exit_code,
            filtered_output,
            raw_output,
            was_filtered,
            session_id,
            ..
        } => {
            let status_span = match exit_code {
                Some(0) => Span::styled(" [exit 0]", Style::default().fg(Color::Green)),
                Some(c) => Span::styled(
                    format!(" [exit {c}]"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                None => Span::styled(
                    format!(" [running {}]", session_id.as_deref().unwrap_or("pty")),
                    Style::default().fg(Color::Yellow),
                ),
            };
            let mode_badge = if *was_filtered {
                if card.show_raw_output {
                    " [VIEW: RAW | r: Filtered]"
                } else {
                    " [VIEW: FILTERED | r: Raw]"
                }
            } else {
                " [UNFILTERED]"
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{fold_glyph} $ {command}"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                status_span,
                Span::styled(mode_badge.to_string(), Style::default().fg(Color::Cyan)),
            ]));

            if !card.collapsed {
                let body = if card.show_raw_output {
                    raw_output.as_str()
                } else {
                    filtered_output.as_str()
                };
                for l in body.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ".to_string(), Style::default().fg(border_color)),
                        Span::raw(l.to_string()),
                    ]));
                }
            }
        }
        ToolOutput::Diff { files, snapshot_id } => {
            let snap_badge = snapshot_id
                .as_deref()
                .map(|s| format!(" (snapshot {s})"))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{fold_glyph} [{}] {} file(s) changed",
                        card.tool_name,
                        files.len()
                    ),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(snap_badge, Style::default().fg(Color::DarkGray)),
            ]));
            if !card.collapsed {
                for f in files {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  │ {:?} {} (+{} -{})",
                            f.change_kind,
                            f.path.display(),
                            f.additions,
                            f.deletions
                        ),
                        Style::default().fg(Color::Yellow),
                    )));
                    for mut dl in highlight_unified_diff(&f.unified_diff) {
                        dl.spans.insert(
                            0,
                            Span::styled("  │   ".to_string(), Style::default().fg(border_color)),
                        );
                        lines.push(dl);
                    }
                }
            }
        }
        ToolOutput::Plan { markdown } => {
            lines.push(Line::from(Span::styled(
                format!("{fold_glyph} [plan] Session Plan Updated"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            if !card.collapsed {
                for mut ml in render_markdown_lines(markdown) {
                    ml.spans.insert(
                        0,
                        Span::styled("  │ ".to_string(), Style::default().fg(border_color)),
                    );
                    lines.push(ml);
                }
            }
        }
        ToolOutput::SubagentCard {
            task_id,
            role,
            status,
            summary,
            worktree_branch,
            diff_summary,
        } => {
            let mut head = format!("{fold_glyph} [task {task_id}] role={role} status={status}");
            if let Some(branch) = worktree_branch {
                head.push_str(&format!(" branch={branch}"));
            }
            lines.push(Line::from(Span::styled(
                head,
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )));
            if !card.collapsed && !summary.is_empty() {
                for l in summary.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ".to_string(), Style::default().fg(border_color)),
                        Span::raw(l.to_string()),
                    ]));
                }
            }
            if !card.collapsed
                && let Some(diff) = diff_summary
                && !diff.is_empty()
            {
                lines.push(Line::from(Span::styled(
                    "  │ worktree changes:".to_string(),
                    Style::default().fg(border_color),
                )));
                for l in diff.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ".to_string(), Style::default().fg(border_color)),
                        Span::raw(l.to_string()),
                    ]));
                }
            }
        }
        other => {
            lines.push(Line::from(Span::styled(
                format!("{fold_glyph} [{}]", card.tool_name),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            if !card.collapsed {
                for l in other.to_model_text().lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ".to_string(), Style::default().fg(border_color)),
                        Span::raw(l.to_string()),
                    ]));
                }
            }
        }
    }

    lines
}
