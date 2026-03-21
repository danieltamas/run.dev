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
| `AppState` | `app.rs` | Central state — projects, selection, wizard, mood, flags, `shell_out_request` |
| `ProjectView` | `app.rs` | A loaded project with its processes and crash info |
| `ShellOutRequest` | `app.rs` | Carries working dir + env for `[t]` shell-out — suspends TUI, opens `$SHELL` |
| `WizardState` | `app.rs` | Enum — multi-step creation/rename flow |
| `ManagedProcess` | `core/process.rs` | A running service: PID, status, logs, resources, `proxied` flag |
| `SharedProcess` | `core/process.rs` | `Arc<Mutex<ManagedProcess>>` — shared across async tasks |
| `ProcessStatus` | `core/process.rs` | `Stopped \| Starting \| Running \| Crashed \| Restarting` |
| `ProjectConfig` | `core/config.rs` | YAML-serializable project definition |
| `ServiceConfig` | `core/config.rs` | YAML-serializable service definition (owns the domain, optional `node_version` for nvm) |
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
            │     │     └── all services loaded with proxied = true
            │     ├── ensure_ssl() for all service domains (spawn_blocking)
            │     ├── kill_orphaned_pids() from state.json
            │     ├── activate_port_forwarding() — pfctl/iptables (80→1111, 443→1112)
            │     ├── start HTTP proxy (:1111) and HTTPS proxy (:1112)
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
       - Shell: t opens $SHELL at service working dir (TUI suspends, resumes on exit)
       - Wizard: a add project/service, text input flow
       - Command bar: / focus, type question, Enter send to Claude
    3. If shell_out_request is set → suspend TUI, run interactive shell, resume TUI
    4. Process completed async tasks (crashes, AI responses)
    5. Tick resource monitor (sysinfo refresh)
    6. Recalculate mood from aggregate service states
    7. Update proxy route table from running services
    8. Render frame via ratatui
}
```

### Per-Service Routing Lifecycle

Domains live at the **service level**, not the project level. Each service independently controls whether its domain is active in `/etc/hosts` via the `proxied: bool` field on `ManagedProcess`.

```
[s] start service
    ├── proc.proxied = true
    ├── update_hosts_for_state() — adds domain to /etc/hosts immediately
    ├── ensure_ssl(service_domain) — spawn_blocking (never blocks event loop)
    │     ├── ensure_mkcert() if .ca_installed sentinel missing (one-time)
    │     ├── upgrade rcgen → mkcert if no .mkcert marker and mkcert available
    │     └── auto-renew if cert expires within 30 days
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
    ├── DNS: /etc/hosts → 127.0.0.1 + ::1 (blocks both IPv4 and IPv6 production)
    ├── Port forward: pfctl (macOS) or iptables (Linux) — 443 → 1112
    ├── HTTPS proxy (127.0.0.1:1112)
    │     ├── TLS handshake (5s timeout) — SNI resolver reads cert from disk:
    │     │     ├── try win.wam.app.pem (exact match)
    │     │     └── try wam.app.pem (wildcard *.wam.app fallback)
    │     ├── Read HTTP request, extract Host header
    │     ├── Inject X-Forwarded-Proto / X-Forwarded-Host / X-Real-IP headers
    │     ├── RouteTable lookup: win.wam.app → port 5111
    │     └── TCP connect to 127.0.0.1:5111, bidirectional copy
    │
    └── Service responds, browser shows green padlock

Browser: http://win.wam.app/path
    │
    ├── Port forward: pfctl/iptables — 80 → 1111
    ├── HTTP proxy (127.0.0.1:1111)
    │     ├── Domain has cert? → 301 redirect to https://win.wam.app/path
    │     └── No cert → forward directly to service port
```

Debug: `curl http://localhost:1111/__run` prints the live proxy route table.

### Process Lifecycle

```
spawn_process()
    ├── wrap_command_with_nvm(command, node_version)
    │     └── if node_version set → bash -c '. nvm.sh && nvm use VERSION && COMMAND'
    ├── shlex::split(command) → parse shell string safely
    ├── kill_port(port) → clear port if occupied (LISTEN-only, excludes self PID)
    ├── tokio::process::Command::new() → spawn child (.process_group(0), stdin null)
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
| `~/.config/rundev/certs/` | SSL certs (`{domain}.pem`, `{domain}-key.pem`, `{domain}.mkcert` marker, `.ca_installed` sentinel) |
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
  backend:
    path: /Users/dan/code/wam/backend
    command: npm run dev
    port: 3000
    subdomain: backend
    node_version: "22.9"        # uses nvm to switch Node version before running
  frontend:
    path: /Users/dan/code/wam/frontend
    command: npm run dev
    port: 5173
    subdomain: ""               # empty = root domain (wam-platform.local)
```

**`node_version`** (optional) — when set, the service command is wrapped with `bash -c '. "$NVM_DIR/nvm.sh" && nvm use <version> && <command>'`. Requires nvm installed at `$NVM_DIR` (defaults to `~/.nvm`). Accepts any version string nvm understands: `"22.9"`, `"20"`, `"lts"`, etc.

---

## SSL Certificate Strategy

`core/ssl.rs::ensure_ssl(domain)` is always called from `tokio::task::spawn_blocking` — never on the async executor — to avoid blocking the event loop.

**Decision tree per domain:**

