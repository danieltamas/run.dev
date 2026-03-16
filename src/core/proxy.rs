//! Reverse proxy — routes incoming HTTP/HTTPS requests to local service ports.
//!
//! Listens on port 80 (and 443 when SSL is available) and forwards each request
//! to the correct service based on the `Host` header. The route table is stored
//! in a shared [`RouteTable`] so it can be hot-updated at runtime as services
//! start, stop, or are reconfigured without restarting the proxy.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::Result;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;

#[derive(Debug, Clone)]
pub struct ProxyRoute {
    pub domain: String,
    pub target_port: u16,
}

pub type RouteTable = Arc<RwLock<Vec<ProxyRoute>>>;

pub async fn run_proxy(routes: RouteTable, listen_port: u16) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", listen_port)).await?;
    tracing_or_eprintln(format!("Proxy listening on 127.0.0.1:{}", listen_port));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, routes).await;
                });
            }
            Err(e) => {
                eprintln!("Proxy accept error: {}", e);
            }
        }
    }
}

pub async fn run_https_proxy(routes: RouteTable, listen_port: u16) -> Result<()> {
    let cert_dir = crate::core::ssl::certs_dir();
    let resolver = SniCertResolver::new(&cert_dir);
    if resolver.is_empty() {
        eprintln!("HTTPS proxy: no certs found in {}, skipping TLS listener", cert_dir.display());
        return Ok(());
    }

    let tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(format!("127.0.0.1:{}", listen_port)).await?;
    tracing_or_eprintln(format!("HTTPS proxy listening on 127.0.0.1:{}", listen_port));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let acceptor = acceptor.clone();
                let routes = routes.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => { let _ = handle_connection(tls_stream, routes).await; }
                        Err(_) => {} // TLS handshake failed (e.g. unknown SNI) — ignore
                    }
                });
            }
            Err(e) => {
                eprintln!("HTTPS proxy accept error: {}", e);
            }
        }
    }
}

