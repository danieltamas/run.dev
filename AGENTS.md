# run.dev — AI-native local dev environment

> Canonical rules are inherited from your harness global agent config (CANON) and are not repeated here.

Single Rust binary that replaces MAMP / nginx / docker-compose for local development. TUI dashboard, automatic `.local` domains with real HTTPS, SNI reverse proxy, process + resource monitoring, and optional Claude-powered crash diagnosis.

- **Binary** — `rundev` (also symlinked as `run.dev`), ~9 MB, crate at repo root
- **Platforms** — macOS (arm64/x86_64) and Linux (x86_64/aarch64). **No Windows** — not a supported target.
- **Static site** — `index.html`, `llms.txt`, `SKILL.md`, `install.sh`, `robots.txt`, `sitemap.xml` served from `getrun.dev`
- **Current version** — **0.3.6** (`Cargo.toml`)

Early software — works end-to-end on macOS, tested on Linux.

---

## Stack & Layout

| Layer | Technology |
|-------|-----------|
| Core | Rust 2021, `tokio`, `clap`, `anyhow`, `serde` + `serde_yaml` |
| TUI | `ratatui` 0.28 + `crossterm` 0.28 (mouse support on) |
| Process mgmt | `tokio::process::Command`, `sysinfo` 0.31 for CPU/RAM |
| Proxy | `tokio::net::TcpListener` (L4 with Host-header peek + SNI), `tokio-rustls` + `rustls` |
| SSL | `rcgen` (pure-Rust self-signed) + external mkcert if available |
| AI (optional) | `reqwest` → local Claude proxy at `http://localhost:3456/v1`, gated by `ai` feature |
| Installer | `install.sh` (bash) — `curl | sh`, rollback on failure |
| Privileged helper | `rundev-hosts-helper` binary + `/etc/sudoers.d/rundev` NOPASSWD rule |

### Directory map

```
src/
  main.rs               CLI entry (`Cli`, `Commands`), dispatches to TUI or sub-command
  app.rs                AppState, event loop, handle_key / handle_wizard_key
  tui.rs                Terminal setup/teardown
  ui/
    mod.rs              Main render
    dashboard.rs        Project/service table, status dots, resource columns
    logs.rs             Log panel (ANSI-stripped)
    command.rs          Bottom command bar (`/` Claude input, error slot)
    wizard.rs           Interactive add-project / add-service / edit-settings modals
  core/
    mod.rs
    scanner.rs          Folder scan → DetectedCommand list (npm/cargo/go/manage.py/…)
    config.rs           Project YAML + global config read/write
    process.rs          spawn_process, monitor, restart, stderr ring buffer, Node preload
    resources.rs        CPU/RAM polling (sysinfo)
    proxy.rs            HTTP host-header + HTTPS SNI reverse proxy, header injection
    hosts.rs            /etc/hosts block management via privileged helper
    ssl.rs              rcgen self-signed certs, mkcert discovery
    preload.rs          Node dotenv/env preload installation
    node_preload.js     JS preload script injected into `node`/`npm`/`npx`/`yarn`/`pnpm`
  ai/
    mod.rs
    mood.rs             Mood enum + categorize_error (pure, no AI needed)
    diagnose.rs         ask_claude → local Claude proxy (optional, feature-gated)
  docs/
    ARCHITECTURE.md     Canonical technical reference

rundev/SKILL.md         Agent skill file shipped to users' projects
SKILL.md                Agent skill file (site copy)
SPEC.md                 Product/implementation spec (working doc)
README.md               User-facing pitch + commands + FAQ
llms.txt                Canonical summary for AI agents (served from getrun.dev)
RELEASE_PROCEDURE.md    Release checklist
install.sh, release.sh  Installer + release script
index.html              Landing page
favicon.svg, og-image.jpg Brand assets
```

### Runtime data

