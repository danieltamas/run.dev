<p align="center">
  <img src="https://img.shields.io/badge/rust-2021-orange?style=flat-square&logo=rust" alt="Rust 2021" />
  <img src="https://img.shields.io/badge/version-0.1.0-blue?style=flat-square" alt="Version" />
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey?style=flat-square" alt="Platform" />
  <img src="https://img.shields.io/badge/license-MIT-green?style=flat-square" alt="License" />
  <img src="https://img.shields.io/badge/AI-Claude%20powered-blueviolet?style=flat-square" alt="AI Powered" />
</p>

<h1 align="center">run.dev</h1>

<h3 align="center">AI-native local dev environment</h3>
<p align="center">
  <em>One dashboard. All your services. Zero config files to babysit.</em>
</p>

---

## The Problem

You're an architect. Or an artisan. Or both. You've got 4 microservices, 2 frontends, a websocket server, and that one Go binary Steve wrote before he left. Every morning you open 6 terminal tabs, type the same commands, forget which port is which, and lose 15 minutes to "address already in use" before your first coffee.

MAMP? Nginx configs? Docker Compose YAML files longer than your lease agreement?

**Nah.**

## What Run.dev Does

Run.dev is a single Rust binary that replaces all of that. It gives you:

- **A nice dashboard** — see every project and service at a glance, start/stop with a keystroke
- **Automatic local domains** — `api.myapp.local`, `frontend.myapp.local`, with real HTTPS
- **Zero-config SSL** — mkcert-trusted certs, with rcgen fallback. No manual cert management
- **Reverse proxy** — SNI-based routing with WebSocket support, from pretty URLs to `localhost:whatever`
- **Process management** — spawn, monitor, restart. CPU and RAM stats per service, live
- **Smart project scanning** — point it at a folder, it figures out `npm run dev` vs `cargo run` vs `go run .`
- **AI crash diagnosis** — when something dies, Claude reads the stderr and tells you what went wrong
- **Personality** — run.dev has moods. When everything works: 😎 *vibing*. When stuff crashes: 💀 *flatlined*

```
  ██████╗ ██╗   ██╗███╗   ██╗  ██████╗ ███████╗██╗   ██╗
  ██╔══██╗██║   ██║████╗  ██║  ██╔══██╗██╔════╝██║   ██║     3/4 services  vibing
  ██████╔╝██║   ██║██╔██╗ ██║  ██║  ██║█████╗  ██║   ██║
  ██╔══██╗██║   ██║██║╚██╗██║  ██║  ██║██╔══╝  ╚██╗ ██╔╝     [a] add service  [n] new project
  ██║  ██║╚██████╔╝██║ ╚████║  ██████╔╝███████╗ ╚████╔╝      [s] start  [x] stop  [r] restart
  ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝  ╚═════╝ ╚══════╝  ╚═══╝       [/] ask the AI anything

  Daniel Tamas
  hello@danieltamas.com                                       2 projects  configured  v0.1.0

  projects ────────────────────────────────────────────────────────────────────────
          service                            url                       local           mem   cpu

  v WAM  2 services
  | * win.wam.app                          https://win.wam.app       localhost:5111   42M  0.0%
  | * api.wam.app                          https://api.wam.app       localhost:4000   38M  2.1%
  v side-project  1 service
  | x backend       bro, backend is ded. port 3000 is already taken.
  |                 press [f] to let me fix it

  ────────────────────────────────────────────────────────────────────────────────
  / ask claude something...
```

## Install

**One line:**

```bash
curl -fsSL https://getrun.dev/install.sh | bash
```

This will:
1. Download (or build from source) the `rundev` binary (also installs `run.dev` as an alias)
2. Install a tiny privileged helper for `/etc/hosts` and `/etc/resolver/` (one-time `sudo`, never again)
3. Set up port forwarding (80 → 1111, 443 → 1112)

**Or build it yourself:**

