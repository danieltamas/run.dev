# Release Procedure — run.dev

## Quick release (automated)

```bash
./release.sh <version> "<commit message>"
```

Example:
```bash
./release.sh 0.3.0 "feat: new wizard flow"
```

The script handles everything: version bump across all files, Cargo.lock update, tests, macOS build, Linux build (Docker), commit, and tag. At the end it prints the push + release commands for you to run manually (SSH agent needs your approval).

Dry run (bumps files but skips builds/commit):
```bash
./release.sh 0.3.0 "feat: whatever" --dry-run
```

### After the script finishes

Run the three commands it prints:

```bash
git push origin main
git push origin v0.3.0
gh release create v0.3.0 --title "v0.3.0" --notes "feat: whatever" rundev-darwin-arm64 rundev-linux-amd64
```

Then clean up:
```bash
rm -f rundev-darwin-arm64 rundev-linux-amd64
```

---

## Prerequisites

- Rust toolchain (rustup)
- Docker Desktop installed and **running**
- `gh` CLI authenticated (`gh auth status`)
- Clean working tree on `main`

The script checks all of these and fails early with a clear message if something's missing.

---

## What the script does (step by step)

1. Validates version format (semver), checks prerequisites
2. Bumps version in all 9 files:

| File | What changes |
|------|-------------|
| `Cargo.toml` | `version = "X.Y.Z"` |
| `src/main.rs` | clap `version = "X.Y.Z"` |
| `README.md` | Badge + ASCII art |
| `SKILL.md` | `**Version**: X.Y.Z` |
| `rundev/SKILL.md` | Frontmatter + body |
| `src/docs/ARCHITECTURE.md` | `**Version**: X.Y.Z` |
| `index.html` | Terminal mock |
| `src/core/process.rs` | Test string |

3. `cargo update --workspace` — syncs Cargo.lock
4. `cargo test` — catches breakage
5. `cargo build --release` → copies `rundev-darwin-arm64`
6. Docker Linux build → copies `rundev-linux-amd64`
7. Rebuilds macOS (Docker overwrites `target/`)
8. Commits all bumped files
9. Tags `vX.Y.Z`
10. Prints push + release commands

`src/ui/dashboard.rs` reads version via `env!("CARGO_PKG_VERSION")` at compile time — no manual edit needed.

---

## Version numbering (semver)

- **Patch** (0.2.1): bug fixes, typos
- **Minor** (0.3.0): new features, CLI commands, wizard steps
- **Major** (1.0.0): stable public release, breaking config changes

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Docker build fails "requires rustc 1.88" | Update the rust image tag in `release.sh` |
| SSH push fails (Secretive) | Push manually from a terminal with the agent active |
| `gh release create` denied | `gh auth login` |
| Straggler version references | Script warns you — update manually and re-run |
| Docker not running | Start Docker Desktop, then re-run |

---

## Manual release (without the script)

If you need to release without the script, follow the steps above manually. The key thing to remember: **build macOS first, copy the binary, then Docker Linux build, copy that binary.** Docker overwrites `target/release/rundev` with a Linux ELF.