Config directory resolves via `dirs`:
- **macOS:** `~/Library/Application Support/rundev/`
- **Linux:** `~/.config/rundev/`

Contents:

```
projects/<name>.yaml       One file per project — editable by the user
certs/<domain>.pem + -key.pem   SSL material (rcgen or mkcert)
config.yaml                Global config (claude_proxy, theme, premium)
state.json                 PID persistence for background mode
```

**Don't assume `~/.config` on macOS.** Always go through `core::config` helpers (`global_config_path()`, `project_config_dir()`).

---

## Core Architecture

```
Browser ──► (pfctl/iptables)  80 → 1111   (run.dev HTTP listener)
            ────────────────  443 → 1112   (run.dev HTTPS listener, SNI)
                                │
                                ▼
                    Host: api.myapp.local
                                │
                                ▼
                      route table lookup
                                │
                                ▼
                       TcpStream → 127.0.0.1:<service port>
```

**Control plane** = `AppState` in `src/app.rs`. Every keystroke and every tick of the resource/process monitors mutates it; the renderer reads it. Projects/services are persisted to YAML; running PIDs are persisted to `state.json` so `q` can leave services running and `rundev` can reattach next launch (`kill -0` check).

**Privileged surface** is confined to one tiny helper binary (`rundev-hosts-helper`) invoked via NOPASSWD sudo. The main binary never edits `/etc/hosts`, `/etc/pf.anchors`, or iptables directly — it shells out to the helper. Installed once via `rundev setup` / `install.sh`.

### Proxy dual-stack

- L4 HTTP on `1111` — peeks the `Host:` header, picks a service, bidirectional `tokio::io::copy`.
- L7 HTTPS on `1112` — `tokio-rustls` with SNI callback that picks the cert for the requested domain; then proxies cleartext to the upstream port. WebSocket upgrades pass through.
- `/etc/hosts` writes **both IPv4 and IPv6** entries (`127.0.0.1` + `::1`) to defeat Happy Eyeballs falling back to production.

### Node preload (for dotenv + proxy header injection)

When a service command runs `node`/`npm`/`npx`/`yarn`/`pnpm`, `process::spawn_process` sets `NODE_OPTIONS=--require <node_preload.js>`. The JS shim in `src/core/node_preload.js` intercepts `.env` reads and can inject proxy-identifying headers. **This is a cross-language contract**: the Rust side picks when to inject; the JS side implements the merge. Changes to either must be kept in sync (there are unit tests on both sides: `ensure_preload_creates_file()` in Rust, `mergeEnv()` / `isEnvFile()` / `patchResult()` in JS).

### AI is strictly optional

`reqwest` is gated behind the `ai` Cargo feature (on by default). All AI code paths check for a configured `claude_proxy` URL and degrade silently if absent or unreachable. **Never let an AI failure break the dashboard.** See Community 14 in the graph ("AI Graceful Degradation") — this rationale is load-bearing.

---

## God Nodes (core abstractions — touch with care)

From the graphify knowledge graph (`graphify-out/GRAPH_REPORT.md`). File-level hubs first, then functions:

1. `src/core/proxy.rs` — **60 edges**. The reverse-proxy hub. `handle_connection()`, `inject_proxy_headers()`, SNI routing.
2. `src/core/process.rs` — **59 edges**. Process lifecycle. `spawn_process()`, stderr ring buffer, exit-code classification, Node preload hookup.
3. `src/app.rs` — **46 edges**. `AppState`, `handle_key()` (15 edges), `handle_wizard_key()` (20 edges — the most connected function in the codebase).
4. `src/core/scanner.rs` — **36 edges**. `detect_commands()`, port detection, `clean_name()` heuristic.
5. `src/ai/mood.rs` — **34 edges**. `categorize_error()` (13 edges), `Mood` enum, personality messages. **Pure — no AI needed.** `diagnose.rs` is where Claude comes in.
6. `src/core/config.rs` — **28 edges**. `ProjectConfig`, `GlobalConfig`, `effective_domain()`, path resolution.
7. `src/core/hosts.rs` — **19 edges**. `build_hosts_content()`, `cleanup_hosts()`, `flush_dns()`, helper-binary invocation.
8. `Cli` / `main.rs` — 12 edges. Cross-community bridge (betweenness 0.174): every sub-command routes through here.
9. `AppState` — cross-community bridge (betweenness 0.167): the one type that connects TUI State → Port Forwarding → Config → Privileged Helper.