1. **`.ca_installed` sentinel missing** → run `ensure_mkcert()` once: install mkcert binary if needed, run `mkcert -install` to trust the local CA, write sentinel. All subprocess output is suppressed.
2. **Cert expires within 30 days** → delete and regenerate (checked via `openssl x509 -enddate`)
3. **`.mkcert` marker exists** → cert already trusted, skip
4. **Cert exists, no marker, mkcert available** → upgrade: delete old rcgen cert, regenerate with mkcert, write marker
5. **No cert + mkcert available** → `mkcert -cert-file ... -key-file ... domain *.domain`, write `.mkcert` marker
6. **No cert + external cert found** — copy from MAMP Pro / Homebrew nginx
7. **Fallback** — `rcgen` self-signed (browser warns for non-.local domains)

### SNI Cert Resolution

The HTTPS proxy reads certs from disk **on every TLS handshake** — no restart needed when certs are regenerated. For `win.wam.app`:
1. Look for `win.wam.app.pem` (exact match)
2. Fall back to `wam.app.pem` (wildcard `*.wam.app` covers subdomains)

### mkcert Auto-Installation

`install.sh` and `rundev setup` both install mkcert atomically before doing anything else:
- **macOS**: `brew install mkcert nss && mkcert -install`
- **Linux**: download binary from dl.filippo.io, `apt-get install libnss3-tools`, `mkcert -install`

`ensure_ssl` also self-heals: if mkcert wasn't present at install time, it installs it on the first service start (guarded by `.ca_installed` sentinel so it only runs once).

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
| `reqwest` (optional, `ai` feature, rustls) | HTTP client for Claude API (uses rustls — no native OpenSSL dependency) |
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

Instead of requiring `sudo` on every hosts update, a small shell script (`/usr/local/bin/rundev-hosts-helper`) is installed once with a NOPASSWD sudoers rule. The app pipes new hosts content to it via stdin. After writing, the helper flushes the DNS cache (macOS: `dscacheutil -flushcache` + `killall -HUP mDNSResponder`, Linux: `resolvectl flush-caches`).

### 3a. Installer Safety

The installer (`install.sh`) is designed to be fully idempotent and safe:

- **Consent screen** — shows all system changes before proceeding (installs, firewall rules, sudoers, hosts)
- **Interactive prompt** — when run via `bash install.sh`, asks `[Y/n]` confirmation
- **Automatic rollback** — if the installer fails at any step, an `EXIT` trap rolls back everything already applied: iptables/pfctl rules, sudoers entries, the hosts helper, and the binary
- **Prebuilt binaries** — downloads from GitHub Releases when available, falls back to building from source (auto-installs Rust + build deps if needed)
- **Localhost-only iptables** — Linux port forwarding rules use `-d 127.0.0.1` to redirect only loopback traffic, never touching outbound internet
- **Version tracking** — each installer revision has a version stamp (e.g., `v2026.03.17-9`) displayed at startup for debugging

### 4. Ring Buffer Logs

Process stdout/stderr are stored in `VecDeque<String>` capped at 100 lines. This prevents unbounded memory growth from chatty services.

### 5. Hot-Updatable Proxy

The `RouteTable` is updated in-place as services start/stop. No proxy restart needed. The proxy reads routes on every incoming request.

### 6. PID Persistence

When the user quits the TUI (`q`), services keep running. On next launch, `state.json` is read and orphaned PIDs are cleaned up. This enables a "background mode" workflow.

### 7. IPv4 + IPv6 Hosts Entries

Every managed domain gets both `127.0.0.1` (IPv4) and `::1` (IPv6) entries in `/etc/hosts`. Without the IPv6 entry, browsers using Happy Eyeballs (RFC 6555) may prefer a cached production IPv6 address over the local IPv4 entry, routing traffic to production despite the hosts file.

> **Note:** Chrome with "Use secure DNS" enabled (DNS-over-HTTPS) bypasses `/etc/hosts` entirely. Users with DoH-compatible DNS servers (1.1.1.1, 8.8.8.8) should disable Secure DNS in `chrome://settings/security` for local domains to resolve correctly.

### 8. SNI-Based Multi-Domain TLS

A single HTTPS listener on port 1112 serves certificates for all service domains using Server Name Indication. `SniCertResolver` reads certs from disk on each handshake (no startup preload), so newly generated mkcert certs are picked up immediately. TLS handshakes have a 5-second timeout. ALPN is configured for `http/1.1` only (h2 was removed — the proxy handles HTTP/1.1 byte-level forwarding and advertising h2 caused binary frame corruption).

### 9. Personality-Driven UX

Error messages use a casual tone with actionable suggestions:
- `"bro, api is ded. port 4000 is already taken. press [f] to let me fix it"`
- Mood system (6 states with emojis) reflects aggregate health at a glance

---

## CLI Commands

Both `rundev` and `run.dev` work (symlink installed by `make install` / `install.sh`).

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
| `t` | Open `$SHELL` at service working dir (TUI suspends, resumes on exit) |
| `f` | Execute auto-fix for crashed service |
| `l` | Toggle log panel (j/k or scroll wheel scrolls; newest lines at bottom) |
| `/` | Focus command bar (ask Claude a question) |
| `Tab` | Cycle panels |
| `Esc` | Cancel/deselect/close |
| `q` | Quit TUI (services keep running) |
| `Q` | Quit and stop all services |

Mouse: click rows to select/expand, scroll wheel scrolls logs or navigates list.

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

- **Version**: 0.2.3 (early alpha)
- **Edition**: Rust 2021
- **Platforms**: macOS (pfctl) and Linux (iptables)
- **Tests**: Unit tests in config.rs, hosts.rs, resources.rs
- **Roadmap**: 3 phases detailed in SPEC.md
