# Release Procedure — run.dev

Step-by-step checklist for cutting a new release. Follow every step in order.

---

## Prerequisites

- Rust toolchain installed (rustup)
- Docker Desktop installed and **running** (for Linux cross-compile)
- `gh` CLI authenticated (`gh auth status`)
- `mkcert` installed (`brew install mkcert`)
- You're on `main`, working tree clean (`git status`)

---

## 1. Bump the version

Update the version string in **all** of these files:

| File | What to change |
|------|----------------|
| `Cargo.toml` | `version = "X.Y.Z"` (line 3) |
| `src/main.rs` | `#[command(... version = "X.Y.Z")]` (line 35) |
| `README.md` | Badge: `version-X.Y.Z-blue` (line 3) |
| `README.md` | ASCII art display: `vX.Y.Z` (line 48) |
| `SKILL.md` | `**Version**: X.Y.Z` |
| `rundev/SKILL.md` | Frontmatter `version:` + body `**Version**:` |
| `src/docs/ARCHITECTURE.md` | `**Version**: X.Y.Z` |
| `index.html` | Terminal mock: `vX.Y.Z` |
| `src/core/process.rs` | Test string in `detect_port_no_false_positive_random_text` |

The dashboard (`src/ui/dashboard.rs`) reads the version at compile time via `env!("CARGO_PKG_VERSION")` — no manual edit needed there.

**Catch stragglers:**
```bash
grep -rn "OLD_VERSION" --include='*.rs' --include='*.md' --include='*.toml' --include='*.html' .
```

## 2. Update Cargo.lock

```bash
cargo update --workspace
```

Verify:
```bash
grep -A1 'name = "rundev"' Cargo.lock
# should show the new version
```

## 3. Run tests

```bash
cargo test
```

The `detect_port_no_false_positive_random_text` test in `src/core/process.rs` contains the version string — if you missed updating it, this will still pass (it's a no-match test), but keep it in sync for correctness.

## 4. Build macOS binary

```bash
cargo build --release
```

Smoke test:
```bash
./target/release/rundev --version   # must print new version
./target/release/rundev doctor      # all checks should pass
```

Copy the binary out:
```bash
cp target/release/rundev rundev-darwin-arm64
```

## 5. Build Linux binary (Docker)

**Docker Desktop must be running.** Verify with `docker info`.

Build order matters — do Linux **after** copying the macOS binary, because the Docker build overwrites `target/release/rundev`:

```bash
docker run --rm -v "$(pwd)":/app -w /app --platform linux/amd64 \
  rust:1.88-slim cargo build --release
```

> **Note:** Dependencies require Rust 1.88+. Using an older image (e.g. `rust:1.87-slim`) will fail.

Copy the Linux binary:
```bash
cp target/release/rundev rundev-linux-amd64
```

Verify both:
```bash
file rundev-darwin-arm64   # Mach-O 64-bit executable
file rundev-linux-amd64    # ELF 64-bit LSB pie executable, x86-64
```

> **After the Linux build, `target/` contains Linux artifacts.** If you need to rebuild for macOS, run `cargo build --release` again.

## 6. Commit

```bash
git add Cargo.toml Cargo.lock src/main.rs README.md SKILL.md rundev/SKILL.md \
  src/docs/ARCHITECTURE.md index.html src/core/process.rs
git commit -m "release: vX.Y.Z"
```

Do **not** commit `rundev-darwin-arm64`, `rundev-linux-amd64`, or `Cross.toml` — these are build artifacts.

## 7. Tag

```bash
git tag -a vX.Y.Z -m "vX.Y.Z"
```

## 8. Push

```bash
git push origin main
git push origin vX.Y.Z
```

Both must succeed before creating the release.

## 9. Create GitHub Release with binaries

```bash
gh release create vX.Y.Z \
  --title "vX.Y.Z" \
  --notes "Release notes here" \
  rundev-darwin-arm64 \
  rundev-linux-amd64
```

This creates the release **and** uploads both binaries as downloadable assets in one command.

Verify the release page:
```bash
gh release view vX.Y.Z
```

Confirm both assets are listed and the download URLs work:
```bash
gh release download vX.Y.Z --pattern "rundev-linux-amd64" --dir /tmp
file /tmp/rundev-linux-amd64
```

## 10. Update the install script

If `install.sh` (served from `getrun.dev`) hardcodes a version or download URL, update it to point to the new release tag. If it uses `latest`, no change needed.

## 11. Verify end-to-end install

```bash
curl -fsSL https://getrun.dev/install.sh | bash
rundev --version   # must show X.Y.Z
```

## 12. Clean up build artifacts

```bash
rm -f rundev-darwin-arm64 rundev-linux-amd64
```

---

## Version numbering (semver)

- **Patch** (0.2.1): bug fixes, typos, minor tweaks
- **Minor** (0.3.0): new features, new CLI commands, new wizard steps
- **Major** (1.0.0): stable public release, breaking config changes

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Docker build fails with "requires rustc 1.88" | Use `rust:1.88-slim` or newer |
| `gh release create` permission denied | Run `gh auth login` |
| SSH push fails (Secretive/key permissions) | Push manually from a terminal with the agent running |
| `cargo test` fails after version bump | Check `src/core/process.rs` test string matches new version |
| `file rundev-linux-amd64` shows Mach-O | You copied before the Docker build finished — rebuild and re-copy |
| Docker socket not found | Start Docker Desktop first |

---

## Checklist (copy into PR description)

```
- [ ] Version bumped in all 9 files
- [ ] `cargo update --workspace`
- [ ] `cargo test` passes
- [ ] macOS build: `cargo build --release` → `rundev-darwin-arm64`
- [ ] Linux build: Docker → `rundev-linux-amd64`
- [ ] `./rundev-darwin-arm64 --version` prints correct version
- [ ] `file` confirms correct binary types (Mach-O / ELF)
- [ ] Committed (no build artifacts)
- [ ] Tagged vX.Y.Z
- [ ] Pushed main + tag
- [ ] GitHub release created with both binaries attached
- [ ] install.sh updated (if needed)
- [ ] End-to-end install verified
- [ ] Build artifacts cleaned up
```
