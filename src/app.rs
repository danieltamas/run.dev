//! Application state, event loop, and wizard state machine.
//!
//! This is the core of run.dev's runtime. It owns the [`AppState`] struct and
//! drives the main event loop: polling terminal events, ticking the resource
//! monitor, receiving AI responses, and routing keyboard/mouse input to the
//! correct handler.
//!
//! The wizard flow is modelled as a [`WizardState`] enum, with each variant
//! representing one step in the project/service creation or rename journey.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::interval;

use crate::ai::diagnose::{answer_question, diagnose_crash};
use crate::ai::mood::{auto_fix_action, calculate_mood, categorize_error, execute_fix, Mood};
use crate::core::config::{load_all_projects, save_project, ProjectConfig, ServiceConfig};
use crate::core::hosts::{cleanup_hosts, update_hosts};
use crate::core::process::{
    kill_orphaned_pids, load_state, pid_exists, restart_process, spawn_process, stop_process,
    ManagedProcess,
    ProcessStatus, SharedProcess,
};
use crate::core::proxy::{activate_port_forwarding, new_route_table, run_proxy, run_https_proxy, update_routes, ProxyRoute, RouteTable};
use crate::core::resources::ResourceMonitor;
use crate::core::scanner::{clean_service_name, detect_commands, DetectedCommand};
use crate::core::ssl::ensure_ssl;

// ── Wizard state machine ───────────────────────────────────────────────────────

/// Drives the multi-step modal wizard for creating projects, adding services,
/// and renaming either.  Each variant is one "screen" in the flow.
///
/// Transitions:
/// - `[a]` from dashboard  → `AddServicePath` (or `AddProjectName` if no projects)
/// - `[n]` from dashboard  → `AddProjectName`
/// - `[e]` from dashboard  → `RenameProject` / `RenameService`
/// - `[Esc]` always steps back or closes the wizard
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum WizardState {
    Inactive,

    // Add-project flow
    AddProjectName { input: String },
    AddProjectDomain { name: String, input: String },
    GeneratingCerts { domain: String },

    // Add-service flow
    AddServicePath { project: String, input: String, completions: Vec<String> },
    AddServiceName {
        project: String,
        path: String,
        input: String,
        suggested: String,
    },
    AddServiceCommand {
        project: String,
        path: String,
        name: String,
        commands: Vec<DetectedCommand>,
        selected: usize,
    },
    AddServiceSubdomain {
        project: String,
        path: String,
        name: String,
        command: String,
        input: String,
        project_domain: String,
    },
    CustomCommand {
        // Returned to after command list when "enter custom command..." selected
        project: String,
        path: String,
        name: String,
        commands: Vec<DetectedCommand>,
        input: String,
    },

    // Rename flows
    RenameProject { project_idx: usize, input: String },
    RenameService { project_idx: usize, old_name: String, input: String },

    // Delete confirmation
    /// `service_name` = None means delete the whole project; Some means delete that service.
    ConfirmDelete { project_idx: usize, service_name: Option<String>, display_name: String },

    // Service context menu (opened with Enter on a selected service)
    ServiceMenu { project_idx: usize, service_idx: usize },
}

impl WizardState {
    pub fn is_active(&self) -> bool {
        !matches!(self, WizardState::Inactive)
    }
}

// ── Row map for mouse clicks ───────────────────────────────────────────────────

/// Maps a rendered TUI row (y coordinate) back to the project/service it represents.
/// Rebuilt every frame in [`render_projects`][crate::ui::dashboard] so mouse
/// clicks can be resolved without storing stale indices.
#[derive(Clone)]
pub struct RowInfo {
    pub y: u16,
    pub project_idx: usize,
    pub service_idx: Option<usize>,
}

// ── Project/service views ──────────────────────────────────────────────────────

pub struct ProjectView {
    pub config: ProjectConfig,
    pub processes: Vec<ManagedProcess>,
    pub shared: Vec<SharedProcess>,
    pub expanded: bool,
}

/// Request to shell out to a service's working directory.
pub struct ShellOutRequest {
    pub working_dir: std::path::PathBuf,
    pub env: std::collections::HashMap<String, String>,
    pub service_name: String,
}

pub struct AppState {
    pub projects: Vec<ProjectView>,
    pub selected_project: usize,
    pub selected_service: Option<usize>,
    pub log_panel_open: bool,
    pub log_scroll: usize,
    pub command_focused: bool,
    pub command_input: String,
    pub run_message: Option<String>,
    /// System-level error/warning shown in the dedicated error bar at the bottom.
    /// Unlike `run_message` (AI/status messages), this persists until explicitly cleared.
    pub error_message: Option<String>,
    pub mood: Mood,
    pub proxy_running: bool,
    pub no_proxy: bool,
    pub no_ai: bool,
    pub should_quit: bool,
    pub quit_stop_all: bool,
    pub row_map: Vec<RowInfo>,
    pub ai_tx: Option<mpsc::Sender<String>>,
    pub wizard: WizardState,
    pub shell_out_request: Option<ShellOutRequest>,
}

impl AppState {
    pub fn new(no_proxy: bool, no_ai: bool) -> Self {
        Self {
            projects: vec![],
            selected_project: 0,
            selected_service: None,
            log_panel_open: false,
            log_scroll: 0,
            command_focused: false,
            command_input: String::new(),
            run_message: None,
            error_message: None,
            mood: Mood::Flatlined,
            proxy_running: false,
            no_proxy,
            no_ai,
            should_quit: false,
            quit_stop_all: false,
            row_map: vec![],
            ai_tx: None,
            wizard: WizardState::Inactive,
            shell_out_request: None,
        }
    }

    pub fn load_projects(&mut self) {
        let configs = load_all_projects();
        let saved = load_state();

        for config in configs {
            self.add_project_view(config, &saved.pids);
        }
    }

    pub fn add_project_view(
        &mut self,
        config: ProjectConfig,
        saved_pids: &HashMap<String, u32>,
    ) {
        let mut processes = vec![];
        let mut shared_procs = vec![];

        for (svc_name, svc_config) in &config.services {
            let id = format!("{}/{}", config.name, svc_name);
            let working_dir = PathBuf::from(&svc_config.path);

            // Wrap command with nvm if a node_version is specified
            let command = wrap_command_with_nvm(&svc_config.command, &svc_config.node_version);

            let mut proc = ManagedProcess::new(
                id.clone(),
                command,
                working_dir,
                svc_config.port,
                svc_config.env.clone(),
            );
            proc.proxied = true;

            if let Some(&pid) = saved_pids.get(&id) {
                if pid_exists(pid) {
                    proc.pid = Some(pid);
                    proc.status = ProcessStatus::Running;
                } else {
                    proc.status = ProcessStatus::Crashed {
                        exit_code: -1,
                        stderr_tail: "process died while run.dev was closed".to_string(),
                    };
                    proc.crash_count = 1;
                }
            }

            let shared = Arc::new(Mutex::new(proc.clone()));
            processes.push(proc);
            shared_procs.push(shared);
        }

        self.projects.push(ProjectView {
            config,
            processes,
            shared: shared_procs,
            expanded: true,
        });
    }

    pub fn recalculate_mood(&mut self) {
        let all: Vec<ManagedProcess> = self
            .projects
            .iter()
            .flat_map(|p| p.processes.iter().cloned())
            .collect();
        self.mood = calculate_mood(&all);
    }

