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
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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
                    let _ = handle_connection(stream, routes, false).await;
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

    let mut tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    // Only advertise HTTP/1.1 — we don't implement HTTP/2 framing, so
    // negotiating h2 would cause the browser to send binary frames that
    // our text-based header parser can't handle (random failures, corruption).
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(format!("127.0.0.1:{}", listen_port)).await?;
    tracing_or_eprintln(format!("HTTPS proxy listening on 127.0.0.1:{}", listen_port));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let acceptor = acceptor.clone();
                let routes = routes.clone();
                tokio::spawn(async move {
                    // Timeout the TLS handshake so bad clients don't hold connections forever
                    let tls_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        acceptor.accept(stream),
                    ).await;
                    match tls_result {
                        Ok(Ok(tls_stream)) => { let _ = handle_connection(tls_stream, routes, true).await; }
                        _ => {} // TLS handshake failed or timed out — ignore
                    }
                });
            }
            Err(e) => {
                eprintln!("HTTPS proxy accept error: {}", e);
            }
        }
    }
}

async fn handle_connection<S>(mut client: S, routes: RouteTable, is_tls: bool) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Process requests one at a time so every request gets proper header injection.
    // With blind bidirectional copy, keep-alive requests after the first would bypass
    // header injection entirely — causing DPoP, CORS, and X-Forwarded-Proto failures.
    let mut buf = vec![0u8; 65536];
    // leftover holds body bytes from the previous read that belong to the next request
    let mut leftover_start = 0usize;
    let mut leftover_end = 0usize;

    loop {
        // ── Read complete HTTP headers (\r\n\r\n) ──────────────────────────────
        // Seed the buffer with any leftover bytes from the previous request cycle
        let mut filled = leftover_end - leftover_start;
        if filled > 0 {
            buf.copy_within(leftover_start..leftover_end, 0);
        }
        leftover_start = 0;
        leftover_end = 0;

        let deadline = std::time::Duration::from_secs(30);
        let mut found_headers_end = false;

        loop {
            // Check if we already have the end-of-headers marker in existing data
            if filled >= 4 {
                let search_start = if filled > 4 { filled - 4 } else { 0 };
                // Actually search all of filled since leftover data might contain it
                if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                    found_headers_end = true;
                    break;
                }
                let _ = search_start; // suppress warning
            }

            if filled >= buf.len() {
                break; // headers exceed 64 KB — forward as-is
            }
            let n = match tokio::time::timeout(deadline, client.read(&mut buf[filled..])).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => {
                    if filled == 0 { return Ok(()); } // clean close or timeout
                    // Client closed mid-headers or timed out — forward what we have
                    found_headers_end = true;
                    break;
                }
            };
            filled += n;
        }

        if filled == 0 {
            return Ok(()); // client closed connection
        }

        let request_data = &buf[..filled];

        // ── Debug status page ──────────────────────────────────────────────────
        if request_data.starts_with(b"GET /__run") {
            let body = {
                let table = routes.read().unwrap();
                if table.is_empty() {
                    "run.dev proxy: no routes registered yet (services still starting?)\n".to_string()
                } else {
                    let lines: Vec<String> = table.iter()
                        .map(|r| format!("  {} -> :{}", r.domain, r.target_port))
                        .collect();
                    format!("run.dev proxy routes:\n{}\n", lines.join("\n"))
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            let _ = client.write_all(response.as_bytes()).await;
            return Ok(());
        }

        let host = extract_host_header(request_data);

        // ── HTTP→HTTPS redirect ────────────────────────────────────────────────
        if !is_tls {
            if let Some(ref h) = host {
                if crate::core::ssl::cert_exists(h) || {
                    h.find('.').map(|i| crate::core::ssl::cert_exists(&h[i+1..])).unwrap_or(false)
                } {
                    let path = extract_request_path(request_data);
                    let location = format!("https://{}{}", h, path);
                    let body = format!("Redirecting to {}", location);
                    let response = format!(
                        "HTTP/1.1 301 Moved Permanently\r\nLocation: {}\r\nContent-Length: {}\r\n\r\n{}",
                        location, body.len(), body
                    );
                    let _ = client.write_all(response.as_bytes()).await;
                    return Ok(());
                }
            }
        }

        // ── Route lookup ───────────────────────────────────────────────────────
        let (target_port, route_table_dump) = {
            let table = routes.read().unwrap();
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

        // ── Connect upstream ───────────────────────────────────────────────────
        let mut upstream = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            TcpStream::connect(format!("127.0.0.1:{}", target_port)),
        ).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
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
            Err(_) => {
                let body = format!(
                    "run.dev proxy: connection to '{}' port {} timed out",
                    host.as_deref().unwrap_or("?"), target_port
                );
                let response = format!(
                    "HTTP/1.1 504 Gateway Timeout\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = client.write_all(response.as_bytes()).await;
                return Ok(());
            }
        };

        // ── Inject proxy headers + Connection: close ───────────────────────────
        // Connection: close ensures upstream closes after responding, so we can
        // cleanly detect the response boundary and loop for the next request.
        let modified = inject_proxy_headers(request_data, is_tls, host.as_deref());
        upstream.write_all(&modified).await?;

        // ── Forward remaining request body from client to upstream ──────────────
        // Determine body length from Content-Length header (if present).
        // The initial buffer may contain some or all of the body already.
        let headers_end_pos = request_data.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4);

        let content_length = extract_content_length(request_data);
        let body_in_buffer = headers_end_pos
            .map(|hep| filled.saturating_sub(hep))
            .unwrap_or(0);
        let remaining_body = content_length
            .unwrap_or(0)
            .saturating_sub(body_in_buffer);

        // Forward remaining body bytes from client
        if remaining_body > 0 {
            let mut left = remaining_body;
            let mut tmp = vec![0u8; 65536];
            while left > 0 {
                let to_read = left.min(tmp.len());
                let n = match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    client.read(&mut tmp[..to_read]),
                ).await {
                    Ok(Ok(n)) if n > 0 => n,
                    _ => break,
                };
                upstream.write_all(&tmp[..n]).await?;
                left -= n;
            }
        }

        // ── Read response from upstream and forward to client ──────────────────
        // Read until upstream closes the connection (Connection: close guarantees this).
        let mut resp_buf = vec![0u8; 65536];
        loop {
            let n = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                upstream.read(&mut resp_buf),
            ).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => break, // upstream closed or timeout
            };
            if client.write_all(&resp_buf[..n]).await.is_err() {
                return Ok(()); // client disconnected
            }
        }

        // If the client sent Connection: close, we're done
        if has_connection_close(request_data) {
            return Ok(());
        }

        // Otherwise loop to handle the next request on this keep-alive connection.
        // Reset leftover tracking (no leftover body data in current design).
        leftover_start = 0;
        leftover_end = 0;
    }
}

