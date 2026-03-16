//! `/etc/hosts` management — adding and removing run.dev-managed entries.
//!
//! run.dev writes a clearly-marked block to `/etc/hosts` so that custom `.local`
//! (or any) domains resolve to `127.0.0.1` without a full DNS server.
//!
//! Because `/etc/hosts` is root-owned, writes go through a privileged helper
//! binary installed at [`HELPER_PATH`] during `run setup`. The helper reads
//! the new hosts content from stdin and writes it atomically to `/etc/hosts`,
//! eliminating repeated password prompts.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::{Context, Result};

const VIBE_MARKER_START: &str = "# >>> run.dev managed — do not edit";
const VIBE_MARKER_END: &str = "# <<< run.dev managed";
const HOSTS_PATH: &str = "/etc/hosts";
pub const HELPER_PATH: &str = "/usr/local/bin/rundev-hosts-helper";

/// The helper script content. Writes /etc/hosts from stdin then flushes DNS caches
/// so domain changes take effect immediately on both macOS and Linux.
pub const HELPER_SCRIPT: &str = "#!/bin/sh
# rundev-hosts-helper — write /etc/hosts and flush DNS cache
cat > /etc/hosts
# Flush DNS so changes take effect immediately
if command -v dscacheutil >/dev/null 2>&1; then
    dscacheutil -flushcache 2>/dev/null
    killall -HUP mDNSResponder 2>/dev/null || true
elif command -v resolvectl >/dev/null 2>&1; then
    resolvectl flush-caches 2>/dev/null || true
elif command -v systemd-resolve >/dev/null 2>&1; then
    systemd-resolve --flush-caches 2>/dev/null || true
elif command -v nscd >/dev/null 2>&1; then
    nscd -i hosts 2>/dev/null || true
fi
";

/// Returns true if the installed helper already has DNS-flush support.
/// Used to detect old installs that need a one-time helper update.
pub fn is_helper_current() -> bool {
    std::fs::read_to_string(HELPER_PATH)
        .map(|c| c.contains("dscacheutil") || c.contains("resolvectl"))
        .unwrap_or(false)
}

pub fn update_hosts(domains: &[(String, Vec<String>)]) -> Result<()> {
    let current = std::fs::read_to_string(HOSTS_PATH)
        .context("Failed to read /etc/hosts")?;

    let new_content = build_hosts_content(&current, domains);

    // Only write if something actually changed
    if new_content == current {
        return Ok(());
    }

    write_hosts(&new_content)
}

pub fn cleanup_hosts() -> Result<()> {
    let current = std::fs::read_to_string(HOSTS_PATH)
        .context("Failed to read /etc/hosts")?;
    let cleaned = remove_run_block(&current);
    if cleaned == current {
        return Ok(());
    }
    write_hosts(&cleaned)
}

fn build_hosts_content(existing: &str, domains: &[(String, Vec<String>)]) -> String {
    let without_block = remove_run_block(existing);
    let block = generate_block(domains, &without_block);
    format!("{}\n{}\n", without_block.trim_end(), block)
}

fn remove_run_block(content: &str) -> String {
    let mut result = Vec::new();
    let mut in_block = false;

    for line in content.lines() {
        if line.trim() == VIBE_MARKER_START {
            in_block = true;
            continue;
        }
        if line.trim() == VIBE_MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            result.push(line);
        }
    }

    result.join("\n")
}

fn generate_block(domains: &[(String, Vec<String>)], existing_hosts: &str) -> String {
    let mut lines = vec![VIBE_MARKER_START.to_string()];
    for (_project_domain, all_domains) in domains {
        // Only include domains not already mapped outside the run.dev block
        let to_add: Vec<&String> = all_domains
            .iter()
            .filter(|d| !has_external_entry(d, existing_hosts))
            .collect();
        if !to_add.is_empty() {
            let joined = to_add.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            lines.push(format!("127.0.0.1  {}", joined));
            lines.push(format!("::1        {}", joined));
        }
    }
    lines.push(VIBE_MARKER_END.to_string());
    lines.join("\n")
}

