//! Modal wizard overlays for creating projects, adding services, and renaming.
//!
//! Each [`WizardState`] variant maps to a rendered overlay. Simple text inputs
//! use [`render_text_input`]; the path-entry step uses [`render_path_input`]
//! which shows live filesystem completions below the cursor; the command-picker
//! step uses [`render_command_list`] for a keyboard-navigable selection list.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{AppState, WizardState};
use crate::core::process::ProcessStatus;
use crate::core::scanner::DetectedCommand;

pub fn render_wizard(f: &mut Frame, app: &AppState) {
    let state = &app.wizard;
    match state {
        WizardState::Inactive => {}

        WizardState::AddProjectName { input } => {
            render_text_input(
                f,
                " new project ",
                "project name",
                input.as_str(),
                None,
                "[enter] confirm  [esc] cancel",
            );
        }

        WizardState::AddProjectDomain { name, input } => {
            let helper_installed =
                std::path::Path::new(crate::core::hosts::HELPER_PATH).exists();
            let footer = if helper_installed {
                "[enter] confirm  [esc] back".to_string()
            } else {
                "[enter] confirm  [esc] back  — will prompt for password to update /etc/hosts".to_string()
            };
            let hint = format!("e.g. {}.app, {}.local, {}.dev", name.to_lowercase().replace(' ', "-"), name.to_lowercase().replace(' ', "-"), name.to_lowercase().replace(' ', "-"));
            render_text_input(
                f,
                " domain ",
                "domain",
                input.as_str(),
                Some(&hint),
                &footer,
            );
        }

        WizardState::GeneratingCerts { domain } => {
            let msg = format!("🔐 generating certs for {}...", domain);
            render_message(f, " ssl ", &msg);
        }

        WizardState::AddServicePath { project, input, completions } => {
            let title = format!(" add service to {} ", project);
            render_path_input(f, &title, input.as_str(), completions.as_slice());
        }

        WizardState::AddServiceName {
            input,
            suggested,
            path,
            ..
        } => {
            let folder = std::path::Path::new(path.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str());
            let hint = format!("folder: {}  →  suggested: {}", folder, suggested);
            render_text_input(
                f,
                " service name ",
                "name",
                input.as_str(),
                Some(&hint),
                "[enter] confirm  [esc] back",
            );
        }

        WizardState::AddServiceCommand {
            commands, selected, ..
        } => {
            render_command_list(f, commands.as_slice(), *selected);
        }

        WizardState::AddServiceSubdomain {
            input,
            project_domain,
            ..
        } => {
            // Show the resolved domain as a live preview
            let resolved = if input.is_empty() {
                project_domain.clone()
            } else if input.contains('.') {
                input.clone()
            } else {
                format!("{}.{}", input, project_domain)
            };
            let hint = format!("→ {}", resolved);
            render_text_input(
                f,
                " domain ",
                "domain",
                input.as_str(),
                Some(&hint),
                "[enter] confirm  [esc] back  (full domain or subdomain)",
            );
        }

        WizardState::CustomCommand { input, .. } => {
            render_text_input(
                f,
                " custom command ",
                "command",
                input.as_str(),
                Some("e.g. node server.js  or  python app.py"),
                "[enter] confirm  [esc] back",
            );
        }

        WizardState::RenameProject { input, .. } => {
            render_text_input(
                f,
                " rename project ",
                "new name",
                input.as_str(),
                None,
                "[enter] confirm  [esc] cancel",
            );
        }

        WizardState::RenameService { input, old_name, .. } => {
            let hint = format!("current name: {}", old_name);
            render_text_input(
                f,
                " rename service ",
                "new name",
                input.as_str(),
                Some(&hint),
                "[enter] confirm  [esc] cancel",
            );
        }

        WizardState::ServiceMenu { project_idx, service_idx } => {
            render_service_menu(f, app, *project_idx, *service_idx);
        }

        WizardState::ConfirmDelete { display_name, .. } => {
            render_confirm_delete(f, display_name.as_str());
        }
    }
}

// ── Render helpers ─────────────────────────────────────────────────────────────