async fn handle_connection<S>(mut client: S, routes: RouteTable) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Read the HTTP request to get the Host header
    let mut buf = vec![0u8; 16384];
    let n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request = &buf[..n];

    // Debug status page — curl http://localhost:1111/__run
    if request.starts_with(b"GET /__run") {
        let table = routes.read().await;
        let body = if table.is_empty() {
            "run.dev proxy: no routes registered yet (services still starting?)\n".to_string()
        } else {
            let lines: Vec<String> = table.iter()
                .map(|r| format!("  {} -> :{}", r.domain, r.target_port))
                .collect();
            format!("run.dev proxy routes:\n{}\n", lines.join("\n"))
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        let _ = client.write_all(response.as_bytes()).await;
        return Ok(());
    }

    let host = extract_host_header(request);

    // Find matching route
    let (target_port, route_table_dump) = {
        let table = routes.read().await;
        let port = find_route(&table, host.as_deref());
        let dump: Vec<String> = table.iter()
            .map(|r| format!("{}:{}", r.domain, r.target_port))
            .collect();
        (port, dump)
    };

    let target_port = match target_port {
        Some(p) => p,
        None => {
            let body = format!(
                "run.dev proxy: no route for '{}'\nknown routes: {}",
                host.as_deref().unwrap_or("(no host header)"),
                if route_table_dump.is_empty() {
                    "none yet — service may still be starting".to_string()
                } else {
                    route_table_dump.join(", ")
                }
            );
            let response = format!(
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            let _ = client.write_all(response.as_bytes()).await;
            return Ok(());
        }
    };

    // Connect to target service — return 503 if it's not up yet
    let mut upstream = match TcpStream::connect(format!("127.0.0.1:{}", target_port)).await {
        Ok(s) => s,
        Err(e) => {
            let body = format!(
                "run.dev proxy: service '{}' is not reachable on port {}\nerror: {}",
                host.as_deref().unwrap_or("?"), target_port, e
            );
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            let _ = client.write_all(response.as_bytes()).await;
            return Ok(());
        }
    };

    // Forward the initial request bytes
    upstream.write_all(request).await?;

    // Bidirectional pipe — use copy_bidirectional so neither direction is
    // dropped prematurely (a bare `select!` would cancel the slower half).
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = upstream.split();

    let _result = tokio::try_join!(
        tokio::io::copy(&mut client_read, &mut upstream_write),
        tokio::io::copy(&mut upstream_read, &mut client_write),
    );

    Ok(())
}

// ── SNI cert resolver ──────────────────────────────────────────────────────────

#[derive(Debug)]
/// Resolves TLS certificates per SNI by reading from disk on each handshake.
/// This means newly generated or upgraded certs (e.g. rcgen → mkcert) are
/// picked up immediately without restarting the proxy.
struct SniCertResolver {
    cert_dir: std::path::PathBuf,
}

impl SniCertResolver {
    fn new(cert_dir: &std::path::Path) -> Self {
        Self { cert_dir: cert_dir.to_path_buf() }
    }

    fn is_empty(&self) -> bool {
        std::fs::read_dir(&self.cert_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    }

    fn load_for_domain(&self, domain: &str) -> Option<Arc<CertifiedKey>> {
        let cert_path = self.cert_dir.join(format!("{}.pem", domain));
        let key_path  = self.cert_dir.join(format!("{}-key.pem", domain));
        if cert_path.exists() && key_path.exists() {
            load_certified_key(&cert_path, &key_path)
        } else {
            None
        }
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = client_hello.server_name()?;

        // Exact match
        if let Some(ck) = self.load_for_domain(sni) {
            return Some(ck);
        }

        // Wildcard fallback: "win.wam.app" → try "wam.app" cert (covers *.wam.app)
        if let Some(dot) = sni.find('.') {
            let parent = &sni[dot + 1..];
            if let Some(ck) = self.load_for_domain(parent) {
                return Some(ck);
            }
        }

        None
    }
}

fn load_certified_key(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Option<Arc<CertifiedKey>> {
    use rustls::pki_types::CertificateDer;

    let cert_bytes = std::fs::read(cert_path).ok()?;
    let key_bytes = std::fs::read(key_path).ok()?;

    let cert_chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_bytes.as_slice())
            .filter_map(|r| r.ok())
            .map(|c| c.into_owned())
            .collect();

    if cert_chain.is_empty() {
        return None;
    }

    let key = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .ok()??;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key).ok()?;

    Some(Arc::new(CertifiedKey::new(cert_chain, signing_key)))
}

fn extract_host_header(request: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(request).ok()?;
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("host:") {
            let host = line[5..].trim().to_string();
            // Strip port if present
            let host = host.split(':').next().unwrap_or(&host).to_string();
            return Some(host);
        }
    }
    None
}

fn find_route(routes: &[ProxyRoute], host: Option<&str>) -> Option<u16> {
    let host = host?;
    // Exact match first
    for route in routes {
        if route.domain.eq_ignore_ascii_case(host) {
            return Some(route.target_port);
        }
    }
    // Wildcard: if host ends with .local and no exact match, try base domain
    None
}

/// Activate port forwarding so the proxy receives browser traffic on 80/443
/// without binding privileged ports. No-op if setup hasn't been run yet.
pub fn activate_port_forwarding() {
    #[cfg(target_os = "macos")]
    activate_pf();

    #[cfg(target_os = "linux")]
    activate_iptables();
}