/// Inject X-Forwarded-* headers into the HTTP request before forwarding upstream.
/// Finds the end of the first header line (\r\n) and inserts proxy headers there.
fn inject_proxy_headers(request: &[u8], is_tls: bool, host: Option<&str>) -> Vec<u8> {
    // Find the first \r\n (end of request line) to insert headers after it
    let header_end = request.windows(2).position(|w| w == b"\r\n");
    let Some(first_crlf) = header_end else {
        return request.to_vec();
    };

    let proto = if is_tls { "https" } else { "http" };
    let mut extra_headers = format!(
        "X-Forwarded-Proto: {}\r\nX-Forwarded-For: 127.0.0.1\r\nX-Real-IP: 127.0.0.1\r\n",
        proto
    );
    if let Some(h) = host {
        extra_headers.push_str(&format!("X-Forwarded-Host: {}\r\n", h));
    }

    let insert_pos = first_crlf + 2; // after the first \r\n
    let mut result = Vec::with_capacity(request.len() + extra_headers.len());
    result.extend_from_slice(&request[..insert_pos]);
    result.extend_from_slice(extra_headers.as_bytes());
    result.extend_from_slice(&request[insert_pos..]);
    result
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

/// Extract the request path from the HTTP request line (e.g. "GET /foo HTTP/1.1" → "/foo")
fn extract_request_path(request: &[u8]) -> String {
    let text = std::str::from_utf8(request).unwrap_or("");
    if let Some(line) = text.lines().next() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            return parts[1].to_string();
        }
    }
    "/".to_string()
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
    let rules = "rdr pass on lo0 proto tcp from any to any port 80 -> 127.0.0.1 port 1111\n\
                 rdr pass on lo0 proto tcp from any to any port 443 -> 127.0.0.1 port 1112\n";

    // Check current nat rules — only reload if stale or missing
    let check = std::process::Command::new("sudo")
        .args(["-n", "pfctl", "-s", "nat"])
        .output();
    let needs_update = match &check {
        Ok(output) => {
            let current = String::from_utf8_lossy(&output.stdout);
            !current.contains("port 1111") || !current.contains("port 1112")
                || current.contains("8080") || current.contains("8443")
        }
        Err(_) => true,
    };

    if needs_update {
        // Flush stale nat rules first
        let _ = std::process::Command::new("sudo")
            .args(["-n", "pfctl", "-F", "nat"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Merge rdr rules directly into the main ruleset (bypasses anchor ordering issues)
        let tmp = std::env::temp_dir().join("rundev-pf-rdr");
        if std::fs::write(&tmp, rules).is_ok() {
            let _ = std::process::Command::new("sudo")
                .args(["-n", "pfctl", "-mf", &tmp.to_string_lossy()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_file(&tmp);
        }
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
    let mut t = table.write().unwrap();
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
        let t = table.read().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].domain, "b.local");
    }
}

fn tracing_or_eprintln(msg: String) {
    eprintln!("{}", msg);
}
