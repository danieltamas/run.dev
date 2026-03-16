//! Main dashboard — banner, project list, and command bar.
//!
//! Renders the full-screen project/service overview: a persistent banner at
//! the top (ASCII art + live status + key tips), an expandable project tree
//! with live status dots and resource stats, and an AI command bar at the bottom.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::app::{AppState, ProjectView, RowInfo};
use crate::ai::mood::{crash_message, CrashInfo};
use crate::core::config::resolve_domain;
use crate::core::process::ProcessStatus;
use crate::core::resources::{format_bytes, format_cpu};
use crate::core::ssl::cert_exists;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// Banner height = 2 border + 14 inner rows (11 art/tips + 3 support section)
const BANNER_HEIGHT: u16 = 16;

const ART: &[&str] = &[
    " ██████╗ ██╗   ██╗███╗   ██╗   ██████╗ ███████╗██╗   ██╗",
    " ██╔══██╗██║   ██║████╗  ██║   ██╔══██╗██╔════╝██║   ██║",
    " ██████╔╝██║   ██║██╔██╗ ██║   ██║  ██║█████╗  ██║   ██║",
    " ██╔══██╗██║   ██║██║╚██╗██║██╗██║  ██║██╔══╝  ╚██╗ ██╔╝",
    " ██║  ██║╚██████╔╝██║ ╚████║╚═╝██████╔╝███████╗ ╚████╔╝ ",
    " ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═════╝ ╚══════╝  ╚═══╝  ",
];

pub fn render_dashboard(f: &mut Frame, area: Rect, state: &mut AppState) {
    // Always use a stable 4-slot layout. The error bar slot is always reserved
    // (1 line) so the layout never changes shape — zero-length constraints in
    // ratatui can corrupt coordinate calculations for subsequent chunks.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(BANNER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(1), // Error bar (blank when no error)
            Constraint::Length(2), // AI command bar
        ])
        .split(area);

    render_banner(f, chunks[0], state);
    render_projects(f, chunks[1], state);
    if state.error_message.is_some() {
        render_error_bar(f, chunks[2], state);
    }
    render_command_bar(f, chunks[3], state);
}