#[cfg(target_os = "macos")]
fn activate_pf() {
    const PF_ANCHOR: &str = "/etc/pf.anchors/rundev";
    const PF_CONF: &str = "/etc/pf.conf";

    let rules = "rdr pass on lo0 proto tcp from any to any port 80 -> 127.0.0.1 port 1111\n\
                 rdr pass on lo0 proto tcp from any to any port 443 -> 127.0.0.1 port 1112\n";

    // Only rewrite anchor if the rules have changed (avoids unnecessary sudo prompts)
    let current = std::fs::read_to_string(PF_ANCHOR).unwrap_or_default();
    if current.trim() != rules.trim() {
        let tmp = std::env::temp_dir().join("rundev-pf-anchor");
        if std::fs::write(&tmp, rules).is_ok() {
            let _ = std::process::Command::new("sudo")
                .args(["-n", "cp", &tmp.to_string_lossy(), PF_ANCHOR])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_file(&tmp);
        }
    }

    // Ensure pf.conf references the rundev rdr-anchor — without this the anchor rules never fire
    let pf_content = std::fs::read_to_string(PF_CONF).unwrap_or_default();
    if !pf_content.contains("rdr-anchor \"rundev\"") {
        let addition = "\n# run.dev port forwarding\nrdr-anchor \"rundev\"\nanchor \"rundev\"\n";
        let new_content = format!("{}{}", pf_content.trim_end(), addition);
        let tmp = std::env::temp_dir().join("rundev-pf-conf");
        if std::fs::write(&tmp, &new_content).is_ok() {
            let _ = std::process::Command::new("sudo")
                .args(["-n", "cp", &tmp.to_string_lossy(), PF_CONF])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_file(&tmp);
        }
        // Reload full pf config so the new rdr-anchor directive takes effect
        let _ = std::process::Command::new("sudo")
            .args(["-n", "pfctl", "-ef", PF_CONF])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    // Load run.dev's anchor rules
    if std::path::Path::new(PF_ANCHOR).exists() {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "pfctl", "-a", "rundev", "-f", PF_ANCHOR])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(target_os = "linux")]
fn activate_iptables() {
    // Only add the rule if it isn't already present
    let exists = std::process::Command::new("sudo")
        .args(["iptables", "-t", "nat", "-C", "OUTPUT", "-p", "tcp",
               "--dport", "80", "-j", "REDIRECT", "--to-port", "1111"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !exists {
        let _ = std::process::Command::new("sudo")
            .args(["iptables", "-t", "nat", "-A", "OUTPUT", "-p", "tcp",
                   "--dport", "80", "-j", "REDIRECT", "--to-port", "1111"])
            .status();
        let _ = std::process::Command::new("sudo")
            .args(["iptables", "-t", "nat", "-A", "OUTPUT", "-p", "tcp",
                   "--dport", "443", "-j", "REDIRECT", "--to-port", "1112"])
            .status();
    }
}

/// Install port-forwarding rules persistently. Called once from `rundev setup`.
pub fn setup_port_forwarding() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    return setup_pf();

    #[cfg(target_os = "linux")]
    return setup_iptables();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("Port forwarding setup is not supported on this platform")
}

#[cfg(target_os = "macos")]
fn setup_pf() -> anyhow::Result<()> {
    const PF_ANCHOR: &str = "/etc/pf.anchors/rundev";
    const PF_CONF: &str = "/etc/pf.conf";

    let anchor_rules = "rdr pass on lo0 proto tcp from any to any port 80 -> 127.0.0.1 port 1111\n\
                        rdr pass on lo0 proto tcp from any to any port 443 -> 127.0.0.1 port 1112\n";

    let tmp = std::env::temp_dir().join("rundev-pf-anchor");
    std::fs::write(&tmp, anchor_rules)?;
    let install_cmd = format!(
        "cp '{}' '{}' && chmod 644 '{}'",
        tmp.display(), PF_ANCHOR, PF_ANCHOR
    );
    let status = std::process::Command::new("sudo")
        .args(["sh", "-c", &install_cmd])
        .status()?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        anyhow::bail!("Failed to install pfctl anchor");
    }

    let pf_content = std::fs::read_to_string(PF_CONF).unwrap_or_default();
    if !pf_content.contains("anchor \"rundev\"") {
        let addition = "\n# run.dev port forwarding\nrdr-anchor \"rundev\"\nanchor \"rundev\"\n";
        let new_content = format!("{}{}", pf_content, addition);
        let tmp2 = std::env::temp_dir().join("rundev-pf-conf");
        std::fs::write(&tmp2, &new_content)?;
        let copy_cmd = format!("cp '{}' '{}'", tmp2.display(), PF_CONF);
        let _ = std::process::Command::new("sudo")
            .args(["sh", "-c", &copy_cmd])
            .status();
        let _ = std::fs::remove_file(&tmp2);
    }

    let _ = std::process::Command::new("sudo")
        .args(["pfctl", "-ef", PF_ANCHOR])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_iptables() -> anyhow::Result<()> {
    // Add rules (activate_iptables checks for duplicates at runtime)
    activate_iptables();

    // Persist across reboots via iptables-persistent if available
    if std::process::Command::new("which")
        .arg("iptables-save")
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let rules_dir = "/etc/iptables";
        let _ = std::process::Command::new("sudo")
            .args(["mkdir", "-p", rules_dir])
            .status();
        let _ = std::process::Command::new("sudo")
            .args(["sh", "-c", &format!("iptables-save > {}/rules.v4", rules_dir)])
            .status();
    }

    Ok(())
}

