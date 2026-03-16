# Architecture — Run.dev (rundev)

> **Central reference for all agents working on this repo.**
> Read this first. It covers what the project does, how it's structured, how data flows, and where to find things.

---

## What This Project Is

**Run.dev** (binary: `rundev`) is an AI-native local development environment manager written in Rust. It replaces manual MAMP/nginx configuration by providing:

- A **TUI dashboard** (ratatui + crossterm) to manage multiple projects and their services
- **Per-service domain routing** via `/etc/hosts` management — each service independently controls whether its domain resolves locally or hits production DNS
- **Reverse HTTP/HTTPS proxy** with SNI-based routing and mkcert-trusted SSL certs
- **Process lifecycle management** — spawn, monitor, stop, restart services as child processes
- **Resource monitoring** — per-process CPU% and memory tracking
- **AI-powered crash diagnosis** — optional Claude API integration for error analysis
- **Project scanning** — auto-detects start commands and ports from package.json, Cargo.toml, go.mod, etc.

---

## Project Structure

```
run.dev/
├── Cargo.toml                # Package manifest (binary: rundev, edition 2021)
├── Cargo.lock                # Locked dependencies
├── Makefile                  # Build shortcuts (install, build, test, clean)
├── SPEC.md                   # Detailed product specification and roadmap
├── install.sh                # Cross-platform installer (macOS/Linux)
├── vibe.yaml                 # Example project config
│
├── src/
│   ├── main.rs               # CLI entrypoint — clap parsing, subcommands, setup
│   ├── app.rs                # AppState, event loop, wizard state machine (~1700 lines)
│   ├── tui.rs                # Terminal init/restore/panic recovery
│   │
│   ├── ui/                   # Rendering layer (pure display, no business logic)
│   │   ├── mod.rs            # Top-level frame composition and layout
│   │   ├── dashboard.rs      # Banner, project tree, status indicators
│   │   ├── wizard.rs         # Modal overlays for project/service creation
│   │   ├── logs.rs           # Live scrollable log panel
│   │   └── command.rs        # AI command bar input
│   │
│   ├── core/                 # Business logic and system integration
│   │   ├── mod.rs            # Module re-exports
│   │   ├── config.rs         # YAML config load/save, directory paths
│   │   ├── scanner.rs        # Project type detection and command inference
│   │   ├── process.rs        # Process spawn/stop/restart, PID persistence
│   │   ├── resources.rs      # CPU/memory monitoring via sysinfo
│   │   ├── hosts.rs          # /etc/hosts management via privileged helper
│   │   ├── proxy.rs          # HTTP/HTTPS reverse proxy with SNI routing
│   │   └── ssl.rs            # Cert management: mkcert (preferred) → rcgen fallback
│   │
│   ├── ai/                   # AI features (feature-gated behind "ai")
│   │   ├── mod.rs            # Module re-exports
│   │   ├── mood.rs           # Mood system, error categorization, crash messages
│   │   └── diagnose.rs       # Claude API integration for crash diagnosis
│   │
│   └── docs/                 # Documentation
│       └── ARCHITECTURE.md   # This file
│
└── target/                   # Build artifacts (gitignored)
```

---

## Key Types and Where They Live

| Type | File | Purpose |
|------|------|---------|
| `AppState` | `app.rs` | Central state — projects, selection, wizard, mood, flags |
| `ProjectView` | `app.rs` | A loaded project with its processes and crash info |
| `WizardState` | `app.rs` | Enum — multi-step creation/rename flow |
| `ManagedProcess` | `core/process.rs` | A running service: PID, status, logs, resources, `proxied` flag |
| `SharedProcess` | `core/process.rs` | `Arc<Mutex<ManagedProcess>>` — shared across async tasks |
| `ProcessStatus` | `core/process.rs` | `Stopped \| Starting \| Running \| Crashed \| Restarting` |
| `ProjectConfig` | `core/config.rs` | YAML-serializable project definition |
| `ServiceConfig` | `core/config.rs` | YAML-serializable service definition (owns the domain) |
| `GlobalConfig` | `core/config.rs` | App-wide settings (Claude proxy, theme, premium) |
| `DetectedCommand` | `core/scanner.rs` | Scanner output: label, command, recommended flag, port |
| `ProxyRoute` | `core/proxy.rs` | Domain → target port mapping |
| `RouteTable` | `core/proxy.rs` | `Arc<RwLock<Vec<ProxyRoute>>>` — hot-updatable routes |
| `ResourceMonitor` | `core/resources.rs` | Wraps sysinfo::System for polling |
| `Mood` | `ai/mood.rs` | `Vibing \| Chill \| GotTheFlu \| Wounded \| Flatlined \| Fixing` |
| `ErrorKind` | `ai/mood.rs` | Crash category: PortInUse, ModuleNotFound, SyntaxError, etc. |
| `FixAction` | `ai/mood.rs` | Auto-fix: KillPort, RunCommand, StartDependency |
| `RunState` | `core/process.rs` | PID persistence for background mode |

