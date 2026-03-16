use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::AppState;

pub fn render_command(f: &mut Frame, area: Rect, state: &AppState) {
    let content = if state.command_focused {
        format!("> {}_", state.command_input)
    } else {
        format!("  {}", state.command_input)
    };

    let style = if state.command_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ask run.dev ")
        .style(style);

    let para = Paragraph::new(content).block(block);
    f.render_widget(para, area);
}
