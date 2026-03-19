# Release Procedure — run.dev

Step-by-step checklist for cutting a new release.

---

## 1. Bump the version

Update the version string in **all** of these files:

| File | Location |
|------|----------|
| `Cargo.toml` | `version = "X.Y.Z"` (line 3) |
| `src/main.rs` | `#[command(... version = "X.Y.Z")]` (line 35) |
| `README.md` | Badge: `version-X.Y.Z-blue` (line 3) |
| `README.md` | ASCII art display: `vX.Y.Z` (line 48) |
| `SKILL.md` | `**Version**: X.Y.Z` (line 18) |
| `rundev/SKILL.md` | Frontmatter `version:` + body `**Version**:` |
| `src/docs/ARCHITECTURE.md` | `**Version**: X.Y.Z` |
| `index.html` | Terminal mock: `vX.Y.Z` |
| `src/core/process.rs` | Test string (cosmetic, keep in sync) |

The dashboard (`src/ui/dashboard.rs`) reads the version at compile time via `env!("CARGO_PKG_VERSION")` — no manual edit needed there.

**Tip:** Search the repo for the old version string to catch any stragglers:
```bash
grep -r "OLD_VERSION" --include='*.rs' --include='*.md' --include='*.toml' --include='*.html' .
```

## 2. Update Cargo.lock

```bash
cargo update --workspace
```

This regenerates the lock file with the new package version. Commit it with the version bump.

## 3. Build and verify locally

```bash
# macOS native build
cargo build --release

# Smoke test
./target/release/rundev --version   # should print the new version
./target/release/rundev doctor      # should pass all checks
```

## 4. Cross-compile for Linux

Use Docker (requires Docker Desktop running):

```bash
docker run --rm -v "$(pwd)":/app -w /app --platform linux/amd64 \
  rust:1.88-slim cargo build --release

# The binary lands at target/release/rundev (Linux ELF)
# Copy it somewhere before rebuilding for macOS:
cp target/release/rundev rundev-linux-amd64
```

Then rebuild for macOS:
```bash
cargo build --release
cp target/release/rundev rundev-darwin-arm64
```

## 5. Run tests

```bash
cargo test
```

Fix anything that breaks. The `detect_port_no_false_positive_random_text` test references the version string — make sure it matches.

## 6. Commit the version bump

```bash
git add -A
git commit -m "release: vX.Y.Z"
```

## 7. Tag the release

```bash
git tag -a vX.Y.Z -m "vX.Y.Z"
```

## 8. Push

```bash
git push origin main
git push origin vX.Y.Z
```

## 9. Create GitHub Release

```bash
gh release create vX.Y.Z \
  --title "vX.Y.Z" \
  --notes "Release notes here" \
  rundev-darwin-arm64#rundev-darwin-arm64 \
  rundev-linux-amd64#rundev-linux-amd64
```

Or create it via the GitHub UI — upload the two binaries as release assets.

## 10. Update the install script

If `install.sh` (served from `getrun.dev`) hardcodes a version or download URL, update it to point to the new release tag.

## 11. Verify the install path

```bash
curl -fsSL https://getrun.dev/install.sh | bash
rundev --version
```

---

## Version numbering

We use semver:

- **Patch** (0.2.1): bug fixes, typos, minor tweaks
- **Minor** (0.3.0): new features, new CLI commands, new wizard steps
- **Major** (1.0.0): stable public release, breaking config changes

---

## Checklist (copy-paste for PR/commit)

```
- [ ] Version bumped in all files (see table above)
- [ ] `cargo update --workspace`
- [ ] `cargo test` passes
- [ ] `cargo build --release` succeeds (macOS)
- [ ] Docker Linux build succeeds
- [ ] `rundev --version` prints correct version
- [ ] `rundev doctor` passes
- [ ] Commit + tag
- [ ] Push main + tag
- [ ] GitHub release created with binaries
- [ ] install.sh updated (if needed)
- [ ] Install path verified
```