---

## Data Flow

### Application Lifecycle

```
main.rs: parse CLI (clap)
    │
    ├── Subcommand (status, doctor, clean, etc.) → run and exit
    │
    └── Default / "up" → launch TUI
            │
            ├── ensure_hosts_helper() — install privileged helper if missing
            ├── AppState::new()
            │     ├── load_projects() from ~/.config/rundev/projects/*.yaml
            │     ├── kill_orphaned_pids() from state.json
            │     ├── start proxy (HTTP :8080, HTTPS :8443)
            │     └── init ResourceMonitor
            │
            ├── tui::init() — raw mode, alternate screen, mouse capture
            │
            ├── run_app() — async event loop (see below)
            │
            └── tui::restore() — cleanup terminal
```

### Event Loop (`app.rs::run_app`)

Runs on a 2-second tick interval:

```
loop {
    1. Poll crossterm events (keyboard, mouse, resize)
    2. Handle input → mutate AppState
       - Navigation: j/k/↑/↓, Enter expand/collapse, Tab cycle
       - Actions: s start, x stop, p pause routing, r restart, f auto-fix
       - Wizard: a add project/service, text input flow
       - Command bar: / focus, type question, Enter send to Claude
    3. Process completed async tasks (crashes, AI responses)
    4. Tick resource monitor (sysinfo refresh)
    5. Recalculate mood from aggregate service states
    6. Update proxy route table from running services
    7. Render frame via ratatui
}
```

### Per-Service Routing Lifecycle

Domains live at the **service level**, not the project level. Each service independently controls whether its domain is active in `/etc/hosts` via the `proxied: bool` field on `ManagedProcess`.

```
[s] start service
    ├── ensure_ssl(domain) — generate cert if missing
    ├── proc.proxied = true
    ├── update_hosts_for_state() — adds domain to /etc/hosts
    └── spawn_process() — start the child process

[p] pause routing
    ├── proc.proxied = false
    ├── update_hosts_for_state() — removes domain from /etc/hosts
    └── process keeps running (traffic goes to production DNS)

[x] stop service
    ├── proc.proxied = false
    ├── update_hosts_for_state() — removes domain from /etc/hosts
    └── kill process (SIGTERM → SIGKILL)

[s] start again
    ├── ensure_ssl(domain) — cert already exists, skipped
    ├── proc.proxied = true
    ├── update_hosts_for_state() — re-adds domain to /etc/hosts
    └── spawn_process()
```

`update_hosts_for_state` rebuilds `/etc/hosts` from scratch based on all currently `proxied = true` services. If no services are proxied, it calls `cleanup_hosts()` to remove all rundev entries.

### Request Routing (Browser → Service)

```
Browser: https://win.wam.app/path
    │
    ├── DNS: /etc/hosts maps win.wam.app → 127.0.0.1
    ├── Port forward: pfctl (macOS) or iptables (Linux) — 443 → 8443
    ├── HTTPS proxy (127.0.0.1:8443)
    │     ├── TLS handshake — SNI selects cert for wam.app
    │     ├── Read HTTP request, extract Host header
    │     ├── RouteTable lookup: win.wam.app → port 4000
    │     └── TCP connect to 127.0.0.1:4000, bidirectional copy
    │
    └── Service responds, browser shows green padlock
```

For HTTP (non-SSL): same flow but port 80 → 8080, no TLS.

### Process Lifecycle