**Heuristic:** if you're editing anything in `proxy.rs`, `process.rs`, `app.rs`, or `scanner.rs`, open the graph first (`graphify-out/graph.html`) to see what else breaks.

---

## Critical Rules

- **Privileged helper is the only code that touches the system.** Never add direct `/etc/hosts`, `sudo`, pfctl, or iptables calls to the main binary. If you need a new privileged capability, add it to the helper and route the call through `hosts.rs` (or a sibling).
- **Proxy and process are hot paths.** `handle_connection()` and `spawn_process()` must not block on disk I/O or AI calls. Anything heavy goes on a spawned task.
- **AI features must degrade gracefully.** If `claude_proxy` is unset or unreachable, every AI code path returns cleanly without user-visible errors. The mood system and template crash messages (`categorize_error()`) work with zero AI. This is enforced by the `ai` feature flag and by the rationale node in Community 14 — don't regress it.
- **TUI layout uses stable 4-slot constraints.** Do not use zero-length `ratatui::Constraint` values: they corrupt coordinates and cause render artifacts. See rationale in Community 16 ("Stable Layout Rationale") — prior incident fixed in `02b7dd3`. If you need a conditionally-hidden panel, use `Min(0)` with sentinel content, not `Length(0)`.
- **Log panel must strip ANSI.** `ui/logs.rs` runs stderr through ANSI-strip before rendering. Raw ANSI wrecks ratatui's cell model. Don't remove that pass.
- **IPv4 + IPv6 both.** Every `/etc/hosts` write must emit both `127.0.0.1 <domain>` and `::1 <domain>`. Chrome's Happy Eyeballs will silently hit production otherwise.
- **Only proxy domains for running services.** `fix: only proxy domains for running services` (`bc45ad4`) — the proxy route table is derived from running state, not from YAML. Stopped services must not shadow real hosts.
- **Absolute paths in project YAML.** Service `path` is resolved at add-time to an absolute path. Do not accept relative paths — configs live in `~/.config/rundev/projects/` and relative resolution is undefined.
- **Free vs premium auto-restart.** Free tier = show diagnosis, wait for `[f]`/`[r]`. Premium tier = exponential backoff auto-restart (1/2/4/8/…/30s, max 5 retries). Don't auto-restart on free tier — the personality UX depends on the user pressing `[f]`.
- **Node preload contract is cross-language.** Any change to the `--require <node_preload.js>` behavior needs matching edits in `src/core/preload.rs` (Rust side — which commands get the env var) AND `src/core/node_preload.js` (JS side — `mergeEnv` / `patchResult` / `isEnvFile`). Both have tests; both suites must pass.
- **The mood system is template-driven, not AI-driven.** `categorize_error()` in `ai/mood.rs` matches stderr against fixed patterns (PortInUse, ModuleNotFound, ConnectionRefused, SyntaxError, OutOfMemory). Claude is only invoked on `Unknown` or on explicit `[/]` / `[f]`. Don't leak AI calls into the template path.
- **No network calls at startup.** Only `rundev update` (and optionally `ask_claude()` — user-triggered) may hit the network. No telemetry, no analytics, no cloud.
- **Windows is not supported.** Don't add `#[cfg(windows)]` branches unless you're genuinely taking on Windows as a target. Keep the code unixy.

---

## Version Bumps

When bumping (e.g. `0.3.0 → 0.4.0`), update **every** reference:

