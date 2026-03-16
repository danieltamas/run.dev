//! # rundev
//!
//! AI-native local dev environment — replaces MAMP/nginx for local development.
//!
//! Manages services, reverse proxy, SSL certificates, and provides Claude AI
//! crash diagnosis, all from a single interactive dashboard.
//!
//! ## Usage
//! ```
//! rundev          # open the dashboard
//! rundev up       # start all services and open the dashboard
//! rundev down     # stop all running services
//! rundev status   # quick status report
//! rundev doctor   # check system dependencies
//! rundev setup    # one-time install of privileged helper + deps
//! ```
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

mod ai;
mod app;
mod core;
mod tui;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use app::{run_app, AppState};
use core::config::load_all_projects;
use core::hosts::{is_helper_current, HELPER_PATH, HELPER_SCRIPT};

#[derive(Parser)]
#[command(name = "rundev", about = "AI-native local dev environment", version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Skip reverse proxy
    #[arg(long, global = true)]
    no_proxy: bool,

    /// Skip SSL setup
    #[arg(long, global = true)]
    no_ssl: bool,

    /// Disable Claude AI integration
    #[arg(long, global = true)]
    no_ai: bool,

    /// Show debug output
    #[arg(long, short, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start all (or one) project services and open TUI
    Up {
        project: Option<String>,
    },
    /// Stop all (or one) project services
    Down {
        project: Option<String>,
    },
    /// Tail logs for a service (hint: use TUI for live logs)
    Logs {
        target: Option<String>,
    },
    /// Quick status (non-TUI)
    Status,
    /// List all projects
    List,
    /// Remove a project
    Remove {
        project: String,
    },
    /// Check system health (ports, hosts, certs)
    Doctor,
    /// Remove hosts entries, stop all services, cleanup state
    Clean,
    /// Install system dependencies and privileged helper (run once after install)
    Setup,
}

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit CryptoProvider before any TLS usage.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // One-time setup: install privileged helper for /etc/hosts management.
    // Also re-runs if the sudoers file is missing pfctl (old installs lacked it).
    if !std::path::Path::new(HELPER_PATH).exists() || !is_setup_current() {
        ensure_hosts_helper()?;
    }

    match cli.command {
        None => {
            let mut state = AppState::new(cli.no_proxy, cli.no_ai);
            state.load_projects();
            let mut terminal = tui::init()?;
            let _guard = tui::TerminalGuard;
            let result = run_app(&mut terminal, state).await;
            drop(_guard);
            result?;
        }

        Some(Commands::Up { project }) => {
            let mut state = AppState::new(cli.no_proxy, cli.no_ai);
            state.load_projects();
            if let Some(ref name) = project {
                state.projects.retain(|p| p.config.name == *name);
            }
            if state.projects.is_empty() {
                eprintln!("No projects found. Open `rundev` and press [a] to create one.");
                return Ok(());
            }
            let mut terminal = tui::init()?;
            let _guard = tui::TerminalGuard;
            let result = run_app(&mut terminal, state).await;
            drop(_guard);
            result?;
        }

        Some(Commands::Down { project }) => {
            let state = core::process::load_state();
            let mut stopped = 0;
            for (id, pid) in &state.pids {
                if let Some(ref name) = project {
                    if !id.starts_with(name.as_str()) { continue; }
                }
                unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
                println!("Stopped {} (pid {})", id, pid);
                stopped += 1;
            }
            if stopped == 0 { println!("No running processes found."); }
            // Clean up /etc/hosts entries and flush DNS so traffic returns to production
            if project.is_none() {
                let _ = core::hosts::cleanup_hosts();
                println!("Cleaned /etc/hosts and flushed DNS.");
            }
        }

        Some(Commands::Status) => cmd_status(),

        Some(Commands::List) => {
            let projects = load_all_projects();
            if projects.is_empty() {
                println!("No projects. Open `rundev` and press [a] to create one.");
            } else {
                for p in &projects {
                    let svc_count = p.services.len();
                    println!("  {}  ({})  {} service{}", p.name, p.domain, svc_count, if svc_count == 1 { "" } else { "s" });
                    for (name, svc) in &p.services {
                        let subdomain = core::config::resolve_domain(&svc.subdomain, &p.domain);
                        let scheme = if core::ssl::cert_exists(&p.domain) { "https" } else { "http" };
                        println!(
                            "    {}  {}://{}  localhost:{}",
                            name, scheme, subdomain, svc.port
                        );
                    }
                }
            }
        }

        Some(Commands::Remove { project }) => {
            let dir = core::config::projects_dir();
            let path = dir.join(format!("{}.yaml", project));
            if path.exists() {
                std::fs::remove_file(&path)?;
                println!("Removed project '{}'", project);
            } else {
                eprintln!("Project '{}' not found", project);
            }
        }

        Some(Commands::Doctor) => cmd_doctor(),

        Some(Commands::Clean) => cmd_clean()?,

        Some(Commands::Setup) => cmd_setup()?,

        Some(Commands::Logs { target }) => {
            let target = target.unwrap_or_else(|| "?".to_string());
            println!("Following logs for {}...", target);
            println!("Tip: use the TUI for live logs — `rundev up` → select service → [l]");
        }
    }

    Ok(())
}