```
spawn_process()
    ├── shlex::split(command) → parse shell string safely
    ├── kill_port(port) → clear port if occupied
    ├── tokio::process::Command::new() → spawn child
    ├── Async stdout/stderr readers → ring buffers (100 lines max)
    ├── Port detection: scan first 50 lines for port patterns
    ├── persist_pid() → write to state.json
    └── child.wait() → on exit:
          ├── exit_code != 0 → Crashed { exit_code, stderr_tail }
          │     ├── categorize_error(stderr) → ErrorKind
          │     ├── crash_message() → personality-driven user message
          │     └── (optional) ask_claude() → AI diagnosis
          └── exit_code == 0 → Stopped
```

### Wizard Flow (Project + Service Creation)

```
[a] on empty space → AddProjectName
    → Enter → project created immediately, domain auto-derived as {name}.local
              cert generation kicks off in background async task
              "created {name} — press [a] to add a service"

[a] on project row → AddServicePath (with filesystem autocomplete)
    → Enter → AddServiceName (suggested from folder name)
    → Enter → scanner::detect_commands(path) → list of commands
    → AddServiceCommand (user picks from list or enters custom)
    → Enter → AddServiceSubdomain (e.g., "win" → win.wam.app)
    → Enter → ServiceConfig saved, process spawned, proxy route added
```

Projects do not have a user-visible domain — domain is configured per service via subdomain.

---

## Configuration & Storage

All persistent state lives under the platform config directory:

| Path | Contents |
|------|----------|
| `~/.config/rundev/projects/*.yaml` | One YAML file per project |
| `~/.config/rundev/config.yaml` | Global settings (Claude proxy, theme) |
| `~/.config/rundev/certs/` | SSL certs (`{domain}.pem`, `{domain}-key.pem`, `{domain}.mkcert` marker) |
| `~/.config/rundev/state.json` | PID map for background persistence |

> On macOS, `~/.config/rundev/` resolves to `~/Library/Application Support/rundev/`.

### Project YAML Format

```yaml
name: wam-platform
domain: wam-platform.local   # internal namespace — not user-configurable
services:
  win:
    path: /Users/dan/code/wam/win
    command: npm run dev
    port: 4000
    subdomain: win              # routes win.wam.app
    env:
      NODE_ENV: development
  frontend:
    path: /Users/dan/code/wam/frontend
    command: npm run dev
    port: 5173
    subdomain: ""               # empty = root domain (wam-platform.local)
```

---

## SSL Certificate Strategy

`core/ssl.rs::ensure_ssl(domain)` generates certs in this order:

1. **Already mkcert cert** — `.mkcert` marker file exists → skip, nothing to do
2. **Upgrade rcgen → mkcert** — cert exists but no `.mkcert` marker and mkcert is available → delete old cert, regenerate with mkcert
3. **mkcert** — `mkcert -cert-file ... -key-file ... domain *.domain` → CA-trusted, works with HSTS-preloaded TLDs (`.app`, `.dev`, etc.), no browser warnings
4. **External cert** — copy from MAMP Pro / Homebrew nginx if found on disk
5. **rcgen fallback** — pure-Rust self-signed cert; browser will warn for non-.local domains

### mkcert Installation

`install.sh` and `rundev setup` both install mkcert automatically:
- **macOS**: `brew install mkcert nss` then `mkcert -install`
- **Linux**: apt-get + binary download from dl.filippo.io then `mkcert -install`

`mkcert -install` adds the local CA to the system trust store (Keychain on macOS, NSS on Linux). This is what makes Chrome accept certs for real TLDs like `.app` and `.dev` without warnings.

---

## Dependencies (Why Each Exists)

| Crate | Why |
|-------|-----|
| `tokio` (full) | Async runtime for process I/O, proxy, timers |
| `ratatui` | TUI widget rendering |
| `crossterm` | Terminal raw mode, keyboard/mouse events |
| `serde` + `serde_yaml` | Project config serialization |
| `serde_json` | State persistence (state.json) |
| `clap` | CLI argument parsing and subcommands |
| `sysinfo` | Per-PID CPU and memory stats |
| `reqwest` (optional, `ai` feature) | HTTP client for Claude API |
| `dirs` | Cross-platform config directory resolution |
| `chrono` | Timestamps on process starts |
| `anyhow` | Ergonomic error propagation |
| `tokio-stream` / `futures` | Async stream utilities for event polling |
| `libc` | Unix signals (SIGTERM, SIGKILL, kill -0) |
| `tokio-rustls` + `rustls` | Async TLS for HTTPS proxy |
| `rustls-pemfile` | Parse PEM certificate files |
| `rcgen` | Self-signed cert fallback when mkcert is unavailable |
| `shlex` | Safe shell command string splitting |