    pub fn selected_service_proc(&self) -> Option<&ManagedProcess> {
        let proj = self.projects.get(self.selected_project)?;
        let idx = self.selected_service?;
        proj.processes.get(idx)
    }

    pub fn selected_service_shared(&self) -> Option<SharedProcess> {
        let proj = self.projects.get(self.selected_project)?;
        let idx = self.selected_service.unwrap_or(0);
        proj.shared.get(idx).cloned()
    }

    pub fn handle_click(&mut self, row: u16, _col: u16) {
        for info in self.row_map.clone() {
            if info.y == row {
                if info.project_idx == self.selected_project
                    && info.service_idx == self.selected_service
                    && info.service_idx.is_none()
                {
                    if let Some(pv) = self.projects.get_mut(info.project_idx) {
                        pv.expanded = !pv.expanded;
                        if !pv.expanded {
                            self.selected_service = None;
                        }
                    }
                } else {
                    self.selected_project = info.project_idx;
                    self.selected_service = info.service_idx;
                }
                return;
            }
        }
    }
}

// ── Main run loop ──────────────────────────────────────────────────────────────

pub async fn run_app(
    terminal: &mut crate::tui::Tui,
    mut state: AppState,
) -> Result<()> {
    let (ai_tx, mut ai_rx) = mpsc::channel::<String>(16);
    state.ai_tx = Some(ai_tx);

    let route_table = new_route_table();

    if !state.no_proxy {
        // Ensure SSL certs exist for all project domains before starting the HTTPS proxy.
        // Generate SSL certs for all project domains before starting the HTTPS
        // proxy (which loads them from disk at startup). Runs in a blocking
        // thread so it doesn't stall the async executor, but we await it so
        // certs are on disk before SniCertResolver scans the directory.
        let domains: Vec<String> = state.projects.iter()
            .flat_map(|pv| pv.config.all_domains())
            .collect();
        let ssl_result = tokio::task::spawn_blocking(move || {
            let mut errors = vec![];
            for domain in domains {
                if let Err(e) = ensure_ssl(&domain) {
                    errors.push(format!("SSL {}: {}", domain, e));
                }
            }
            errors
        }).await.unwrap_or_default();

        if !ssl_result.is_empty() {
            state.error_message = Some(format!("SSL: {}", ssl_result.join("; ")));
        }

        // Try to forward ports 80/443 → 1111/1112 via pfctl (setup by `rundev setup`)
        activate_port_forwarding();

        let rt = route_table.clone();
        tokio::spawn(async move {
            if let Err(e) = run_proxy(rt, 1111).await {
                eprintln!("HTTP proxy failed: {}", e);
            }
        });

        let rt2 = route_table.clone();
        tokio::spawn(async move {
            if let Err(e) = run_https_proxy(rt2, 1112).await {
                eprintln!("HTTPS proxy failed: {}", e);
            }
        });

        state.proxy_running = true;
    }

    // Kill orphaned processes from any previous session (e.g. after a crash).
    kill_orphaned_pids().await;

    sync_proxy_routes(&state, &route_table).await;

    let mut resource_monitor = ResourceMonitor::new();
    let mut tick = interval(Duration::from_millis(100));
    let mut resource_tick: u8 = 0;
    let mut hosts_updated = false;
    let mut events = EventStream::new();

    loop {
        sync_process_states(&mut state).await;
        state.recalculate_mood();

        terminal.draw(|f| crate::ui::render(f, &mut state))?;

        tokio::select! {
            _ = tick.tick() => {
                // Update /etc/hosts once, after first render, without blocking startup.
                if !hosts_updated {
                    hosts_updated = true;
                    update_hosts_for_state(&mut state);
                }

                resource_tick = resource_tick.wrapping_add(1);
                if resource_tick % 20 == 0 {
                    let all_shared: Vec<SharedProcess> = state.projects
                        .iter()
                        .flat_map(|p| p.shared.iter().cloned())
                        .collect();
                    resource_monitor.update(&all_shared).await;
                    sync_traffic_stats(&mut state, &route_table).await;
                    // Re-sync proxy routes so auto-detected ports take effect
                    sync_proxy_routes(&state, &route_table).await;
                }
            }

            maybe_event = events.next() => {
                if let Some(Ok(event)) = maybe_event {
                    handle_event(&mut state, event, &route_table).await?;
                }
            }

            Some(msg) = ai_rx.recv() => {
                state.run_message = Some(msg);
            }
        }

        // Shell out to a service's working directory
        if let Some(req) = state.shell_out_request.take() {
            shell_out(terminal, &req).await?;
        }

        if state.should_quit {
            // Show shutdown screen while stopping services
            graceful_shutdown(terminal, &state).await;
            // Force exit — proxy tasks run infinite accept loops that would
            // prevent graceful tokio runtime shutdown, leaving the process alive.
            std::process::exit(0);
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

// ── Shell out ───────────────────────────────────────────────────────────────────

async fn shell_out(
    terminal: &mut crate::tui::Tui,
    req: &ShellOutRequest,
) -> Result<()> {
    // Suspend TUI
    crate::tui::restore()?;

    eprintln!(
        "\n  \x1b[36mshell for:\x1b[0m {}  \x1b[2m(type `exit` to return to run.dev)\x1b[0m\n",
        req.service_name
    );

    // Spawn interactive shell with a custom prompt so the user always
    // knows they're inside a run.dev sub-shell.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sh")
        .to_string();

    // Build a tiny rc file that sources the user's config then overrides the prompt
    let tmp_rc = std::env::temp_dir().join(format!("rundev-shell-{}.rc", std::process::id()));
    let mut shell_args: Vec<String> = Vec::new();
    let mut extra_env: Vec<(String, String)> = Vec::new();

    match shell_name.as_str() {
        "zsh" => {
            // For zsh: create a temp ZDOTDIR with a .zshrc that sources user's then sets PROMPT
            let zdotdir = std::env::temp_dir().join(format!("rundev-zsh-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&zdotdir);
            let zshrc = zdotdir.join(".zshrc");
            let user_rc = dirs::home_dir()
                .map(|h| h.join(".zshrc"))
                .unwrap_or_default();
            let rc_content = format!(
                "[ -f \"{}\" ] && source \"{}\"\nPROMPT=$'\\e[48;5;31m\\e[97m run.dev:{} \\e[0m '\"$PROMPT\"\n",
                user_rc.display(),
                user_rc.display(),
                req.service_name
            );
            let _ = std::fs::write(&zshrc, rc_content);
            extra_env.push(("ZDOTDIR".to_string(), zdotdir.display().to_string()));
        }
        "bash" => {
            // For bash: --rcfile with a custom rc
            let user_rc = dirs::home_dir()
                .map(|h| h.join(".bashrc"))
                .unwrap_or_default();
            let rc_content = format!(
                "[ -f \"{}\" ] && source \"{}\"\nPS1=\"\\[\\e[48;5;31m\\e[97m\\] run.dev:{} \\[\\e[0m\\] $PS1\"\n",
                user_rc.display(),
                user_rc.display(),
                req.service_name
            );
            let _ = std::fs::write(&tmp_rc, rc_content);
            shell_args.push("--rcfile".to_string());
            shell_args.push(tmp_rc.display().to_string());
        }
        _ => {
            // Fallback: set PS1 env var (works for sh and most POSIX shells)
            extra_env.push(("PS1".to_string(), format!(
                "\x1b[48;5;31m\x1b[97m run.dev:{} \x1b[0m $ ",
                req.service_name
            )));
        }
    }

    let mut cmd = tokio::process::Command::new(&shell);
    cmd.current_dir(&req.working_dir)
        .envs(&req.env)
        .envs(extra_env.clone())
        .args(&shell_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let _ = cmd.status().await;

    // Clean up temp files
    let _ = std::fs::remove_file(&tmp_rc);
    if shell_name == "zsh" {
        let zdotdir = std::env::temp_dir().join(format!("rundev-zsh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&zdotdir);
    }

    // Resume TUI
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    terminal.clear()?;

    Ok(())
}

// ── Sync ───────────────────────────────────────────────────────────────────────

async fn sync_process_states(state: &mut AppState) {
    let selected_proj = state.selected_project;
    let selected_svc = state.selected_service;

    let mut respawn_list: Vec<SharedProcess> = Vec::new();

    for (pi, pv) in state.projects.iter_mut().enumerate() {
        for (si, shared) in pv.shared.iter().enumerate() {
            let mut p = shared.lock().await;
            if let Some(proc) = pv.processes.get_mut(si) {
                proc.status = p.status.clone();
                proc.pid = p.pid;
                proc.port = p.port;
                proc.cpu_percent = p.cpu_percent;
                proc.memory_bytes = p.memory_bytes;
                proc.crash_count = p.crash_count;
                // Only clone log buffers when needed and when they've changed
                let is_selected = pi == selected_proj && selected_svc == Some(si);
                if is_selected {
                    // Only re-clone if line count changed (new output arrived)
                    if proc.combined_log.len() != p.combined_log.len() {
                        proc.last_stderr = p.last_stderr.clone();
                        proc.stdout_log = p.stdout_log.clone();
                        proc.combined_log = p.combined_log.clone();
                    }
                }
                // Check for EADDRINUSE auto-recovery respawn flag
                if p.needs_respawn {
                    p.needs_respawn = false;
                    respawn_list.push(shared.clone());
                }
            }
        }
    }

    // Respawn services that recovered from EADDRINUSE
    for shared in respawn_list {
        tokio::spawn(async move {
            let _ = spawn_process(shared).await;
        });
    }
}


async fn graceful_shutdown(terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>, state: &AppState) {
    use ratatui::style::{Color, Style, Modifier};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Paragraph, Padding};

    // Collect service names and their shared handles
    let mut services: Vec<(String, SharedProcess)> = Vec::new();
    for pv in &state.projects {
        for (i, proc) in pv.processes.iter().enumerate() {
            if proc.pid.is_some() {
                if let Some(shared) = pv.shared.get(i) {
                    services.push((proc.id.clone(), shared.clone()));
                }
            }
        }
    }

    let total = services.len();

    for (idx, (_name, shared)) in services.iter().enumerate() {
        // Render shutdown progress
        let _ = terminal.draw(|f| {
            let area = f.area();

            let banner_style = Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD);
            let dim_style = Style::default().fg(Color::DarkGray);
            let done_style = Style::default().fg(Color::Green);
            let active_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  run.dev is shutting down  ", banner_style)));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  Stopping services ({}/{})...", idx + 1, total),
                dim_style,
            )));
            lines.push(Line::from(""));

            // Show each service with a status indicator
            for (j, (svc_name, _)) in services.iter().enumerate() {
                let short = svc_name.split('/').last().unwrap_or(svc_name);
                let line = if j < idx {
                    Line::from(Span::styled(format!("  [x] {}", short), done_style))
                } else if j == idx {
                    Line::from(Span::styled(format!("  [-] {} stopping...", short), active_style))
                } else {
                    Line::from(Span::styled(format!("  [ ] {}", short), dim_style))
                };
                lines.push(line);
            }

            let paragraph = Paragraph::new(lines)
                .block(Block::default().padding(Padding::new(2, 2, 1, 1)));
            f.render_widget(paragraph, area);
        });

        let _ = stop_process(shared.clone()).await;
    }

    // Final frame: all done
    let _ = terminal.draw(|f| {
        let area = f.area();
        let banner_style = Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD);
        let done_style = Style::default().fg(Color::Green);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  run.dev is shutting down  ", banner_style)));
        lines.push(Line::from(""));

        for (svc_name, _) in &services {
            let short = svc_name.split('/').last().unwrap_or(svc_name);
            lines.push(Line::from(Span::styled(format!("  [x] {}", short), done_style)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Cleaning up DNS...", Style::default().fg(Color::DarkGray))));

        let paragraph = Paragraph::new(lines)
            .block(Block::default().padding(Padding::new(2, 2, 1, 1)));
        f.render_widget(paragraph, area);
    });

    // Clean up hosts and DNS
    let _ = cleanup_hosts();

    // Brief pause so the user sees the final state
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Restore terminal
    let _ = crate::tui::restore();
}

/// Wrap a command with `nvm use <version>` if a node_version is specified.
/// The resulting command runs through bash so nvm is available.
fn wrap_command_with_nvm(command: &str, node_version: &Option<String>) -> String {
    match node_version {
        Some(version) if !version.is_empty() => {
            // Source nvm and switch to the requested version before running the command.
            // NVM_DIR is typically ~/.nvm; fall back to the standard location.
            format!(
                "bash -c 'export NVM_DIR=\"${{NVM_DIR:-$HOME/.nvm}}\"; [ -s \"$NVM_DIR/nvm.sh\" ] && . \"$NVM_DIR/nvm.sh\"; nvm use {} && {}'",
                version,
                command.replace('\'', "'\\''"),
            )
        }
        _ => command.to_string(),
    }
}

pub fn update_hosts_for_state(state: &mut AppState) {
    // Build per-project entries for domains of currently-proxied services only.
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    for pv in &state.projects {
        let mut proxied_domains: Vec<String> = Vec::new();
        for proc in &pv.processes {
            if !proc.proxied { continue; }
            let svc_name = proc.id.split('/').last().unwrap_or(&proc.id);
            let domain = if let Some(svc) = pv.config.services.get(svc_name) {
                crate::core::config::resolve_domain(&svc.subdomain, &pv.config.domain)
            } else if svc_name.contains('.') {
                svc_name.to_string()
            } else {
                pv.config.domain.clone()
            };
            if !proxied_domains.contains(&domain) {
                proxied_domains.push(domain);
            }
        }
        if !proxied_domains.is_empty() {
            entries.push((pv.config.domain.clone(), proxied_domains));
        }
    }

    if entries.is_empty() {
        let _ = cleanup_hosts();
        return;
    }

    if let Err(e) = update_hosts(&entries) {
        state.error_message = Some(format!(
            "/etc/hosts update failed: {}  Run `rundev setup` to fix.", e
        ));
    }
}

async fn sync_proxy_routes(state: &AppState, route_table: &RouteTable) {
    let mut routes = vec![];
    for pv in &state.projects {
        for (svc_name, svc) in &pv.config.services {
            let domain = if svc.subdomain.is_empty() && svc_name.contains('.') {
                svc_name.clone()
            } else {
                crate::core::config::resolve_domain(&svc.subdomain, &pv.config.domain)
            };
            // Only route traffic to services that are actually running
            let id = format!("{}/{}", pv.config.name, svc_name);
            let proc = pv.processes.iter().find(|p| p.id == id);
            let is_active = proc.map(|p| matches!(p.status,
                crate::core::process::ProcessStatus::Running |
                crate::core::process::ProcessStatus::Starting |
                crate::core::process::ProcessStatus::Restarting
            )).unwrap_or(false);
            if !is_active { continue; }
            let target_port = proc
                .map(|p| p.port)
                .filter(|&p| p > 0)
                .unwrap_or(svc.port);
            if target_port > 0 {
                routes.push(ProxyRoute::new(domain, target_port));
            }
        }
    }
    update_routes(route_table, routes).await;
}

/// Copy byte counters from the proxy route table into each process for display.
async fn sync_traffic_stats(state: &mut AppState, route_table: &RouteTable) {
    let table = route_table.read().unwrap();
    for pv in &mut state.projects {
        for proc in &mut pv.processes {
            let svc_name = proc.id.split('/').last().unwrap_or(&proc.id);
            let svc_config = pv.config.services.get(svc_name);
            if let Some(svc) = svc_config {
                let domain = if svc.subdomain.is_empty() && svc_name.contains('.') {
                    svc_name.to_string()
                } else {
                    crate::core::config::resolve_domain(&svc.subdomain, &pv.config.domain)
                };
                if let Some(route) = table.iter().find(|r| r.domain == domain) {
                    proc.net_in = route.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
                    proc.net_out = route.bytes_out.load(std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

// ── Event routing ──────────────────────────────────────────────────────────────

async fn handle_event(
    state: &mut AppState,
    event: Event,
    _route_table: &RouteTable,
) -> Result<()> {
    match event {
        Event::Key(key) => {
            if state.wizard.is_active() {
                handle_wizard_key(state, key).await?;
            } else if state.command_focused {
                handle_command_key(state, key).await?;
            } else {
                handle_key(state, key).await?;
            }
        }
        Event::Mouse(mouse) => {
            // Catch any panic from mouse handling (e.g. index out of bounds
            // from rapid clicks/drags) so it doesn't crash the whole app.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_mouse(state, mouse);
            }));
        }
        _ => {}
    }
    Ok(())
}

// ── Wizard key handler ─────────────────────────────────────────────────────────

async fn handle_wizard_key(state: &mut AppState, key: KeyEvent) -> Result<()> {
    let wizard = state.wizard.clone();
    match wizard {
        WizardState::AddProjectName { mut input } => match key.code {
            KeyCode::Esc => state.wizard = WizardState::Inactive,
            KeyCode::Backspace => { input.pop(); state.wizard = WizardState::AddProjectName { input }; }
            KeyCode::Enter if !input.trim().is_empty() => {
                let name = input.trim().to_string();

                // Reject duplicate project names
                if state.projects.iter().any(|pv| pv.config.name == name) {
                    state.run_message = Some(format!("project '{}' already exists", name));
                    state.wizard = WizardState::AddProjectName { input: name };
                    return Ok(());
                }

                // Proceed to domain input — pre-fill with sanitised name
                let default_domain = name.to_lowercase().replace(' ', "-");
                state.wizard = WizardState::AddProjectDomain {
                    name,
                    input: default_domain,
                };
            }
            KeyCode::Char(c) => { input.push(c); state.wizard = WizardState::AddProjectName { input }; }
            _ => {}
        },

        WizardState::AddProjectDomain { name, mut input } => match key.code {
            KeyCode::Esc => {
                // Go back to project name step
                state.wizard = WizardState::AddProjectName { input: name };
            }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::AddProjectDomain { name, input };
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                let domain = input.trim().to_string();

                // Create the project
                let config = ProjectConfig {
                    name: name.clone(),
                    domain: domain.clone(),
                    services: HashMap::new(),
                    config_path: crate::core::config::projects_dir()
                        .join(format!("{}.yaml", name)),
                };
                if let Err(e) = save_project(&config) {
                    state.run_message = Some(format!("error saving project: {}", e));
                    state.wizard = WizardState::Inactive;
                    return Ok(());
                }

                // Add to state
                let saved = load_state();
                state.add_project_view(config, &saved.pids);
                state.selected_project = state.projects.len().saturating_sub(1);
                state.selected_service = None;

                // Kick off cert generation async in background
                let domain_clone = domain.clone();
                let tx = state.ai_tx.clone();
                tokio::spawn(async move {
                    let msg = match ensure_ssl(&domain_clone) {
                        Ok(_) => format!("🔐 certs ready for {}", domain_clone),
                        Err(e) => format!("⚠️  cert generation failed for {}: {}", domain_clone, e),
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(msg).await;
                    }
                });
                state.wizard = WizardState::Inactive;
                state.run_message = Some(format!("created {} — press [a] to add a service", name));
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::AddProjectDomain { name, input };
            }
            _ => {}
        },

        WizardState::GeneratingCerts { .. } => {
            state.wizard = WizardState::Inactive;
        }

        WizardState::AddServicePath { project, mut input, .. } => match key.code {
            KeyCode::Esc => state.wizard = WizardState::Inactive,
            KeyCode::Backspace => {
                input.pop();
                let completions = path_completions(&input);
                state.wizard = WizardState::AddServicePath { project, input, completions };
            }
            KeyCode::Tab => {
                input = autocomplete_path(&input);
                let completions = path_completions(&input);
                state.wizard = WizardState::AddServicePath { project, input, completions };
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                let raw = input.trim().to_string();
                let path = expand_path(&raw);
                let folder = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
                    .to_string();
                let suggested = clean_service_name(&folder);
                state.wizard = WizardState::AddServiceName {
                    project,
                    path,
                    input: suggested.clone(),
                    suggested,
                };
            }
            KeyCode::Char(c) => {
                input.push(c);
                let completions = path_completions(&input);
                state.wizard = WizardState::AddServicePath { project, input, completions };
            }
            _ => {}
        },

        WizardState::AddServiceName { project, path, mut input, suggested } => match key.code {
            KeyCode::Esc => {
                state.wizard = WizardState::AddServicePath { project, input: path, completions: vec![] };
            }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::AddServiceName { project, path, input, suggested };
            }
            KeyCode::Enter => {
                let name = if input.trim().is_empty() {
                    suggested.clone()
                } else {
                    input.trim().to_string()
                };
                let dir = std::path::Path::new(&path);
                let mut commands = detect_commands(dir);
                if commands.is_empty() {
                    commands.push(DetectedCommand {
                        label: "enter custom command...".to_string(),
                        command: String::new(),
                        recommended: false,
                        port: None,
                    });
                }
                state.wizard = WizardState::AddServiceCommand {
                    project,
                    path,
                    name,
                    commands,
                    selected: 0,
                };
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::AddServiceName { project, path, input, suggested };
            }
            _ => {}
        },

        WizardState::AddServiceCommand { project, path, name, commands, mut selected } => {
            match key.code {
                KeyCode::Esc => {
                    state.wizard = WizardState::AddServiceName {
                        project,
                        input: name.clone(),
                        suggested: name,
                        path,
                    };
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    selected = (selected + 1).min(commands.len().saturating_sub(1));
                    state.wizard = WizardState::AddServiceCommand { project, path, name, commands, selected };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    state.wizard = WizardState::AddServiceCommand { project, path, name, commands, selected };
                }
                KeyCode::Enter => {
                    let chosen = &commands[selected];
                    if chosen.command.is_empty() {
                        // "enter custom command..."
                        state.wizard = WizardState::CustomCommand {
                            project,
                            path,
                            name,
                            commands,
                            input: String::new(),
                        };
                    } else {
                        let command = chosen.command.clone();
                        let project_domain = state
                            .projects
                            .iter()
                            .find(|p| p.config.name == project)
                            .map(|p| p.config.domain.clone())
                            .unwrap_or_else(|| project.to_lowercase().replace(' ', "-"));
                        state.wizard = WizardState::AddServiceSubdomain {
                            project,
                            path,
                            name: name.clone(),
                            command,
                            input: name,
                            project_domain,
                        };
                    }
                }
                _ => {}
            }
        }

        WizardState::CustomCommand { project, path, name, commands, mut input } => match key.code {
            KeyCode::Esc => {
                state.wizard = WizardState::AddServiceCommand {
                    project, path, name, commands, selected: 0,
                };
            }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::CustomCommand { project, path, name, commands, input };
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                let command = input.trim().to_string();
                let project_domain = state
                    .projects
                    .iter()
                    .find(|p| p.config.name == project)
                    .map(|p| p.config.domain.clone())
                    .unwrap_or_else(|| project.to_lowercase().replace(' ', "-"));
                state.wizard = WizardState::AddServiceSubdomain {
                    project, path, name: name.clone(), command,
                    input: name,
                    project_domain,
                };
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::CustomCommand { project, path, name, commands, input };
            }
            _ => {}
        },

        WizardState::AddServiceSubdomain {
            project, path, name, mut input, command, project_domain
        } => match key.code {
            KeyCode::Esc => {
                state.wizard = WizardState::AddServiceName {
                    project,
                    path,
                    input: name.clone(),
                    suggested: name,
                };
            }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::AddServiceSubdomain {
                    project, path, name, command, input, project_domain,
                };
            }
            KeyCode::Enter => {
                let subdomain = input.trim().to_string();
                complete_add_service(state, project, path, name, command, 0, subdomain, project_domain).await;
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::AddServiceSubdomain {
                    project, path, name, command, input, project_domain,
                };
            }
            _ => {}
        },

        WizardState::RenameProject { project_idx, mut input } => match key.code {
            KeyCode::Esc => { state.wizard = WizardState::Inactive; }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::RenameProject { project_idx, input };
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                complete_rename_project(state, project_idx, input.trim().to_string());
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::RenameProject { project_idx, input };
            }
            _ => {}
        },

        WizardState::RenameService { project_idx, old_name, mut input } => match key.code {
            KeyCode::Esc => { state.wizard = WizardState::Inactive; }
            KeyCode::Backspace => {
                input.pop();
                state.wizard = WizardState::RenameService { project_idx, old_name, input };
            }
            KeyCode::Enter if !input.trim().is_empty() => {
                complete_rename_service(state, project_idx, old_name, input.trim().to_string());
            }
            KeyCode::Char(c) => {
                input.push(c);
                state.wizard = WizardState::RenameService { project_idx, old_name, input };
            }
            _ => {}
        },

        WizardState::ServiceMenu { project_idx, service_idx } => {
            state.wizard = WizardState::Inactive;
            state.selected_project = project_idx;
            state.selected_service = Some(service_idx);
            match key.code {
                KeyCode::Char('s') => start_selected(state).await,
                KeyCode::Char('p') => pause_selected(state).await,
                KeyCode::Char('r') => restart_selected(state).await,
                KeyCode::Char('x') => stop_selected(state).await,
                KeyCode::Char('e') => open_rename_wizard(state),
                KeyCode::Char('d') => open_delete_confirm(state),
                KeyCode::Char('l') => { state.log_panel_open = true; state.log_scroll = 0; }
                KeyCode::Char('t') => {
                    if let Some(proc) = state.selected_service_proc() {
                        state.shell_out_request = Some(ShellOutRequest {
                            working_dir: proc.working_dir.clone(),
                            env: proc.env.clone(),
                            service_name: proc.id.clone(),
                        });
                    }
                }
                _ => {} // Esc or anything else just closes
            }
        }

        WizardState::ConfirmDelete { project_idx, service_name, .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(svc) = service_name {
                    complete_delete_service(state, project_idx, svc).await;
                } else {
                    complete_delete_project(state, project_idx).await;
                }
            }
            _ => { state.wizard = WizardState::Inactive; }
        },

        WizardState::Inactive => {}
    }
    Ok(())
}

async fn complete_add_service(
    state: &mut AppState,
    project_name: String,
    path: String,
    svc_name: String,
    command: String,
    port: u16,
    subdomain: String,
    _project_domain: String,
) {
    // Find and update the project config
    let proj_idx = state
        .projects
        .iter()
        .position(|p| p.config.name == project_name);

    let proj_idx = match proj_idx {
        Some(i) => i,
        None => {
            state.run_message = Some(format!("project '{}' not found", project_name));
            state.wizard = WizardState::Inactive;
            return;
        }
    };

    let svc_config = ServiceConfig {
        path: path.clone(),
        command: command.clone(),
        port,
        subdomain: subdomain.clone(),
        env: HashMap::new(),
        node_version: None,
    };

    // Update in-memory config
    state.projects[proj_idx]
        .config
        .services
        .insert(svc_name.clone(), svc_config.clone());

    // Persist
    let config = state.projects[proj_idx].config.clone();
    if let Err(e) = save_project(&config) {
        state.run_message = Some(format!("error saving config: {}", e));
    }

    // Create process and start it
    let id = format!("{}/{}", project_name, svc_name);
    let proc = ManagedProcess::new(
        id,
        command,
        PathBuf::from(&path),
        port,
        HashMap::new(),
    );

    let shared = Arc::new(Mutex::new(proc.clone()));
    let shared_clone = shared.clone();

    state.projects[proj_idx].processes.push(proc);
    // Mark as proxied since we're starting it immediately
    if let Some(p) = state.projects[proj_idx].processes.last_mut() {
        p.proxied = true;
    }
    state.projects[proj_idx].shared.push(shared);

    // Update /etc/hosts to include this new domain
    update_hosts_for_state(state);

    // Start the service
    tokio::spawn(async move {
        let _ = spawn_process(shared_clone).await;
    });

    state.selected_service = Some(state.projects[proj_idx].processes.len() - 1);
    state.wizard = WizardState::Inactive;
    state.run_message = Some(format!(
        "added {} — starting on :{}", svc_name, port
    ));
}

fn complete_rename_project(state: &mut AppState, proj_idx: usize, new_name: String) {
    if proj_idx >= state.projects.len() {
        state.wizard = WizardState::Inactive;
        return;
    }

    let old_name = state.projects[proj_idx].config.name.clone();
    if new_name == old_name {
        state.wizard = WizardState::Inactive;
        return;
    }

    // Check for name collision
    if state.projects.iter().any(|p| p.config.name == new_name) {
        state.run_message = Some(format!("project '{}' already exists", new_name));
        state.wizard = WizardState::Inactive;
        return;
    }

    // Delete old config file, update name, save under new filename
    let old_path = state.projects[proj_idx].config.config_path.clone();
    state.projects[proj_idx].config.name = new_name.clone();
    let new_path = crate::core::config::projects_dir().join(format!("{}.yaml", new_name));
    state.projects[proj_idx].config.config_path = new_path.clone();

    if let Err(e) = save_project(&state.projects[proj_idx].config) {
        state.run_message = Some(format!("error saving: {}", e));
        state.wizard = WizardState::Inactive;
        return;
    }
    let _ = std::fs::remove_file(&old_path);

    // Update process IDs so they reflect the new project name
    for proc in &mut state.projects[proj_idx].processes {
        let svc_part = proc.id.split('/').last().unwrap_or("").to_string();
        proc.id = format!("{}/{}", new_name, svc_part);
    }

    state.wizard = WizardState::Inactive;
    state.run_message = Some(format!("renamed to '{}'", new_name));
}

fn complete_rename_service(
    state: &mut AppState,
    proj_idx: usize,
    old_name: String,
    new_name: String,
) {
    if proj_idx >= state.projects.len() {
        state.wizard = WizardState::Inactive;
        return;
    }

    if new_name == old_name {
        state.wizard = WizardState::Inactive;
        return;
    }

    // Check for collision within same project
    if state.projects[proj_idx].config.services.contains_key(&new_name) {
        state.run_message = Some(format!("service '{}' already exists", new_name));
        state.wizard = WizardState::Inactive;
        return;
    }

    // Rename in config: remove old key, insert with new key
    if let Some(svc_config) = state.projects[proj_idx].config.services.remove(&old_name) {
        state.projects[proj_idx].config.services.insert(new_name.clone(), svc_config);
    } else {
        state.run_message = Some(format!("service '{}' not found", old_name));
        state.wizard = WizardState::Inactive;
        return;
    }

    // Update matching process ID
    let project_name = state.projects[proj_idx].config.name.clone();
    for proc in &mut state.projects[proj_idx].processes {
        if proc.id == format!("{}/{}", project_name, old_name) {
            proc.id = format!("{}/{}", project_name, new_name);
        }
    }

    // Persist
    if let Err(e) = save_project(&state.projects[proj_idx].config) {
        state.run_message = Some(format!("error saving: {}", e));
    }

    state.wizard = WizardState::Inactive;
    state.run_message = Some(format!("renamed to '{}'", new_name));
}

fn open_delete_confirm(state: &mut AppState) {
    if state.projects.is_empty() { return; }
    let proj_idx = state.selected_project;

    if let Some(svc_idx) = state.selected_service {
        if let Some(proc) = state.projects[proj_idx].processes.get(svc_idx) {
            let svc_name = proc.id.split('/').last().unwrap_or("").to_string();
            let display = format!("service '{}'", svc_name);
            state.wizard = WizardState::ConfirmDelete {
                project_idx: proj_idx,
                service_name: Some(svc_name),
                display_name: display,
            };
        }
    } else {
        let name = state.projects[proj_idx].config.name.clone();
        let svc_count = state.projects[proj_idx].config.services.len();
        let display = if svc_count == 0 {
            format!("project '{}'", name)
        } else {
            format!("project '{}' and its {} service{}", name, svc_count, if svc_count == 1 { "" } else { "s" })
        };
        state.wizard = WizardState::ConfirmDelete {
            project_idx: proj_idx,
            service_name: None,
            display_name: display,
        };
    }
}

async fn complete_delete_project(state: &mut AppState, proj_idx: usize) {
    if proj_idx >= state.projects.len() {
        state.wizard = WizardState::Inactive;
        return;
    }

    // Stop all running services first
    for shared in state.projects[proj_idx].shared.clone() {
        let _ = stop_process(shared).await;
    }

    // Remove config file
    let path = state.projects[proj_idx].config.config_path.clone();
    let name = state.projects[proj_idx].config.name.clone();
    let _ = std::fs::remove_file(&path);

    // Remove from state
    state.projects.remove(proj_idx);
    state.selected_project = state.selected_project.min(state.projects.len().saturating_sub(1));
    state.selected_service = None;

    // Update hosts
    update_hosts_for_state(state);

    state.wizard = WizardState::Inactive;
    state.run_message = Some(format!("deleted project '{}'", name));
}

async fn complete_delete_service(state: &mut AppState, proj_idx: usize, svc_name: String) {
    if proj_idx >= state.projects.len() {
        state.wizard = WizardState::Inactive;
        return;
    }

    let project_name = state.projects[proj_idx].config.name.clone();
    let id = format!("{}/{}", project_name, svc_name);

    // Stop the service if running — find index via processes (not behind a lock)
    // then use the corresponding shared handle.
    if let Some(idx) = state.projects[proj_idx].processes.iter().position(|p| p.id == id) {
        let shared = state.projects[proj_idx].shared[idx].clone();
        let _ = stop_process(shared).await;
    }

    // Remove from config, processes, and shared handles.
    // Find the index via `processes` (which is not behind a lock), then remove
    // at the same index from `shared` to keep the two vectors in sync.
    state.projects[proj_idx].config.services.remove(&svc_name);
    if let Some(idx) = state.projects[proj_idx].processes.iter().position(|p| p.id == id) {
        state.projects[proj_idx].processes.remove(idx);
        state.projects[proj_idx].shared.remove(idx);
    }

    // Persist updated config
    if let Err(e) = save_project(&state.projects[proj_idx].config) {
        state.run_message = Some(format!("error saving: {}", e));
        state.wizard = WizardState::Inactive;
        return;
    }

    state.selected_service = None;
    update_hosts_for_state(state);

    state.wizard = WizardState::Inactive;
    state.run_message = Some(format!("deleted service '{}'", svc_name));
}

// ── Main keyboard handler ──────────────────────────────────────────────────────

async fn handle_key(state: &mut AppState, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => { state.should_quit = true; state.quit_stop_all = true; }
        KeyCode::Char('j') | KeyCode::Down => {
            if state.log_panel_open { state.log_scroll = state.log_scroll.saturating_sub(3); }
            else { navigate_down(state); }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if state.log_panel_open { state.log_scroll = state.log_scroll.saturating_add(3); }
            else { navigate_up(state); }
        }
        KeyCode::Enter => {
            if state.selected_service.is_some() {
                state.wizard = WizardState::ServiceMenu {
                    project_idx: state.selected_project,
                    service_idx: state.selected_service.unwrap(),
                };
            } else {
                toggle_expand(state);
            }
        }
        KeyCode::Char('s') => start_selected(state).await,
        KeyCode::Char('p') => pause_selected(state).await,
        KeyCode::Char('x') => stop_selected(state).await,
        KeyCode::Char('r') => restart_selected(state).await,
        KeyCode::Char('l') => { state.log_panel_open = !state.log_panel_open; state.log_scroll = 0; }
        KeyCode::Char('t') => {
            if let Some(proc) = state.selected_service_proc() {
                state.shell_out_request = Some(ShellOutRequest {
                    working_dir: proc.working_dir.clone(),
                    env: proc.env.clone(),
                    service_name: proc.id.clone(),
                });
            }
        }
        KeyCode::Char('f') => fix_selected(state).await,
        KeyCode::Char('/') => { state.command_focused = true; state.command_input.clear(); }
        KeyCode::Tab => state.log_panel_open = !state.log_panel_open,
        KeyCode::Esc => {
            if state.error_message.is_some() {
                state.error_message = None;
            } else {
                state.selected_service = None;
                state.log_panel_open = false;
            }
        }
        KeyCode::Char('a') => open_add_wizard(state),
        KeyCode::Char('n') => open_new_project_wizard(state),
        KeyCode::Char('e') => open_rename_wizard(state),
        KeyCode::Char('d') => open_delete_confirm(state),
        _ => {}
    }
    Ok(())
}