- `Cargo.toml` + `Cargo.lock`
- `README.md` version badge
- `llms.txt`, `SKILL.md`, `rundev/SKILL.md`, `SPEC.md` (if versioned)
- `install.sh` (if it hardcodes a version)
- `index.html` (landing-page version references)
- `RELEASE_PROCEDURE.md` (bump the example)

Run `release.sh` — it does the GitHub release + cross-compile for the four targets (darwin-arm64, linux-amd64, linux-arm64, macos-aarch64 legacy).

## Doc Parity

Five documents describe the same system and must not drift:

- `README.md` — user-facing pitch, commands, FAQ
- `src/docs/ARCHITECTURE.md` — internal design of record
- `SPEC.md` — product/implementation spec
- `llms.txt` — canonical summary for AI agents (served from getrun.dev)
- `SKILL.md` + `rundev/SKILL.md` — agent skill file (site vs shipped copy must match)

When you change a flow, command, keybinding, or public behavior: update all five (or consciously decide one doesn't apply, and say why in the commit).

---

## Testing

| Suite | Count (approx) | Location | Run |
|-------|----------------|----------|-----|
| Rust unit + integration | ~100 | `src/**/*.rs` (inline `#[cfg(test)]`) | `cargo test` |
| Node preload JS | small | inline in `src/core/preload.rs` tests | `cargo test` exercises them via the Rust harness |

No end-to-end framework — manual smoke test is the release gate (see `RELEASE_PROCEDURE.md`).

**Every bugfix needs a regression test.** Community 4 ("Service Name Normalization") and Community 10 ("Byte Formatting Utilities") are almost 100% table-driven unit tests — follow that shape for new pure helpers.

---

## SOPs

### When you touch the proxy (`src/core/proxy.rs`)
1. Keep `handle_connection()` non-blocking. Disk, config, and AI work happen elsewhere.
2. Test HTTP + HTTPS + WebSocket upgrade paths manually — it's easy to regress upgrades.
3. If you change header injection, re-run `inject_proxy_headers()` tests and verify the Node preload side still tags requests consistently.
4. **Release FDs promptly — connections must not linger.** Each proxied connection holds a client+upstream FD pair. Never `try_join!` the two copy directions (it waits for *both*, so when one peer closes the other half lingers until `IDLE_READ_TIMEOUT`, leaking FDs). HTTP drives on `resp` (Connection: close → one response per socket); WebSocket uses `select!` so either side closing tears down both. Leaked FDs → EMFILE ("Too many open files") → every domain dies until restart. This was a repeat incident (`13423dc`, and the v0.3.6 fix). `raise_fd_limit()` in `main` lifts the soft limit to 65536 (macOS defaults as low as 256); keep `MAX_CONCURRENT_CONNS` well under it. `curl http://localhost:1111/__run` shows live `active connections` + `fd soft limit` — a climbing `active` that never falls is a leak.

### When you touch process management (`src/core/process.rs`)
1. `spawn_process()` must set `NODE_OPTIONS=--require <node_preload.js>` for node-family commands (`detect_commands()` / `is_direct_node_command()` — see Community 12).
2. Crashes must capture the last ~20 lines of stderr into the ring buffer so `categorize_error()` can classify them.
3. Stop = SIGTERM → 5s grace → SIGKILL. Don't silently SIGKILL.
4. **Stop the whole process *group*, not just the leader.** Services spawn with `process_group(0)`; a single command (`turbo dev`, `bun dev`, `npm run dev`) fans out into many child dev servers each binding their own port. `kill_pid()` must wait for the entire group to drain (`kill(-pgid, 0)` probe) before/after SIGKILL, and sweep `setsid()` escapees from a pre-kill `collect_descendants()` snapshot. Reaping only the leader leaves subsystem ports hanging after exit — the bug this SOP exists to prevent.

### When you touch the scanner (`src/core/scanner.rs`)
1. Adding a project type (e.g. `deno.json`) requires: detection branch, command list, port heuristic, AND a test case for `clean_name()` if the folder naming is unusual.
2. Port detection priority is: explicit flag → `.env` → `package.json` proxy → framework default. Don't reorder without updating `SPEC.md`.

### When you touch the wizard (`src/ui/wizard.rs` + `handle_wizard_key()` in `src/app.rs`)
1. `handle_wizard_key()` is the single most connected function in the codebase (20 edges). Small state-machine errors cascade. Step through the flow manually before shipping.
2. Every new wizard step needs: a render fn, an input handler branch, a back/cancel path, and an `Esc` path.
3. If you add a field to project/service YAML, update `wizard.rs` **and** the edit-settings flow (recent commit `20993d1` added TUI edit — keep it aligned).

### When you touch `/etc/hosts` (`src/core/hosts.rs`)
1. Always write IPv4 + IPv6 entries.
2. Route through the privileged helper — never `sudo` inline.
3. On cleanup, remove only the run.dev-managed block (between the `# >>> run.dev managed` sentinels). Never rewrite the whole file.

### When you touch SSL (`src/core/ssl.rs`)
1. Generate SAN for both the apex domain and `*.apex` (wildcard). HTTPS SNI depends on it.
2. Check for mkcert before falling back to rcgen — mkcert gives green padlocks without user action.
3. Cert files live in `~/.config/rundev/certs/` (or macOS equivalent). Never in the project directory.

### When you touch AI code (`src/ai/*`)
1. Every path must check `claude_proxy` is configured and return cleanly if not.
2. Don't make AI calls from render or input-handling code. Always spawn a task and update `AppState` on completion.
3. Template errors (`categorize_error()` matches) must **not** call Claude. Only `Unknown` and explicit user triggers do.

### Before every release
1. Bump version everywhere (see Version Bumps).
2. `cargo test && cargo build --release`.
3. Cross-compile for all four targets (`release.sh` handles this — needs `Cross.toml` + Docker).
4. Smoke-test on macOS: `rundev` → add project → add service → crash it → `[f]` fix → `q` → relaunch → service still there.
5. Verify `install.sh` on a throwaway macOS/Linux machine or VM.
6. Tag, push, let GitHub Actions publish the four binaries.

See `RELEASE_PROCEDURE.md` for the full checklist.

---

## Knowledge Graph

`graphify-out/` contains a persistent knowledge graph (`graph.json`, `GRAPH_REPORT.md`, interactive `graph.html`). Use it to orient before exploring unfamiliar code:

- **Before grepping:** open `graphify-out/GRAPH_REPORT.md` god nodes and community hubs. Faster than reading files.
- **Architecture questions:** `graph.html` is interactive — filter by community, trace bridges.
- **After code changes:** run `/Users/danime/.local/pipx/venvs/graphifyy/bin/python -c "from graphify.watch import _rebuild_code; from pathlib import Path; _rebuild_code(Path('.'))"` to keep it current. (graphify lives in the `graphifyy` pipx venv — plain `python3` does not have it.)

Key bridge nodes (high betweenness): `Cli` (0.174), `AppState` (0.167), `load_state()` (0.114). If a change touches any of these, the graph will show every community affected — use it as a checklist for which files to sync.

Interesting clusters to know exist:
- C2 **TUI Application State** — `AppState`, handlers, click routing
- C3 **Mood & Error Categorization** — `categorize_error`, mood transitions, template messages
- C7 **External Services & Integrations** — Claude proxy, privileged helper, pfctl/iptables, project scanner bundled as one cluster
- C12/C13 **Node Preload** — the cross-language Rust↔JS contract lives here
- C14 **AI Graceful Degradation** — the "AI is optional" rationale node; don't regress it
- C16 **Stable Layout Rationale** — zero-length ratatui constraints are banned; history tied to commit `02b7dd3`