---

## Architectural Patterns

### 1. Shared Mutable State via Arc<Mutex<>>

`ManagedProcess` is wrapped in `Arc<Mutex<>>` (`SharedProcess`) so that:
- The event loop can read status for rendering
- Async stdout/stderr readers can push log lines
- The crash handler can update status

The proxy route table uses `Arc<RwLock<>>` since it's read-heavy.

### 2. Feature-Gated AI

The `ai` feature (default: on) gates `reqwest` and Claude integration. When disabled (`--no-ai` or compiled without), the app works identically minus AI diagnosis. `diagnose.rs` functions return `Option<String>` — `None` means unavailable.

### 3. Privileged Helper for /etc/hosts

Instead of requiring `sudo` on every hosts update, a small shell script (`/usr/local/bin/rundev-hosts-helper`) is installed once with a NOPASSWD sudoers rule. The app pipes new hosts content to it via stdin.

### 4. Ring Buffer Logs

Process stdout/stderr are stored in `VecDeque<String>` capped at 100 lines. This prevents unbounded memory growth from chatty services.

### 5. Hot-Updatable Proxy

The `RouteTable` is updated in-place as services start/stop. No proxy restart needed. The proxy reads routes on every incoming request.

### 6. PID Persistence

When the user quits the TUI (`q`), services keep running. On next launch, `state.json` is read and orphaned PIDs are cleaned up. This enables a "background mode" workflow.

### 7. SNI-Based Multi-Domain TLS

A single HTTPS listener on port 8443 serves certificates for all project domains using Server Name Indication. Certs are loaded at startup from the certs directory and matched by domain (including wildcard `*.domain`).

### 8. Personality-Driven UX

Error messages use a casual tone with actionable suggestions:
- `"bro, api is ded. port 4000 is already taken. press [f] to let me fix it"`
- Mood system (6 states with emojis) reflects aggregate health at a glance

---

## CLI Commands

```
rundev                    # Open TUI dashboard
rundev up [project]       # Start project(s) and open TUI
rundev down [project]     # Stop project(s)
rundev status             # Quick status (no TUI)
rundev list               # List all projects and their services
rundev remove <project>   # Delete a project config
rundev doctor             # Health check: ports, certs, hosts, helper, mkcert
rundev clean              # Stop everything, remove /etc/hosts entries
rundev setup              # Install privileged helper + mkcert + port forwarding
rundev logs [target]      # Hints to use TUI instead
```

**Flags**: `--no-proxy`, `--no-ssl`, `--no-ai`, `-v/--verbose`

---

## Keyboard Shortcuts (TUI)

| Key | Action |
|-----|--------|
| `j`/`k`/`↑`/`↓` | Navigate project/service list |
| `Enter` | Expand/collapse project |
| `a` | Add project (nothing selected) or service (project selected) |
| `s` | Start selected service |
| `x` | Stop selected service |
| `p` | Pause routing — removes domain from /etc/hosts, process keeps running |
| `r` | Restart selected service |
| `f` | Execute auto-fix for crashed service |
| `l` | Toggle log panel |
| `/` | Focus command bar (ask Claude a question) |
| `Tab` | Cycle panels |
| `Esc` | Cancel/deselect/close |
| `q` | Quit TUI (services keep running) |
| `Q` | Quit and stop all services |

Mouse: click rows to select/expand, scroll wheel for navigation.

---

## Scanner: Supported Project Types

`core/scanner.rs::detect_commands()` checks for (in order):

1. **Node.js** — `package.json` scripts (dev, start, serve, watch)
2. **Rust** — `Cargo.toml` → `cargo run` / `cargo run --release`
3. **Go** — `go.mod` → `go run .`
4. **Procfile** — extracts process lines
5. **Django** — `manage.py` → `python manage.py runserver`
6. **Rails** — `Gemfile` → `bundle exec rails server`
7. **Docker Compose** — `docker-compose.yml` → extracts service commands

Port inference: command flags → `.env PORT=` → `package.json proxy` → framework defaults.

---

## Project Status

- **Version**: 0.1.0 (early alpha)
- **Edition**: Rust 2021
- **Platforms**: macOS (pfctl) and Linux (iptables)
- **Tests**: Unit tests in config.rs, hosts.rs, resources.rs
- **Roadmap**: 3 phases detailed in SPEC.md