fn open_rename_wizard(state: &mut AppState) {
    if state.projects.is_empty() { return; }
    let proj_idx = state.selected_project;

    if let Some(svc_idx) = state.selected_service {
        // Rename the selected service — get name from process ID
        if let Some(proc) = state.projects[proj_idx].processes.get(svc_idx) {
            let old_name = proc.id.split('/').last().unwrap_or("").to_string();
            state.wizard = WizardState::RenameService {
                project_idx: proj_idx,
                old_name: old_name.clone(),
                input: old_name,
            };
        }
    } else {
        // Rename the selected project
        let name = state.projects[proj_idx].config.name.clone();
        state.wizard = WizardState::RenameProject { project_idx: proj_idx, input: name };
    }
}

fn open_add_wizard(state: &mut AppState) {
    if state.projects.is_empty() {
        state.wizard = WizardState::AddProjectName { input: String::new() };
    } else {
        // Go directly to add service for the currently selected project
        let project_name = state.projects[state.selected_project].config.name.clone();
        state.wizard = WizardState::AddServicePath { project: project_name, input: String::new(), completions: vec![] };
    }
}

fn open_new_project_wizard(state: &mut AppState) {
    state.wizard = WizardState::AddProjectName { input: String::new() };
}

async fn handle_command_key(state: &mut AppState, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => { state.command_focused = false; state.command_input.clear(); }
        KeyCode::Enter => {
            let query = state.command_input.trim().to_string();
            state.command_focused = false;
            state.command_input.clear();
            if !query.is_empty() {
                if state.no_ai {
                    state.run_message = Some("AI disabled (--no-ai)".to_string());
                } else {
                    state.run_message = Some("thinking...".to_string());
                    let context = build_context(state);
                    if let Some(tx) = state.ai_tx.clone() {
                        tokio::spawn(async move {
                            let result = answer_question(&query, &context).await
                                .unwrap_or_else(|| "couldn't reach Claude proxy".to_string());
                            let _ = tx.send(result).await;
                        });
                    }
                }
            }
        }
        KeyCode::Backspace => { state.command_input.pop(); }
        KeyCode::Char(c) => state.command_input.push(c),
        _ => {}
    }
    Ok(())
}

