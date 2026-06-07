//! Log panel — scrollable live output for the selected service.
//!
//! Renders the last N lines from the selected service's stderr/stdout ring buffer.
//! Lines are coloured by severity (error keywords in red, warnings in yellow,
//! everything else in the default terminal colour) so issues stand out at a glance.
//!
//! Because the dashboard captures the mouse (the terminal can't do native
//! selection), text selection is implemented here: the controller records a
//! [`LogSelection`][crate::app::LogSelection] from drag events and this renderer
//! draws the highlight. The visible lines and panel geometry are stashed back
//! into [`AppState`] each frame so a release can be turned into copied text.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use crate::app::AppState;
use crate::core::text::strip_ansi;

pub fn render_logs(f: &mut Frame, area: Rect, state: &mut AppState) {
    let inner_width = area.width.saturating_sub(2) as usize; // minus left/right borders
    let visible = area.height.saturating_sub(2) as usize; // minus top/bottom borders

    // Stash geometry + the exact lines on screen so mouse handlers can turn a
    // drag into a (row, col) selection and then into copied text.
    state.log_panel_area = Some(area);
    let logs = get_selected_logs(state, visible, inner_width);
    state.log_visible_lines = logs.clone();

    let title = get_selected_title(state);
    let selection = state.log_selection;

    let items: Vec<ListItem> = logs
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let base = severity_color(line);
            let sel = selection.and_then(|s| s.range_for_row(row as u16, line.chars().count()));
            ListItem::new(styled_log_line(line, base, sel))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " logs: {}  [drag] select  [c] copy all  [j/k] scroll  [l] close ",
                title
            ))
            .style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(list, area);
}

/// Severity colour for a log line based on error/warn keywords.
fn severity_color(line: &str) -> Color {
    if line.contains("error") || line.contains("Error") || line.contains("ERROR") {
        Color::Red
    } else if line.contains("warn") || line.contains("Warn") || line.contains("WARN") {
        Color::Yellow
    } else {
        Color::Gray
    }
}

/// Build a log line, reverse-highlighting the selected `[start, end)` char range.
fn styled_log_line(line: &str, base: Color, sel: Option<(usize, usize)>) -> Line<'static> {
    let (start, end) = match sel {
        Some((s, e)) if e > s => (s, e),
        _ => return Line::from(Span::styled(line.to_string(), Style::default().fg(base))),
    };

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    let slice = |r: std::ops::Range<usize>| -> String { chars[r].iter().collect() };

    if start > 0 {
        spans.push(Span::styled(slice(0..start), Style::default().fg(base)));
    }
    spans.push(Span::styled(
        slice(start..end),
        Style::default().fg(base).add_modifier(Modifier::REVERSED),
    ));
    if end < chars.len() {
        spans.push(Span::styled(slice(end..chars.len()), Style::default().fg(base)));
    }
    Line::from(spans)
}

/// Wrap long lines to fit within `max_width`, then return the last `visible` wrapped lines.
fn get_selected_logs(state: &AppState, visible: usize, max_width: usize) -> Vec<String> {
    if let Some(proj) = state.projects.get(state.selected_project) {
        let svc_idx = state.selected_service.unwrap_or(0);
        if let Some(proc) = proj.processes.get(svc_idx) {
            if proc.combined_log.is_empty() {
                return vec!["  waiting for output…".to_string()];
            }
            // Strip ANSI codes and wrap each log line to the panel width
            let width = if max_width > 0 { max_width } else { 120 };
            let mut wrapped: Vec<String> = Vec::new();
            for raw in proc.combined_log.iter() {
                let clean = strip_ansi(raw);
                let chars: Vec<char> = clean.chars().collect();
                if chars.len() <= width {
                    wrapped.push(clean);
                } else {
                    // Split at char boundaries
                    for chunk in chars.chunks(width) {
                        wrapped.push(chunk.iter().collect());
                    }
                }
            }
            let total = wrapped.len();
            let end = total.saturating_sub(state.log_scroll);
            let start = end.saturating_sub(visible);
            return wrapped[start..end].to_vec();
        }
    }
    vec!["  no logs available".to_string()]
}

fn get_selected_title(state: &AppState) -> String {
    if let Some(proj) = state.projects.get(state.selected_project) {
        let svc_idx = state.selected_service.unwrap_or(0);
        if let Some(proc) = proj.processes.get(svc_idx) {
            return proc.id.clone();
        }
    }
    "none".to_string()
}