```bash
git clone https://github.com/nicepkg/run.dev.git
cd run.dev
make install
```

**Requirements:** Rust toolchain. That's it. No Node. No Python. No Docker. No external certificate tools.

## Quick Start

```bash
# Launch the dashboard (both commands work)
rundev
run.dev

# Press [a] to create a project → give it a name → get a .local domain
# Press [a] again on the project → point it at a folder
# Run.dev scans it, suggests a start command, picks a port
# Your service is running. With SSL. On a pretty URL. In 30 seconds.
```

## How It Works

```
Browser                   Run.dev                       Your services
  │                          │                              │
  │  https://api.myapp.local │                              │
  ├─────────────────────────►│  /etc/hosts → 127.0.0.1 + ::1     │
  │                          │  port 443 → 1112 (pfctl)    │
  │                          │  TLS + SNI → pick SSL cert   │
  │                          │  Host header → route lookup  │
  │                          ├─────────────────────────────►│ localhost:4000
  │                          │◄─────────────────────────────┤
  │◄─────────────────────────┤                              │
  │  200 OK (green padlock)  │                              │
```

No Docker network. No Traefik. No nginx.conf. Just a binary that manages your processes and routes your traffic.

## CLI

Both `rundev` and `run.dev` work as the command name.

```bash
rundev                    # Open the TUI dashboard
rundev up [project]       # Start project(s) and open dashboard
rundev down [project]     # Stop project(s)
rundev status             # Quick status check (no TUI)
rundev list               # List all projects and services
rundev doctor             # Health check — ports, certs, hosts, helper
rundev clean              # Stop everything, remove /etc/hosts entries
rundev setup              # Re-install privileged helper + port forwarding
```

**Flags:**

| Flag | What it does |
|------|--------------|
| `--no-proxy` | Skip the reverse proxy |
| `--no-ssl` | Skip SSL certificate setup |
| `--no-ai` | Disable Claude integration |
| `-v` | Verbose/debug output |

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate up/down |
| `Enter` | Expand/collapse project |
| `a` | Add project or service |
| `s` | Start selected service |
| `x` | Stop selected service |
| `r` | Restart selected service |
| `f` | Auto-fix crashed service |
| `l` | Toggle log panel |
| `/` | Ask Claude a question |
| `q` | Quit (services keep running) |
| `Q` | Quit and stop everything |

Mouse works too. Click things. Scroll things. It's 2026.

## Project Detection

Point run.dev at a folder and it knows what to do:

| It finds | It suggests |
|----------|-------------|
| `package.json` | `npm run dev`, `npm start`, etc. |
| `Cargo.toml` | `cargo run` |
| `go.mod` | `go run .` |
| `manage.py` | `python manage.py runserver` |
| `Gemfile` | `bundle exec rails server` |
| `Procfile` | Each process line |
| `docker-compose.yml` | Service commands (runs natively, not in Docker) |

Ports are auto-detected from command flags, `.env` files, `package.json` proxy fields, or framework defaults.

## The Mood System

Run.dev watches all your services and expresses how things are going:

| Mood | Emoji | Meaning |
|------|-------|---------|
| Vibing | 😎 | Everything running, no issues |
| Chill | 😌 | Running, minor warnings |
| Got the Flu | 🤒 | 1-2 services crashed |
| Wounded | 🤕 | More than half are down |
| Flatlined | 💀 | Everything is down |
| Fixing | 🔧 | Auto-restart in progress |

When something crashes, run.dev doesn't just show you an exit code. It reads the stderr, categorizes the error, and gives you a personality-driven message with a suggested fix:

```
✗ api    bro, api is ded. port 4000 is already taken.
         i know what's wrong. press [f] to let me fix it
```

Press `[f]` and it kills the process hogging the port. Press `/` and ask Claude what went wrong.

## AI Integration (Optional)