fn handle_mouse(state: &mut AppState, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollDown => {
            // Scroll down = newer lines = decrease offset from bottom
            if state.log_panel_open { state.log_scroll = state.log_scroll.saturating_sub(3); }
            else { navigate_down(state); }
        }
        MouseEventKind::ScrollUp => {
            // Scroll up = older lines = increase offset from bottom
            if state.log_panel_open { state.log_scroll = state.log_scroll.saturating_add(3); }
            else { navigate_up(state); }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            state.handle_click(mouse.row, mouse.column);
        }
        _ => {}
    }
}

// ── Navigation ─────────────────────────────────────────────────────────────────

fn navigate_down(state: &mut AppState) {
    let proj_count = state.projects.len();
    if proj_count == 0 { return; }

    if let Some(svc_idx) = state.selected_service {
        let svc_count = state.projects[state.selected_project].processes.len();
        if svc_idx + 1 < svc_count {
            state.selected_service = Some(svc_idx + 1);
        } else {
            state.selected_service = None;
            state.selected_project = (state.selected_project + 1).min(proj_count - 1);
        }
    } else if state.projects[state.selected_project].expanded
        && !state.projects[state.selected_project].processes.is_empty()
    {
        state.selected_service = Some(0);
    } else {
        state.selected_project = (state.selected_project + 1).min(proj_count - 1);
    }
}

