//! UI rendering — dashboard, log panel, and wizard overlays.
//!
//! The top-level [`render`] function splits the terminal area and delegates to
//! the three sub-modules: [`dashboard`] for the main project/service list,
//! [`logs`] for the live log panel, and [`wizard`] for modal input overlays.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

pub mod dashboard;
pub mod logs;
pub mod wizard;

use ratatui::{
    layout::{Constraint, Direction, Layout},
    Frame,
};

use crate::app::AppState;
use dashboard::render_dashboard;
use logs::render_logs;
use wizard::render_wizard;

pub fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    state.row_map.clear();

    if state.log_panel_open {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        render_dashboard(f, chunks[0], state);
        render_logs(f, chunks[1], state);
    } else {
        render_dashboard(f, area, state);
    }

    // Wizard renders as overlay on top of everything
    if state.wizard.is_active() {
        render_wizard(f, state);
    }
}