fn cmd_status() {
    let projects = load_all_projects();
    let state = core::process::load_state();

    if projects.is_empty() {
        println!("No projects. Open `rundev` and press [a] to create one.");
        return;
    }

    let total: usize = projects.iter().map(|p| p.services.len()).sum();
    let running = state.pids.len();
    println!();
    println!("😎 run.dev — {}/{} services up", running, total);
    println!();

    for proj in &projects {
        println!("{}  ({} services)", proj.domain, proj.services.len());
        for (name, svc) in &proj.services {
            let subdomain = core::config::resolve_domain(&svc.subdomain, &proj.domain);
            let scheme = if core::ssl::cert_exists(&proj.domain) { "https" } else { "http" };
            let id = format!("{}/{}", proj.name, name);
            let icon = if state.pids.contains_key(&id) { "🟢" } else { "⚫" };
            println!(
                "  {} {:<14} {}://{:<30} localhost:{}",
                icon, name, scheme, subdomain, svc.port
            );
        }
        println!();
    }
}

fn cmd_doctor() {
    println!("=== rundev doctor ===\n");

    let hosts_ok = std::path::Path::new("/etc/hosts").exists();
    println!("hosts:     {}", if hosts_ok { "✅ /etc/hosts exists" } else { "❌ not found" });

    let mkcert_ok = core::ssl::mkcert_available();
    println!("mkcert:    {}", if mkcert_ok { "✅ installed (trusted HTTPS)" } else { "⚠️  not found — run: brew install mkcert && mkcert -install" });

    let port_80 = std::net::TcpListener::bind("127.0.0.1:80").is_ok();
    let port_1111 = std::net::TcpListener::bind("127.0.0.1:1111").is_ok();
    println!("port 80:   {}", if port_80 { "✅ available" } else { "⚠️  in use" });
    println!("port 1111: {}", if port_1111 { "✅ available" } else { "⚠️  in use" });

    let config_dir = core::config::projects_dir();
    let projects = load_all_projects();
    println!("config:    {}", config_dir.display());
    println!("projects:  {}", projects.len());

    for p in &projects {
        let cert_ok = core::ssl::cert_exists(&p.domain);
        println!(
            "  {} {}  certs: {}",
            p.name,
            p.domain,
            if cert_ok { "✅" } else { "⚠️  none" }
        );
    }
}

fn cmd_clean() -> Result<()> {
    println!("Cleaning up run.dev...");
    core::hosts::cleanup_hosts()?;
    println!("✅ Removed /etc/hosts entries");

    let state = core::process::load_state();
    for (id, pid) in &state.pids {
        unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
        println!("Stopped {}", id);
    }

    println!("Done.");
    Ok(())
}