fn navigate_up(state: &mut AppState) {
    if let Some(svc_idx) = state.selected_service {
        if svc_idx == 0 { state.selected_service = None; }
        else { state.selected_service = Some(svc_idx - 1); }
    } else if state.selected_project > 0 {
        state.selected_project -= 1;
        if state.projects[state.selected_project].expanded {
            let n = state.projects[state.selected_project].processes.len();
            if n > 0 { state.selected_service = Some(n - 1); }
        }
    }
}

fn toggle_expand(state: &mut AppState) {
    if state.selected_service.is_some() { return; }
    if let Some(pv) = state.projects.get_mut(state.selected_project) {
        pv.expanded = !pv.expanded;
        if !pv.expanded { state.selected_service = None; }
    }
}

// ── Service actions ────────────────────────────────────────────────────────────

async fn start_selected(state: &mut AppState) {
    let proj_idx = state.selected_project;

    if let Some(svc_idx) = state.selected_service {
        // Mark proxied and update hosts immediately — don't wait for SSL
        if let Some(proc) = state.projects.get_mut(proj_idx)
            .and_then(|pv| pv.processes.get_mut(svc_idx))
        {
            proc.proxied = true;
        }
        update_hosts_for_state(state);

        // Generate SSL cert for the service's actual domain (not just the project domain)
        if let Some(domain) = state.projects.get(proj_idx).and_then(|pv| {
            let proc = pv.processes.get(svc_idx)?;
            let svc_name = proc.id.split('/').last().unwrap_or(&proc.id);
            if let Some(svc) = pv.config.services.get(svc_name) {
                Some(crate::core::config::resolve_domain(&svc.subdomain, &pv.config.domain))
            } else if svc_name.contains('.') {
                Some(svc_name.to_string())
            } else {
                Some(pv.config.domain.clone())
            }
        }) {
            let tx = state.ai_tx.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = ensure_ssl(&domain) {
                    if let Some(tx) = tx {
                        let _ = tx.blocking_send(format!("⚠️  cert failed for {}: {}", domain, e));
                    }
                }
            });
        }

        // Spawn the process
        if let Some(shared) = state.selected_service_shared() {
            tokio::spawn(async move { let _ = spawn_process(shared).await; });
        }
    } else {
        // Start all services in the project — generate certs for ALL service domains
        let n = state.projects.get(proj_idx).map(|pv| pv.processes.len()).unwrap_or(0);
        let domains: Vec<String> = state.projects.get(proj_idx)
            .map(|pv| pv.config.all_domains())
            .unwrap_or_default();
        for i in 0..n {
            if let Some(proc) = state.projects.get_mut(proj_idx)
                .and_then(|pv| pv.processes.get_mut(i))
            {
                proc.proxied = true;
            }
        }
        update_hosts_for_state(state);
        for domain in domains {
            if !domain.is_empty() {
                let tx = state.ai_tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = ensure_ssl(&domain) {
                        if let Some(tx) = tx {
                            let _ = tx.blocking_send(format!("⚠️  cert failed for {}: {}", domain, e));
                        }
                    }
                });
            }
        }
        if let Some(pv) = state.projects.get(proj_idx) {
            for s in pv.shared.clone() {
                tokio::spawn(async move { let _ = spawn_process(s).await; });
            }
        }
    }
}