fn render_error_bar(f: &mut Frame, area: Rect, state: &AppState) {
    let msg = state.error_message.as_deref().unwrap_or("");
    let text = format!(" ⚠  {}  [esc to dismiss]", msg);
    f.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

// ── Banner ─────────────────────────────────────────────────────────────────────

fn render_banner(f: &mut Frame, area: Rect, state: &AppState) {
    let mood = &state.mood;
    let total: usize = state.projects.iter().map(|p| p.processes.len()).sum();
    let running: usize = state
        .projects
        .iter()
        .flat_map(|p| &p.processes)
        .filter(|p| p.status == ProcessStatus::Running)
        .count();

    let block = Block::default()
        .title(format!(" run.dev v{} ", VERSION))
        .title_bottom(Line::from(Span::styled(
            " © 2025 Daniel Tamas — danieltamas.com ",
            Style::default().fg(Color::DarkGray),
        )))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Outer vertical split: top 3-column section + full-width support section (msg + link)
    let outer_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // art + tips
            Constraint::Length(3), // support section: msg + blank + link
        ])
        .split(inner);

    // Three columns: art (fixed) | spacer (fills terminal width) | tips (fixed right)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(57), // ASCII art
            Constraint::Min(0),     // responsive spacer
            Constraint::Length(46), // tips — right-aligned
        ])
        .split(outer_rows[0]);

    // ── Left column ────────────────────────────────────────────────────────────
    // Rows: empty | art×6 | empty | name | email | filler
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // empty
            Constraint::Length(1), // art 1
            Constraint::Length(1), // art 2
            Constraint::Length(1), // art 3
            Constraint::Length(1), // art 4
            Constraint::Length(1), // art 5
            Constraint::Length(1), // art 6
            Constraint::Length(1), // empty
            Constraint::Length(1), // name
            Constraint::Length(1), // email
            Constraint::Min(0),
        ])
        .split(cols[0]);

    for (i, line) in ART.iter().enumerate() {
        f.render_widget(
            Paragraph::new(*line)
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            left[i + 1],
        );
    }

    f.render_widget(
        Paragraph::new("Daniel Tamas")
            .style(Style::default().add_modifier(Modifier::DIM)),
        left[8],
    );
    f.render_widget(
        Paragraph::new("hello@danieltamas.com")
            .style(Style::default().add_modifier(Modifier::DIM)),
        left[9],
    );
    let (msg, price, chars_shown, is_typing) = support_message();
    let displayed: String = msg.chars().take(chars_shown).collect();
    let pink = Color::Rgb(255, 20, 147);
    let link_color = Color::Rgb(200, 16, 115);

    let line1 = if is_typing {
        // Typewriter: show partial text + blinking cursor, no price yet
        Line::from(vec![
            Span::styled(format!(" {}", displayed), Style::default().fg(pink)),
            Span::styled("▌", Style::default().fg(pink)),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!(" {} — {} USDC", msg, price), Style::default().fg(pink)),
        ])
    };
    let line2 = if is_typing {
        Line::from(vec![])
    } else {
        Line::from(vec![
            Span::styled(
                format!(" → dani.fkey.id/?amount={}&token=USDC&chain=base", price),
                Style::default().fg(link_color).add_modifier(Modifier::UNDERLINED),
            ),
        ])
    };
    f.render_widget(
        Paragraph::new(Text::from(vec![line1, line2])),
        outer_rows[1],
    );

    // ── Right column ───────────────────────────────────────────────────────────
    // Rows: empty | status(2, wraps) | empty | tip×4 | empty | count+version
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // [0] empty
            Constraint::Length(2), // [1] status line (2 rows, wraps)
            Constraint::Length(1), // [2] empty
            Constraint::Length(1), // [3] tip: add / new / rename
            Constraint::Length(1), // [4] tip: start / stop / pause
            Constraint::Length(1), // [5] tip: restart / logs / delete
            Constraint::Length(1), // [6] tip: ask AI / quit
            Constraint::Length(1), // [7] empty
            Constraint::Length(1), // [8] proj count + version
            Constraint::Min(0),
        ])
        .split(cols[2]);

    // Status line
    let status = if total == 0 {
        Line::from(Span::styled(
            "no services yet",
            Style::default().add_modifier(Modifier::DIM),
        ))
    } else {
        Line::from(vec![
            Span::styled(
                format!("{}/{}", running, total),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" services  ", Style::default()),
            Span::raw(mood.emoji()),
            Span::styled(
                format!("  {}", mood.message("")),
                Style::default().fg(Color::White),
            ),
        ])
    };
    f.render_widget(Paragraph::new(status).wrap(Wrap { trim: true }), right[1]);

    // Key tips
    let dim = Style::default().fg(Color::DarkGray);
    f.render_widget(
        Paragraph::new("[a] add  [n] new project  [e] rename").style(dim),
        right[3],
    );
    f.render_widget(
        Paragraph::new("[s] start  [x] stop  [p] pause").style(dim),
        right[4],
    );
    f.render_widget(
        Paragraph::new("[r] restart  [l] logs  [t] shell").style(dim),
        right[5],
    );
    f.render_widget(
        Paragraph::new("[d] delete  [/] ask AI  [q] quit").style(dim),
        right[6],
    );

    // Project count + version
    let proj_count = state.projects.len();
    let count_line = if proj_count == 0 {
        Line::from(vec![
            Span::styled("no projects yet", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                format!("  v{}", VERSION),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!("{}", proj_count),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " project{}  configured",
                if proj_count == 1 { "" } else { "s" }
            )),
            Span::styled(
                format!("  v{}", VERSION),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    };
    f.render_widget(Paragraph::new(count_line), right[8]);
}

// ── Support messages ───────────────────────────────────────────────────────────

/// Returns `(msg, price, chars_to_show, is_typing)` for the animated support row.
/// Message rotates every 10 seconds and is revealed with a typewriter effect.
fn support_message() -> (&'static str, &'static str, usize, bool) {
    const ITEMS: &[(&str, &str)] = &[
        ("BUY ME a mass spectrometer from 1987",                       "42.00"),
        ("BUY ME a mass-produced samurai sword from alibaba",          "9.99"),
        ("BUY ME a used roomba with trust issues",                     "9.99"),
        ("BUY ME a 55-gallon drum of lube (it's for the servers)",     "69.00"),
        ("BUY ME a nokia 3310 (for emotional support)",                "9.99"),
        ("BUY ME a life-size cardboard cutout of linus torvalds",      "13.37"),
        ("BUY ME an industrial cheese wheel",                          "19.99"),
        ("BUY ME a decommissioned stop sign",                          "11.00"),
        ("BUY ME a bluetooth-enabled ouija board",                     "19.19"),
        ("BUY ME a used oscilloscope with existential dread",          "11.11"),
        ("BUY ME a vintage 56k modem for nostalgia reasons",           "14.40"),
        ("BUY ME a 3d-printed tyrannosaurus femur",                    "18.88"),
        ("BUY ME a taxidermied squirrel in business casual",           "14.99"),
        ("BUY ME a fog machine for the standup meeting",               "24.99"),
        ("BUY ME a rotary phone that makes me feel important",         "31.41"),
        ("BUY ME a nasa surplus o-ring (unused, obviously)",           "27.18"),
        ("BUY ME a bag of resistors i'll never use",                   "12.34"),
        ("BUY ME a hammer for extremely light tapping",                "9.99"),
        ("BUY ME a fax service subscription (fully ironic)",           "40.40"),
        ("BUY ME a second-hand dentist chair (for vibes)",             "21.00"),
        ("BUY ME a pallet of post-it notes (load-bearing)",            "55.55"),
        ("BUY ME a server rack turned into furniture",                 "17.76"),
        ("BUY ME a certificate of authenticity for something fake",    "22.22"),
        ("BUY ME 1000 yards of bubble wrap (therapeutic)",             "9.99"),
        ("BUY ME an expired globe (geopolitics not included)",         "44.44"),
        ("BUY ME a broken laptop that once belonged to a philosopher", "10.10"),
        ("BUY ME a surplus military compass (true north only)",        "33.33"),
        ("BUY ME a commercial ice cream maker and no regrets",         "12.34"),
        ("BUY ME a decommissioned fire extinguisher (decorative)",     "9.99"),
        ("BUY ME a typewriter so i can feel things",                   "15.00"),
        ("BUY ME a second hand dick pump (gently used)",               "29.99"),
        ("BUY ME several wooden buttplugs (artisanal)",                "19.99"),
        ("BUY ME a horse-sized dildo for debugging motivation",        "69.69"),
        ("BUY ME a used fleshlight and zero context",                  "9.99"),
        ("BUY ME a pair of nipple clamps i can expense as hardware",   "14.20"),
        ("BUY ME a glory hole kit (networking purposes)",              "42.00"),
        ("BUY ME a prostate massager labeled 'senior developer tool'", "39.99"),
        ("BUY ME an onlyfans subscription (for research)",             "9.99"),
        ("BUY ME a fursuit head for the sprint retrospective",         "199.00"),
        ("BUY ME a leather daddy outfit for production deployments",   "88.88"),
    ];

    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let idx      = (ms / 30_000) as usize % ITEMS.len();
    let window   = (ms % 30_000) as usize; // 0..29999 ms into current cycle
    let (msg, price) = ITEMS[idx];
    let total_chars  = msg.chars().count();
    // Reveal one character every 60ms (smooth typewriter feel)
    let chars_shown  = (window / 60).min(total_chars);
    let is_typing = chars_shown < total_chars;
    (msg, price, chars_shown, is_typing)
}

// ── Projects ───────────────────────────────────────────────────────────────────

// Fixed widths for the right-side stats columns
const COL_URL:     usize = 30;
const COL_LOCAL:   usize = 14;
const COL_MEM:     usize =  5;
const COL_CPU:     usize =  5;
const COL_RPAD:    usize =  3; // right-edge padding so last column isn't flush against the border
// spacers: " " + "  " + "  " + " " = 6
const RIGHT_TOTAL: usize = COL_URL + 2 + COL_LOCAL + 2 + COL_MEM + 1 + COL_CPU + COL_RPAD; // 62

fn render_projects(f: &mut Frame, area: Rect, state: &mut AppState) {
    // Inner width available to List items (subtract 2 for borders)
    let inner_w = area.width.saturating_sub(2) as usize;

    // Service rows have "  │ ● " (6 chars) before the name
    const SVC_INDENT: usize = 6;
    // Header has 7 spaces before "service" to align with the name
    const HDR_INDENT: usize = 7;

    // Name column fills whatever remains after indent + right stats + 1 spacer
    let svc_name_w = inner_w.saturating_sub(SVC_INDENT + RIGHT_TOTAL + 1).max(8);
    let hdr_name_w = inner_w.saturating_sub(HDR_INDENT + RIGHT_TOTAL + 1).max(8);

    let mut items: Vec<ListItem> = vec![];
    let mut current_y = area.y + 1; // +1 for top border

    // Column headers
    let hdr = Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM);
    items.push(ListItem::new(Line::from(vec![
        Span::styled(" ".repeat(HDR_INDENT), hdr),
        Span::styled(format!("{:<width$}", "service", width = hdr_name_w), hdr),
        Span::styled(" ", hdr),
        Span::styled(format!("{:<width$}", "url", width = COL_URL), hdr),
        Span::styled("  ", hdr),
        Span::styled(format!("{:<width$}", "local", width = COL_LOCAL), hdr),
        Span::styled("  ", hdr),
        Span::styled(format!("{:>width$}", "mem", width = COL_MEM), hdr),
        Span::styled(" ", hdr),
        Span::styled(format!("{:>width$}", "cpu", width = COL_CPU), hdr),
        Span::raw(" ".repeat(COL_RPAD)),
    ])));
    current_y += 1;

    for (proj_idx, pv) in state.projects.iter().enumerate() {
        let is_sel_proj = state.selected_project == proj_idx;

        state.row_map.push(RowInfo {
            y: current_y,
            project_idx: proj_idx,
            service_idx: None,
        });
        current_y += 1;

        let proj_style = if is_sel_proj && state.selected_service.is_none() {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let svc_count = pv.processes.len();
        let count_dim = Style::default().fg(Color::DarkGray);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                if pv.expanded { "▼ " } else { "▶ " },
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(pv.config.name.clone(), proj_style),
            Span::styled(
                format!("  {} service{}", svc_count, if svc_count == 1 { "" } else { "s" }),
                count_dim,
            ),
            Span::raw("  "),
            Span::raw(project_mood_emoji(pv)),
        ])));

        if pv.expanded {
            for (svc_idx, proc) in pv.processes.iter().enumerate() {
                let is_sel = is_sel_proj && state.selected_service == Some(svc_idx);

                state.row_map.push(RowInfo {
                    y: current_y,
                    project_idx: proj_idx,
                    service_idx: Some(svc_idx),
                });
                current_y += 1;

                let svc_name = proc.id.split('/').last().unwrap_or(&proc.id);
                let dot = status_dot(&proc.status);
                let dot_color = status_color(&proc.status);

                let svc_config = pv.config.services.get(svc_name);
                let (domain_url, local_url) = build_urls(
                    svc_name,
                    svc_config.map(|s| s.subdomain.as_str()).unwrap_or(""),
                    &pv.config.domain,
                    proc.port,
                );

                let mem = format_bytes(proc.memory_bytes);
                let cpu = format_cpu(proc.cpu_percent);

                let name_style = if is_sel {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };

                let url_color = if proc.status == ProcessStatus::Running {
                    Color::Cyan
                } else {
                    Color::DarkGray
                };

                // Show status label when transitioning; URL when running/stopped
                let status_label = match &proc.status {
                    ProcessStatus::Starting    => Some("  starting…"),
                    ProcessStatus::Restarting  => Some("  restarting…"),
                    _                          => None,
                };

                let mut row_spans = vec![
                    Span::raw("  │ "),
                    Span::styled(dot, Style::default().fg(dot_color)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<width$}", svc_name, width = svc_name_w),
                        name_style,
                    ),
                    Span::raw(" "),
                ];

                if let Some(label) = status_label {
                    row_spans.push(Span::styled(
                        format!("{:<width$}", label, width = COL_URL),
                        Style::default().fg(Color::Yellow),
                    ));
                } else if proc.status == ProcessStatus::Running && !proc.proxied {
                    // Running but routing is paused — domain resolves to prod
                    row_spans.push(Span::styled(
                        format!("{:<width$}", "⏸ routing off → prod", width = COL_URL),
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    row_spans.push(Span::styled(
                        format!("{:<width$}", domain_url, width = COL_URL),
                        Style::default().fg(url_color),
                    ));
                }

                row_spans.push(Span::styled("  ", Style::default()));
                row_spans.push(Span::styled(
                    format!("{:<width$}", local_url, width = COL_LOCAL),
                    Style::default().fg(Color::DarkGray),
                ));
                row_spans.push(Span::styled("  ", Style::default()));
                row_spans.push(Span::styled(
                    format!("{:>width$}", mem, width = COL_MEM),
                    Style::default().fg(Color::White),
                ));
                row_spans.push(Span::styled(" ", Style::default()));
                row_spans.push(Span::styled(
                    format!("{:>width$}", cpu, width = COL_CPU),
                    Style::default().fg(Color::Green),
                ));
                row_spans.push(Span::raw(" ".repeat(COL_RPAD)));

                items.push(ListItem::new(Line::from(row_spans)));

                if let ProcessStatus::Crashed { stderr_tail, .. } = &proc.status {
                    let crash_info = CrashInfo {
                        stderr_tail: stderr_tail.clone(),
                        port: proc.port,
                        peak_memory_mb: proc.memory_bytes / (1024 * 1024),
                    };
                    let msg = crash_message(svc_name, &crash_info);
                    let lines: Vec<String> = msg.lines().map(|l| l.to_string()).collect();
                    if let Some(first_line) = lines.first() {
                        current_y += 1;
                        items.push(ListItem::new(Line::from(vec![
                            Span::raw("  │   "),
                            Span::styled(
                                truncate(first_line, 62).to_string(),
                                Style::default().fg(Color::Red),
                            ),
                        ])));
                    }
                    if let Some(second) = lines.get(1) {
                        current_y += 1;
                        items.push(ListItem::new(Line::from(vec![
                            Span::raw("  │   "),
                            Span::styled(
                                truncate(second, 62).to_string(),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ])));
                    }
                }
            }

            if pv.processes.is_empty() {
                current_y += 1;
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  │   "),
                    Span::styled(
                        "no services — press [a] to add one",
                        Style::default().fg(Color::DarkGray),
                    ),
                ])));
            }
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No projects yet. Press [n] to create one.",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" projects "));
    f.render_widget(list, area);
}