fn cmd_setup() -> Result<()> {
    println!("=== rundev setup ===\n");

    // ── 0. mkcert CA ─────────────────────────────────────────────────────────
    println!("\nSetting up mkcert (trusted local HTTPS)...");
    if !core::ssl::mkcert_available() {
        // Auto-install mkcert via brew on macOS
        let brew_ok = std::process::Command::new("brew")
            .args(["install", "mkcert", "nss"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !brew_ok {
            println!("⚠️  Could not install mkcert — install manually: brew install mkcert && mkcert -install");
        }
    }
    if core::ssl::mkcert_available() {
        match core::ssl::mkcert_install_ca() {
            Ok(_) => println!("✅ mkcert CA trusted — HTTPS will work without browser warnings"),
            Err(e) => println!("⚠️  mkcert -install failed: {}", e),
        }
    }

    // ── 1. Privileged hosts helper ───────────────────────────────────────────
    println!("\nInstalling privileged hosts helper (requires your password once)...");

    let helper_script = HELPER_SCRIPT;
    let sudoers = sudoers_line();

    let tmp_helper  = std::env::temp_dir().join("rundev-hosts-helper");
    let tmp_sudoers = std::env::temp_dir().join("rundev-sudoers");
    std::fs::write(&tmp_helper, helper_script)?;
    std::fs::write(&tmp_sudoers, &sudoers)?;

    let install_cmd = format!(
        "cp '{}' '{}' && chmod 755 '{}' && cp '{}' /etc/sudoers.d/rundev && chmod 440 /etc/sudoers.d/rundev",
        tmp_helper.display(), HELPER_PATH, HELPER_PATH,
        tmp_sudoers.display()
    );

    let status = std::process::Command::new("sudo")
        .args(["sh", "-c", &install_cmd])
        .status()?;

    let _ = std::fs::remove_file(&tmp_helper);
    let _ = std::fs::remove_file(&tmp_sudoers);

    if !status.success() {
        anyhow::bail!("Failed to install privileged helper");
    }
    println!("✅ Privileged hosts helper installed — no more password prompts");

    // ── 3. Port forwarding ───────────────────────────────────────────────────
    println!("\nSetting up port forwarding (80 → 1111, 443 → 1112)...");
    match core::proxy::setup_port_forwarding() {
        Ok(_) => println!("✅ Port forwarding active — browser traffic will route correctly"),
        Err(e) => println!("⚠️  Port forwarding setup failed: {}\n   HTTP proxy will still work on :1111", e),
    }

    // Write sentinel so `rundev` never prompts for helper install again
    let sentinel = setup_sentinel_path();
    if let Some(parent) = sentinel.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&sentinel, "");

    println!("\n✨ run.dev is ready. Run `rundev` to start.\n");
    Ok(())
}

/// Returns true if /etc/sudoers.d/rundev already grants passwordless port-forwarding
/// commands (pfctl on macOS, iptables on Linux). Used to detect stale installs.
/// Path to a user-readable sentinel file written after successful helper install.
/// We can't read /etc/sudoers.d/rundev (root-only 440), so we use this instead.
fn setup_sentinel_path() -> std::path::PathBuf {
    core::config::projects_dir()
        .parent()
        .unwrap_or(&std::path::PathBuf::from("."))
        .join("setup_done")
}

fn is_setup_current() -> bool {
    setup_sentinel_path().exists() && is_helper_current()
}

/// Returns the platform-specific sudoers NOPASSWD line for rundev's privileged operations.
fn sudoers_line() -> String {
    let user = whoami();
    #[cfg(target_os = "macos")]
    return format!("# rundev sudoers\n{} ALL=(ALL) NOPASSWD: {}, /sbin/pfctl\n", user, HELPER_PATH);
    #[cfg(target_os = "linux")]
    return format!("# rundev sudoers\n{} ALL=(ALL) NOPASSWD: {}, /sbin/iptables, /usr/sbin/iptables, /sbin/iptables-save\n", user, HELPER_PATH);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    format!("# rundev sudoers\n{} ALL=(ALL) NOPASSWD: {}\n", user, HELPER_PATH)
}

/// Install the privileged /etc/hosts helper the first time rundev is run.
/// Runs before the TUI so the sudo prompt appears cleanly in the terminal.
fn ensure_hosts_helper() -> Result<()> {
    println!();
    println!("  run.dev needs one-time permission to manage /etc/hosts and port forwarding.");
    println!("  This installs a tiny helper so you'll never be prompted again.");
    println!();

    let helper_script = HELPER_SCRIPT;
    let sudoers = sudoers_line();

    let tmp_helper  = std::env::temp_dir().join("rundev-hosts-helper");
    let tmp_sudoers = std::env::temp_dir().join("rundev-sudoers");
    std::fs::write(&tmp_helper, helper_script)?;
    std::fs::write(&tmp_sudoers, &sudoers)?;

    let install_cmd = format!(
        "cp '{}' '{}' && chmod 755 '{}' && cp '{}' /etc/sudoers.d/rundev && chmod 440 /etc/sudoers.d/rundev",
        tmp_helper.display(), HELPER_PATH, HELPER_PATH,
        tmp_sudoers.display()
    );

    let status = std::process::Command::new("sudo")
        .args(["-p", "  [sudo] password to install rundev helper: ", "sh", "-c", &install_cmd])
        .status()?;

    let _ = std::fs::remove_file(&tmp_helper);
    let _ = std::fs::remove_file(&tmp_sudoers);

    if status.success() {
        println!("  ✅ Helper installed — no more prompts.\n");
    } else {
        eprintln!("  ⚠️  Helper install failed. /etc/hosts updates may require manual steps.");
    }

    // Set up port forwarding (80→8080, 443→8443) so browser traffic reaches the proxy.
    println!("  Setting up port forwarding (80 → 1111, 443 → 1112)...");
    match core::proxy::setup_port_forwarding() {
        Ok(_)  => println!("  ✅ Port forwarding active.\n"),
        Err(e) => eprintln!("  ⚠️  Port forwarding failed: {}\n", e),
    }

    // Write sentinel so we never prompt again on future launches
    let sentinel = setup_sentinel_path();
    if let Some(parent) = sentinel.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&sentinel, "");

    Ok(())
}

fn whoami() -> String {
    std::process::Command::new("whoami")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "nobody".to_string()))
}