/// Remove this service's domain from /etc/hosts so traffic goes to prod,
/// but leave the process running. Re-adding with [s] restores local routing.
async fn pause_selected(state: &mut AppState) {
    let proj_idx = state.selected_project;
    if let Some(svc_idx) = state.selected_service {
        if let Some(proc) = state.projects.get_mut(proj_idx)
            .and_then(|pv| pv.processes.get_mut(svc_idx))
        {
            proc.proxied = false;
        }
        update_hosts_for_state(state);
    } else {
        let n = state.projects.get(proj_idx).map(|pv| pv.processes.len()).unwrap_or(0);
        for i in 0..n {
            if let Some(proc) = state.projects.get_mut(proj_idx)
                .and_then(|pv| pv.processes.get_mut(i))
            {
                proc.proxied = false;
            }
        }
        update_hosts_for_state(state);
    }
}

async fn stop_selected(state: &mut AppState) {
    let proj_idx = state.selected_project;
    if let Some(svc_idx) = state.selected_service {
        // Remove from /etc/hosts first
        if let Some(proc) = state.projects.get_mut(proj_idx)
            .and_then(|pv| pv.processes.get_mut(svc_idx))
        {
            proc.proxied = false;
        }
        update_hosts_for_state(state);
        // Then stop the process
        if let Some(shared) = state.selected_service_shared() {
            let _ = stop_process(shared).await;
        }
    } else {
        let n = state.projects.get(proj_idx).map(|pv| pv.processes.len()).unwrap_or(0);
        let shared_list = state.projects.get(proj_idx)
            .map(|pv| pv.shared.clone())
            .unwrap_or_default();
        for i in 0..n {
            if let Some(proc) = state.projects.get_mut(proj_idx)
                .and_then(|pv| pv.processes.get_mut(i))
            {
                proc.proxied = false;
            }
        }
        update_hosts_for_state(state);
        for s in shared_list { let _ = stop_process(s).await; }
    }
}

