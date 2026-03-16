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

pub fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    let visible = area.height.saturating_sub(2) as usize; // minus top/bottom borders
    let logs = get_selected_logs(state, visible);
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

fn get_selected_logs(state: &AppState, visible: usize) -> Vec<String> {
    if let Some(proj) = state.projects.get(state.selected_project) {
        let svc_idx = state.selected_service.unwrap_or(0);
        if let Some(proc) = proj.processes.get(svc_idx) {
            if proc.combined_log.is_empty() {
                return vec!["  waiting for output…".to_string()];
            }
            // Chronological order: oldest at top, newest at bottom — like a terminal.
            // log_scroll is lines scrolled UP from the bottom (0 = show newest).
            let logs: Vec<String> = proc.combined_log.iter().cloned().collect();
            let total = logs.len();
            let end = total.saturating_sub(state.log_scroll);
            let start = end.saturating_sub(visible);
            return logs[start..end].to_vec();
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
