//! Log panel — scrollable live output for the selected service.
//!
//! Renders the last N lines from the selected service's stderr/stdout ring buffer.
//! Lines are coloured by severity (error keywords in red, warnings in yellow,
//! everything else in the default terminal colour) so issues stand out at a glance.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use crate::app::AppState;

/// Strip ANSI escape sequences from a string so raw process output
/// doesn't interfere with ratatui's own rendering.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume the escape sequence
            if let Some(next) = chars.next() {
                if next == '[' {
                    // CSI sequence: eat until we hit a letter (0x40–0x7E)
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == 'm' {
                            break;
                        }
                    }
                }
                // OSC or other sequences — skip the next char and move on
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    let inner_width = area.width.saturating_sub(2) as usize; // minus left/right borders
    let visible = area.height.saturating_sub(2) as usize; // minus top/bottom borders
    let logs = get_selected_logs(state, visible, inner_width);
    let title = get_selected_title(state);

    let items: Vec<ListItem> = logs
        .iter()
        .map(|line| {
            let color = if line.contains("error") || line.contains("Error") || line.contains("ERROR") {
                Color::Red
            } else if line.contains("warn") || line.contains("Warn") || line.contains("WARN") {
                Color::Yellow
            } else {
                Color::Gray
            };
            ListItem::new(Line::from(Span::styled(line.clone(), Style::default().fg(color))))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" logs: {}  [j/k] scroll  [l] close ", title))
            .style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(list, area);
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