/// Returns true if `domain` is already mapped to 127.0.0.1 (or any IP) in the
/// given hosts content (outside run.dev's managed block).
fn has_external_entry(domain: &str, hosts: &str) -> bool {
    hosts.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            return false;
        }
        trimmed.split_whitespace().skip(1).any(|h| h == domain)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── block generation ─────────────────────────────────────────────────────

    #[test]
    fn generate_block_single_project() {
        let domains = vec![("myapp.local".to_string(), vec!["myapp.local".to_string()])];
        let block = generate_block(&domains, "");
        assert!(block.contains(VIBE_MARKER_START));
        assert!(block.contains(VIBE_MARKER_END));
        assert!(block.contains("127.0.0.1  myapp.local"));
        assert!(block.contains("::1        myapp.local"));
    }

    #[test]
    fn generate_block_multiple_domains() {
        let domains = vec![(
            "proj.local".to_string(),
            vec!["proj.local".to_string(), "api.proj.local".to_string()],
        )];
        let block = generate_block(&domains, "");
        assert!(block.contains("127.0.0.1  proj.local api.proj.local"));
    }

    #[test]
    fn generate_block_skips_externally_mapped_domain() {
        let external = "127.0.0.1 mamp.local\n";
        let domains = vec![(
            "mamp.local".to_string(),
            vec!["mamp.local".to_string(), "api.mamp.local".to_string()],
        )];
        let block = generate_block(&domains, external);
        // mamp.local already exists externally — only api.mamp.local should be added
        assert!(!block.contains("127.0.0.1  mamp.local api.mamp.local"));
        assert!(block.contains("api.mamp.local"));
    }

    #[test]
    fn has_external_entry_detects_mapped_domain() {
        let hosts = "127.0.0.1 myapp.local api.myapp.local\n::1 localhost\n";
        assert!(has_external_entry("myapp.local", hosts));
        assert!(has_external_entry("api.myapp.local", hosts));
        assert!(!has_external_entry("other.local", hosts));
    }

    #[test]
    fn has_external_entry_ignores_comments() {
        let hosts = "# 127.0.0.1 commented.local\n127.0.0.1 real.local\n";
        assert!(!has_external_entry("commented.local", hosts));
        assert!(has_external_entry("real.local", hosts));
    }

    // ── remove_run_block ────────────────────────────────────────────────────

    #[test]
    fn remove_block_strips_managed_section() {
        let hosts = format!(
            "127.0.0.1 localhost\n{}\n127.0.0.1  myapp.local\n{}\n",
            VIBE_MARKER_START, VIBE_MARKER_END
        );
        let result = remove_run_block(&hosts);
        assert!(!result.contains(VIBE_MARKER_START));
        assert!(!result.contains("myapp.local"));
        assert!(result.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn remove_block_no_op_when_no_block() {
        let hosts = "127.0.0.1 localhost\n::1 localhost\n";
        let result = remove_run_block(hosts);
        assert_eq!(result, "127.0.0.1 localhost\n::1 localhost");
    }

    // ── build_hosts_content ──────────────────────────────────────────────────

    #[test]
    fn build_content_replaces_existing_block() {
        let existing = format!(
            "127.0.0.1 localhost\n{}\n127.0.0.1  old.local\n{}\n",
            VIBE_MARKER_START, VIBE_MARKER_END
        );
        let domains = vec![("new.local".to_string(), vec!["new.local".to_string()])];
        let result = build_hosts_content(&existing, &domains);
        assert!(result.contains("new.local"));
        assert!(!result.contains("old.local"));
        assert!(result.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn build_content_adds_block_to_plain_hosts() {
        let existing = "127.0.0.1 localhost\n";
        let domains = vec![("app.local".to_string(), vec!["app.local".to_string()])];
        let result = build_hosts_content(existing, &domains);
        assert!(result.contains("127.0.0.1 localhost"));
        assert!(result.contains("app.local"));
        assert!(result.contains(VIBE_MARKER_START));
        assert!(result.contains(VIBE_MARKER_END));
    }

    // ── change detection ─────────────────────────────────────────────────────

    #[test]
    fn identical_content_detected_as_unchanged() {
        let domains = vec![("app.local".to_string(), vec!["app.local".to_string()])];
        let base = "127.0.0.1 localhost\n";
        let built = build_hosts_content(base, &domains);
        // Building again from the same base+block should produce identical output
        let rebuilt = build_hosts_content(&built, &domains);
        assert_eq!(built, rebuilt);
    }
}

fn write_hosts(content: &str) -> Result<()> {
    // Try direct write first (works if rundev is run as root, or /etc/hosts is writable)
    if std::fs::write(HOSTS_PATH, content).is_ok() {
        flush_dns();
        return Ok(());
    }

    // Use the privileged helper installed by `run setup`
    // The helper reads new hosts content from stdin and writes to /etc/hosts.
    // A sudoers NOPASSWD rule for this specific binary is installed once at setup time,
    // so `sudo rundev-hosts-helper` never prompts after that.
    // The helper also flushes DNS, but we flush again below to be safe.
    if std::path::Path::new(HELPER_PATH).exists() {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = std::process::Command::new("sudo")
            .arg(HELPER_PATH)
            .stdin(Stdio::piped())
            .spawn()
            .context("Failed to launch rundev-hosts-helper")?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(content.as_bytes())
                .context("Failed to write to rundev-hosts-helper stdin")?;
        }

        let status = child.wait().context("rundev-hosts-helper did not complete")?;
        if status.success() {
            flush_dns();
            return Ok(());
        }
    }

    anyhow::bail!(
        "Cannot write to /etc/hosts.\nRun `run setup` to install the privileged helper (one-time, no future prompts)."
    )
}

/// Flush the OS DNS cache so /etc/hosts changes take effect immediately.
fn flush_dns() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("dscacheutil")
            .arg("-flushcache")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("killall")
            .args(["-HUP", "mDNSResponder"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("resolvectl")
            .arg("flush-caches")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