// ── Command bar ────────────────────────────────────────────────────────────────

fn render_command_bar(f: &mut Frame, area: Rect, state: &AppState) {
    let content = if state.command_focused {
        format!("🧠 > {}_", state.command_input)
    } else if let Some(ref msg) = state.run_message {
        format!("🧠 {}", msg)
    } else {
        "🧠 press [/] to ask me anything".to_string()
    };

    let style = if state.command_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    f.render_widget(Paragraph::new(content).style(style), area);
}

// ── URL building ───────────────────────────────────────────────────────────────

fn build_urls(svc_name: &str, subdomain: &str, project_domain: &str, port: u16) -> (String, String) {
    let local_url = format!("localhost:{}", port);
    let full_domain = effective_domain(svc_name, subdomain, project_domain);
    let scheme = if cert_exists(project_domain) { "https" } else { "http" };
    let domain_url = format!("{}://{}", scheme, full_domain);
    (domain_url, local_url)
}

/// Resolve the actual domain for a service.
/// If subdomain is set, use it (via resolve_domain).
/// If empty but the service name looks like a full domain (contains '.'), use the name directly.
/// Otherwise fall back to the project domain.
fn effective_domain(svc_name: &str, subdomain: &str, project_domain: &str) -> String {
    if !subdomain.is_empty() {
        resolve_domain(subdomain, project_domain)
    } else if svc_name.contains('.') {
        svc_name.to_string()
    } else {
        project_domain.to_string()
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn status_dot(status: &ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Running => "●",
        ProcessStatus::Starting | ProcessStatus::Restarting => "◐",
        ProcessStatus::Stopped => "○",
        ProcessStatus::Crashed { .. } => "✗",
    }
}

fn status_color(status: &ProcessStatus) -> Color {
    match status {
        ProcessStatus::Running => Color::Green,
        ProcessStatus::Starting | ProcessStatus::Restarting => Color::Yellow,
        ProcessStatus::Stopped => Color::DarkGray,
        ProcessStatus::Crashed { .. } => Color::Red,
    }
}

fn project_mood_emoji(pv: &ProjectView) -> &'static str {
    let total = pv.processes.len();
    if total == 0 { return "○"; }
    let running = pv.processes.iter().filter(|p| p.status == ProcessStatus::Running).count();
    let crashed = pv.processes.iter().filter(|p| p.status.is_crashed()).count();
    if crashed == 0 && running == total { "✨" }
    else if crashed > 0 && running > 0 { "⚠" }
    else if running == 0 && crashed > 0 { "💀" }
    else { "○" }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { s[..max].to_string() }
}