async fn restart_selected(state: &mut AppState) {
    if let Some(shared) = state.selected_service_shared() {
        tokio::spawn(async move { let _ = restart_process(shared).await; });
    }
}

async fn fix_selected(state: &mut AppState) {
    let proc_info = state.selected_service_proc().cloned();
    let ai_tx = state.ai_tx.clone();
    let no_ai = state.no_ai;

    if let Some(proc) = proc_info {
        // Check for fixable errors in both crashed status and live stderr
        // (e.g. nodemon keeps the process "running" even after a crash)
        let stderr_text = match &proc.status {
            ProcessStatus::Crashed { ref stderr_tail, .. } => stderr_tail.clone(),
            _ => proc.last_stderr.iter().cloned().collect::<Vec<_>>().join("\n"),
        };
        if !stderr_text.is_empty() {
            let error_kind = categorize_error(&stderr_text);
            if let Some(action) = auto_fix_action(&error_kind, &proc) {
                state.run_message = Some("fixing...".to_string());
                let shared = state.selected_service_shared();
                let tx = ai_tx.clone();
                tokio::spawn(async move {
                    match execute_fix(&action).await {
                        Ok(msg) => {
                            if let Some(tx) = &tx { let _ = tx.send(format!("fixed: {}", msg)).await; }
                            if let Some(s) = shared {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                let _ = restart_process(s).await;
                            }
                        }
                        Err(e) => {
                            if let Some(tx) = &tx { let _ = tx.send(format!("fix failed: {}", e)).await; }
                        }
                    }
                });
            } else if !no_ai {
                state.run_message = Some("asking claude...".to_string());
                let service = proc.id.clone();
                let stderr = stderr_text.clone();
                if let Some(tx) = ai_tx {
                    tokio::spawn(async move {
                        let r = diagnose_crash(&service, &stderr).await
                            .unwrap_or_else(|| "couldn't reach Claude".to_string());
                        let _ = tx.send(r).await;
                    });
                }
            } else {
                state.run_message = Some("unknown error — press [l] for logs".to_string());
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn build_context(state: &AppState) -> String {
    state.projects.iter()
        .flat_map(|pv| pv.processes.iter().map(|p| {
            format!("{}: {} (port {}, {})", p.id, p.status.label(), p.port, p.command)
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

fn expand_path(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.display(), &path[2..]);
        }
    }
    path.to_string()
}

/// Returns matching subdirectories for the current input (shown in UI).
fn path_completions(input: &str) -> Vec<String> {
    let expanded = expand_path(input.trim());
    let path = std::path::Path::new(&expanded);

    let (search_dir, prefix) = if expanded.ends_with('/') || path.is_dir() {
        (path.to_path_buf(), String::new())
    } else {
        let parent = path.parent().map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        (parent, name)
    };

    if !search_dir.exists() { return vec![]; }

    let mut matches: Vec<String> = std::fs::read_dir(&search_dir)
        .into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && name.to_lowercase().starts_with(&prefix.to_lowercase()))
        .map(|name| format!("{}/", search_dir.join(&name).display()))
        .collect();
    matches.sort();
    matches.truncate(8);
    matches
}

/// Tab-completes the input to the longest unambiguous prefix.
fn autocomplete_path(input: &str) -> String {
    let completions = path_completions(input);
    match completions.len() {
        0 => input.to_string(),
        1 => completions[0].clone(),
        _ => {
            // Strip trailing '/' for prefix comparison
            let names: Vec<&str> = completions.iter().map(|s| s.trim_end_matches('/')).collect();
            let common: String = names.iter().skip(1).fold(names[0].to_string(), |acc, s| {
                acc.chars().zip(s.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect()
            });
            if common.len() > names[0].len().saturating_sub(
                names[0].rsplit('/').next().unwrap_or("").len()
            ) {
                common
            } else {
                input.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── wrap_command_with_nvm ────────────────────────────────────────────────

    #[test]
    fn nvm_none_returns_command_unchanged() {
        let result = wrap_command_with_nvm("npm run dev", &None);
        assert_eq!(result, "npm run dev");
    }

    #[test]
    fn nvm_empty_string_returns_command_unchanged() {
        let result = wrap_command_with_nvm("npm run dev", &Some("".to_string()));
        assert_eq!(result, "npm run dev");
    }

    #[test]
    fn nvm_version_wraps_command() {
        let result = wrap_command_with_nvm("npm run dev", &Some("22.9".to_string()));
        assert!(result.contains("nvm use 22.9"));
        assert!(result.contains("npm run dev"));
        assert!(result.starts_with("bash -c '"));
        assert!(result.contains("NVM_DIR"));
    }

    #[test]
    fn nvm_escapes_single_quotes_in_command() {
        let result = wrap_command_with_nvm("echo 'hello world'", &Some("20".to_string()));
        assert!(result.contains("nvm use 20"));
        // Single quotes in the command should be escaped
        assert!(result.contains("'\\''"));
    }

    #[test]
    fn nvm_lts_version() {
        let result = wrap_command_with_nvm("npm start", &Some("lts".to_string()));
        assert!(result.contains("nvm use lts"));
    }
}