pub fn new_route_table() -> RouteTable {
    Arc::new(RwLock::new(vec![]))
}

pub async fn update_routes(table: &RouteTable, new_routes: Vec<ProxyRoute>) {
    let mut t = table.write().await;
    *t = new_routes;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(domain: &str, port: u16) -> ProxyRoute {
        ProxyRoute { domain: domain.to_string(), target_port: port }
    }

    // ── extract_host_header ──────────────────────────────────────────────────

    #[test]
    fn extract_host_simple() {
        let req = b"GET / HTTP/1.1\r\nHost: myapp.local\r\nAccept: */*\r\n\r\n";
        assert_eq!(extract_host_header(req), Some("myapp.local".to_string()));
    }

    #[test]
    fn extract_host_with_port_strips_it() {
        let req = b"GET / HTTP/1.1\r\nHost: myapp.local:1111\r\n\r\n";
        assert_eq!(extract_host_header(req), Some("myapp.local".to_string()));
    }

    #[test]
    fn extract_host_case_insensitive_header() {
        let req = b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n";
        assert_eq!(extract_host_header(req), Some("example.com".to_string()));
    }

    #[test]
    fn extract_host_missing_returns_none() {
        let req = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        assert_eq!(extract_host_header(req), None);
    }

    // ── find_route ───────────────────────────────────────────────────────────

    #[test]
    fn find_route_exact_match() {
        let routes = vec![route("api.myapp.local", 3001), route("myapp.local", 3000)];
        assert_eq!(find_route(&routes, Some("api.myapp.local")), Some(3001));
    }

    #[test]
    fn find_route_first_match_wins() {
        let routes = vec![route("app.local", 4000), route("app.local", 9999)];
        assert_eq!(find_route(&routes, Some("app.local")), Some(4000));
    }

    #[test]
    fn find_route_no_match_returns_none() {
        let routes = vec![route("other.local", 3000)];
        assert_eq!(find_route(&routes, Some("app.local")), None);
    }

    #[test]
    fn find_route_none_host_returns_none() {
        let routes = vec![route("app.local", 3000)];
        assert_eq!(find_route(&routes, None), None);
    }

    #[test]
    fn find_route_case_insensitive() {
        let routes = vec![route("MyApp.local", 5000)];
        assert_eq!(find_route(&routes, Some("myapp.local")), Some(5000));
    }

    // ── update_routes ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_routes_replaces_table() {
        let table = new_route_table();
        update_routes(&table, vec![route("a.local", 3000)]).await;
        update_routes(&table, vec![route("b.local", 4000)]).await;
        let t = table.read().await;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].domain, "b.local");
    }
}

fn tracing_or_eprintln(msg: String) {
    eprintln!("{}", msg);
}