fn render_path_input(f: &mut Frame, title: &str, input: &str, completions: &[String]) {
    let comp_count = completions.len().min(8) as u16;
    // height: border(2) + margin(2) + input(1) + gap(1) + completions + footer(1)
    let height = 7 + if comp_count > 0 { comp_count + 1 } else { 0 };
    let area = centered_rect(70, height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut constraints = vec![
        Constraint::Length(1), // input line
        Constraint::Length(1), // hint
    ];
    if comp_count > 0 {
        constraints.push(Constraint::Length(1)); // spacer
        for _ in 0..comp_count {
            constraints.push(Constraint::Length(1));
        }
    }
    constraints.push(Constraint::Min(0)); // filler
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(constraints)
        .split(inner);

    // Input line
    let input_line = Line::from(vec![
        Span::styled("  path:  ", Style::default().fg(Color::DarkGray)),
        Span::styled(input, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(input_line), chunks[0]);

    // Static hint
    f.render_widget(
        Paragraph::new("  absolute or ~/relative path").style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );

    // Completions
    if comp_count > 0 {
        // chunks[2] is spacer (blank), chunks[3..3+comp_count] are completion rows
        for (i, completion) in completions.iter().enumerate().take(comp_count as usize) {
            // Show only the last two path components for readability
            let display = abbreviated_path(completion);
            f.render_widget(
                Paragraph::new(format!("  {}", display))
                    .style(Style::default().fg(Color::Blue)),
                chunks[3 + i],
            );
        }
    }

    // Footer
    let footer_idx = chunks.len() - 1;
    f.render_widget(
        Paragraph::new("  [tab] complete  [enter] confirm  [esc] cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[footer_idx],
    );
}

fn abbreviated_path(path: &str) -> String {
    // Replace home dir with ~ for display
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let p = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    p
}

fn render_text_input(
    f: &mut Frame,
    title: &str,
    field_label: &str,
    input: &str,
    hint: Option<&str>,
    footer: &str,
) {
    let height = if hint.is_some() { 9 } else { 7 };
    let area = centered_rect(60, height, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(if hint.is_some() {
            vec![
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ]
        } else {
            vec![
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ]
        })
        .split(inner);

    let label_idx = 0;
    let hint_idx_opt = if hint.is_some() { Some(2usize) } else { None };
    let footer_idx = if hint.is_some() { 4 } else { 3 };

    // Field label + cursor
    let input_line = Line::from(vec![
        Span::styled(
            format!("  {}:  ", field_label),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(input, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(input_line), chunks[label_idx]);

    if let (Some(h), Some(idx)) = (hint, hint_idx_opt) {
        let hint_line = Paragraph::new(format!("  {}", h))
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(hint_line, chunks[idx]);
    }

    let footer_line = Paragraph::new(format!("  {}", footer))
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer_line, chunks[footer_idx]);
}

fn render_command_list(f: &mut Frame, commands: &[DetectedCommand], selected: usize) {
    let height = (commands.len() as u16 + 6).min(20);
    let area = centered_rect(64, height, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" select start command ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let is_sel = i == selected;
            let cursor = if is_sel { "▶ " } else { "  " };

            let label_style = if cmd.command.is_empty() {
                // "enter custom command..." option
                Style::default().fg(Color::DarkGray)
            } else if is_sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let mut spans = vec![
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
                Span::styled(cmd.label.clone(), label_style),
            ];

            if cmd.recommended {
                spans.push(Span::styled(
                    "  (recommended)",
                    Style::default().fg(Color::Green),
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn render_service_menu(f: &mut Frame, app: &AppState, project_idx: usize, service_idx: usize) {
    let pv = match app.projects.get(project_idx) {
        Some(p) => p,
        None => return,
    };
    let proc = match pv.processes.get(service_idx) {
        Some(p) => p,
        None => return,
    };

    let svc_name = proc.id.split('/').last().unwrap_or(&proc.id);
    let is_running = proc.status == ProcessStatus::Running;

    let title = format!(" {} ", svc_name);
    let area = centered_rect(44, 11, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<(&str, &str, Color)> = vec![
        ("[s]", "start",   if is_running { Color::DarkGray } else { Color::Green }),
        ("[r]", "restart", Color::Yellow),
        ("[x]", "stop",    if is_running { Color::Red } else { Color::DarkGray }),
        ("[l]", "logs",    Color::Blue),
        ("[e]", "rename",  Color::White),
        ("[d]", "delete",  Color::Red),
    ];

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            items.iter().map(|_| Constraint::Length(1))
                .chain(std::iter::once(Constraint::Min(0)))
                .chain(std::iter::once(Constraint::Length(1)))
                .collect::<Vec<_>>()
        )
        .split(inner);

    for (i, (key, label, color)) in items.iter().enumerate() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("  {}", key), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  {}", label), Style::default().fg(*color)),
            ])),
            rows[i],
        );
    }

    f.render_widget(
        Paragraph::new("  [esc] close")
            .style(Style::default().fg(Color::DarkGray)),
        rows[items.len() + 1],
    );
}

fn render_confirm_delete(f: &mut Frame, display_name: &str) {
    let area = centered_rect(60, 7, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" delete ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Red));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("  Delete {}?", display_name))
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new("  Running services will be stopped.")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new("  [y] confirm delete  [n / esc] cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

fn render_message(f: &mut Frame, title: &str, msg: &str) {
    let area = centered_rect(52, 5, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let para = Paragraph::new(format!("  {}", msg))
        .style(Style::default().fg(Color::White))
        .alignment(Alignment::Left);
    f.render_widget(para, inner);
}

/// Returns a centered rect of fixed height and percentage width
fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let popup_width = (area.width * width_pct / 100).min(area.width.saturating_sub(4));
    let popup_x = (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = (area.height.saturating_sub(height)) / 2;

    Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height,
    }
}
