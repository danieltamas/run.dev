//! SSL certificate management.
//!
//! Generates locally-trusted certificates for each project domain so browsers
//! show a green padlock without warnings. Strategy (in order):
//!
//! 1. **mkcert** — if installed, generates certs signed by the local mkcert CA
//!    which is trusted by the system keychain → no browser warnings, works with
//!    HSTS-preloaded TLDs like `.app` and `.dev`.
//! 2. **External certs** — copies from MAMP Pro / Homebrew nginx if found.
//! 3. **rcgen fallback** — self-signed cert; browser shows a warning unless the
//!    cert is manually imported into the trust store.
//!
//! Run `mkcert -install` once (included in `rundev setup`) to get silent HTTPS.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn certs_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rundev")
        .join("certs")
}

/// Returns true if a cert already exists for this domain.
pub fn cert_exists(domain: &str) -> bool {
    let dir = certs_dir();
    dir.join(format!("{}.pem", domain)).exists()
        && dir.join(format!("{}-key.pem", domain)).exists()
}

/// Returns true if mkcert binary is in PATH.
pub fn mkcert_available() -> bool {
    std::process::Command::new("mkcert")
        .arg("-CAROOT")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Install mkcert via brew (macOS) or apt/binary (Linux) if not present,
/// then run `mkcert -install` to trust the local CA. Idempotent.
pub fn ensure_mkcert() -> Result<()> {
    if !mkcert_available() {
        // macOS: install via brew
        #[cfg(target_os = "macos")]
        {
            let brew = std::process::Command::new("brew")
                .args(["install", "mkcert", "nss"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .context("brew not found — install Homebrew first: https://brew.sh")?;
            if !brew.success() {
                anyhow::bail!("brew install mkcert failed");
            }
        }

        // Linux: download binary from official releases
        #[cfg(target_os = "linux")]
        {
            let arch = if cfg!(target_arch = "x86_64") { "amd64" } else { "arm64" };
            let url = format!("https://dl.filippo.io/mkcert/latest?for=linux/{}", arch);
            let dl = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "curl -fsSL '{}' -o /tmp/mkcert && sudo install -m 0755 /tmp/mkcert /usr/local/bin/mkcert",
                    url
                ))
                .status()
                .context("Failed to download mkcert")?;
            if !dl.success() {
                anyhow::bail!("mkcert download/install failed");
            }
            // nss tools for Firefox/Chrome on Linux
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg("apt-get install -y libnss3-tools 2>/dev/null || true")
                .status();
        }
    }

    // Install/refresh the local CA in the system trust store
    let status = std::process::Command::new("mkcert")
        .arg("-install")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("mkcert -install failed")?;
    if !status.success() {
        anyhow::bail!("mkcert -install failed");
    }
    Ok(())
}

/// Install the mkcert root CA into the system trust store.
/// Safe to call multiple times — mkcert is idempotent.
pub fn mkcert_install_ca() -> Result<()> {
    ensure_mkcert()
}

pub fn ensure_ssl(domain: &str) -> Result<()> {
    let cert_dir = certs_dir();
    std::fs::create_dir_all(&cert_dir)?;

    let cert_path = cert_dir.join(format!("{}.pem", domain));
    let key_path  = cert_dir.join(format!("{}-key.pem", domain));
    let marker_path = cert_dir.join(format!("{}.mkcert", domain));

    // Ensure mkcert CA is installed — guarded by a sentinel so it only
    // runs once per machine, not on every service start.
    let ca_sentinel = cert_dir.join(".ca_installed");
    if !ca_sentinel.exists() {
        let _ = ensure_mkcert();
        let _ = std::fs::write(&ca_sentinel, "");
    }

    let already_mkcert = marker_path.exists();

    if cert_path.exists() && key_path.exists() {
        if already_mkcert {
            return Ok(());
        }
        if mkcert_available() {
            // Upgrade existing rcgen cert to mkcert-trusted cert
            let _ = std::fs::remove_file(&cert_path);
            let _ = std::fs::remove_file(&key_path);
        } else {
            return Ok(());
        }
    }

    // 1. mkcert — CA-trusted, works with HSTS-preloaded TLDs (.app, .dev, etc.)
    if mkcert_available() {
        generate_with_mkcert(domain, &cert_path, &key_path)?;
        let _ = std::fs::write(&marker_path, "");
        return Ok(());
    }

    // 2. Copy from MAMP / Homebrew nginx if found
    if let Some((ext_cert, ext_key)) = find_external_cert(domain) {
        std::fs::copy(&ext_cert, &cert_path).context("Failed to copy external cert")?;
        std::fs::copy(&ext_key, &key_path).context("Failed to copy external key")?;
        return Ok(());
    }

    // 3. Fallback: self-signed via rcgen (browser will warn for non-.local domains)
    generate_with_rcgen(domain, &cert_path, &key_path)
}

/// Search common locations (MAMP Pro, Homebrew nginx, etc.) for an existing
/// cert+key pair for `domain`. Returns paths to the first matching pair found.
fn find_external_cert(domain: &str) -> Option<(PathBuf, PathBuf)> {
    let candidates = vec![
        PathBuf::from("/Library/Application Support/appsolute/MAMP PRO/conf/ssl"),
        PathBuf::from("/usr/local/etc/nginx/ssl"),
        PathBuf::from("/opt/homebrew/etc/nginx/ssl"),
        PathBuf::from("/etc/ssl/certs"),
    ];

    let name_variants = vec![
        (format!("{}.crt", domain),  format!("{}.key", domain)),
        (format!("{}.pem", domain),  format!("{}-key.pem", domain)),
        (format!("{}.crt", domain),  format!("{}.key.pem", domain)),
        (format!("server.crt"),      format!("server.key")),
    ];

    for base in &candidates {
        for (cert_file, key_file) in &name_variants {
            let c = base.join(cert_file);
            let k = base.join(key_file);
            if c.exists() && k.exists() { return Some((c, k)); }
        }
        let sub = base.join(domain);
        for (cert_file, key_file) in &name_variants {
            let c = sub.join(cert_file);
            let k = sub.join(key_file);
            if c.exists() && k.exists() { return Some((c, k)); }
        }
    }

    None
}

/// Generate a locally-trusted cert using mkcert. Requires mkcert to be installed
/// and `mkcert -install` to have been run (handled by `rundev setup`).
fn generate_with_mkcert(domain: &str, cert_path: &Path, key_path: &Path) -> Result<()> {
    let status = std::process::Command::new("mkcert")
        .arg("-cert-file").arg(cert_path)
        .arg("-key-file").arg(key_path)
        .arg(domain)
        .arg(format!("*.{}", domain))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to run mkcert")?;

    if !status.success() {
        anyhow::bail!("mkcert failed for domain {}", domain);
    }
    Ok(())
}

/// Pure-Rust self-signed cert generation via `rcgen`. No external tools needed.
/// The cert covers both `domain` and `*.domain` (wildcard).
fn generate_with_rcgen(domain: &str, cert_path: &Path, key_path: &Path) -> Result<()> {
    use rcgen::{generate_simple_self_signed, CertifiedKey};

    let subject_alt_names = vec![domain.to_string(), format!("*.{}", domain)];

    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)
        .context("Failed to generate self-signed certificate")?;

    std::fs::write(cert_path, cert.pem()).context("Failed to write certificate")?;
    std::fs::write(key_path, key_pair.serialize_pem()).context("Failed to write private key")?;

    Ok(())
}