Run.dev is built to work with [Claude Code](https://docs.anthropic.com/en/docs/claude-code), Anthropic's local CLI agent. When a service crashes, run.dev doesn't just show you the exit code — it sends the stderr to Claude for a real diagnosis. Claude reads the logs, understands the context, and tells you what actually went wrong.

**What you get:**

- **Crash diagnosis** — a service dies, Claude reads the stderr and explains what happened and how to fix it
- **Live debug sessions** — press `/` in the dashboard, ask Claude anything about your running services. It knows your project structure, your routes, your ports — it answers in context
- **Auto-fix suggestions** — for common errors (port conflicts, missing modules, connection failures), run.dev suggests a fix. Press `[f]` and it handles it

Run.dev talks to Claude Code — your code and logs never leave your machine.

Configure it in `~/.config/rundev/config.yaml`:

```yaml
claude_proxy: http://localhost:3456/v1
```

The AI features are entirely optional. The app works perfectly without them. Disable anytime with `--no-ai` or just don't configure the proxy.

## Architecture

See [`src/docs/ARCHITECTURE.md`](src/docs/ARCHITECTURE.md) for the full technical deep-dive — module structure, data flow diagrams, key types, and design decisions.

## Per-Service Node Version

If your services need different Node.js versions, add `node_version` to the service config. Run.dev wraps the start command with `nvm use <version>` automatically — no manual switching.

```yaml
services:
  backend:
    path: /Users/dan/code/myapp/backend
    command: npm run dev
    port: 3000
    node_version: "22.9"       # uses nvm to switch before running
  frontend:
    path: /Users/dan/code/myapp/frontend
    command: npm run dev
    port: 5173
    node_version: "20"         # different version, no problem
```

Requires [nvm](https://github.com/nvm-sh/nvm) installed at `$NVM_DIR` (defaults to `~/.nvm`). If `node_version` is omitted, the command runs with whatever Node is on your PATH.

## Config Files

Everything lives in `~/.config/rundev/` (or `~/Library/Application Support/rundev/` on macOS):

```
~/.config/rundev/
├── projects/           # One YAML per project
│   ├── myapp.yaml
│   └── side-project.yaml
├── certs/              # Auto-generated SSL certificates
│   ├── myapp.local.pem
│   └── myapp.local-key.pem
├── config.yaml         # Global settings (Claude proxy, theme)
└── state.json          # PID persistence for background mode
```

## FAQ

**Do my services stop when I close run.dev?**
No. Press `q` and they keep running in the background. Next time you open run.dev, it reconnects. Press `Q` (capital) to stop everything on exit.

**Does it work with Docker services?**
Run.dev manages processes directly — it doesn't orchestrate containers. If your service runs with a shell command, run.dev can manage it. If it only runs in Docker, you'd run `docker compose up` as the service command.

**Do I need to trust the self-signed certs?**
Run.dev uses mkcert to generate locally-trusted certs — green padlock out of the box, no manual trust needed. If mkcert isn't available, it falls back to rcgen self-signed certs (browsers will show a warning you can click through once).

**Does it work with Chrome's DNS-over-HTTPS?**
Chrome with "Use secure DNS" enabled bypasses `/etc/hosts` entirely. If your local domains aren't resolving, go to `chrome://settings/security` and disable "Use secure DNS". Safari, Firefox, curl, and all other tools work out of the box — run.dev writes both IPv4 and IPv6 entries to prevent Happy Eyeballs from falling back to production addresses.

**What about Windows?**
Not yet. macOS and Linux only. WSL might work but isn't tested.

## License

MIT License — see [LICENSE](LICENSE) for details.

Copyright (c) 2026 Daniel Tamas <hello@danieltamas.com>

---

<p align="center">
  <strong>Built by <a href="mailto:hello@danieltamas.com">Daniel Tamas</a></strong>
  <br />
  <em>Because life's too short for nginx.conf</em>
</p>

<p align="center">
  <a href="https://getrun.dev">getrun.dev</a>
</p>
