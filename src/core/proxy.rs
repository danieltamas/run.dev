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
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

pub struct ProxyRoute {
    pub domain: String,
    pub target_port: u16,
    /// Bytes received from the client (request bodies, headers).
    pub bytes_in: Arc<AtomicU64>,
    /// Bytes sent to the client (response bodies, headers).
    pub bytes_out: Arc<AtomicU64>,
}

impl Clone for ProxyRoute {
    fn clone(&self) -> Self {
        Self {
            domain: self.domain.clone(),
            target_port: self.target_port,
            bytes_in: self.bytes_in.clone(),
            bytes_out: self.bytes_out.clone(),
        }
    }
}

impl std::fmt::Debug for ProxyRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyRoute")
            .field("domain", &self.domain)
            .field("target_port", &self.target_port)
            .field("bytes_in", &self.bytes_in.load(Ordering::Relaxed))
            .field("bytes_out", &self.bytes_out.load(Ordering::Relaxed))
            .finish()
    }
}

impl ProxyRoute {
    pub fn new(domain: String, target_port: u16) -> Self {
        Self {
            domain,
            target_port,
            bytes_in: Arc::new(AtomicU64::new(0)),
            bytes_out: Arc::new(AtomicU64::new(0)),
        }
    }
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
    // Read until we have the complete HTTP headers (\r\n\r\n).
    // A single read() may not capture everything — large headers like DPoP JWTs
    // can span multiple TCP segments, and injecting proxy headers into a partial
    // header block corrupts whatever header got split across the boundary.
    let mut buf = vec![0u8; 65536];
    let mut filled = 0usize;
    let deadline = std::time::Duration::from_secs(10);

    loop {
        if filled >= 4 && buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if filled >= buf.len() {
            break;
        }
        let n = match tokio::time::timeout(deadline, client.read(&mut buf[filled..])).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => {
                if filled == 0 { return Ok(()); }
                break;
            }
        };
        filled += n;
    }

    if filled == 0 {
        return Ok(());
    }

    let request = &buf[..filled];

    // Debug status page — curl http://localhost:1111/__run
    if request.starts_with(b"GET /__run") {
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
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        let _ = client.write_all(response.as_bytes()).await;
        return Ok(());
    }

    let host = extract_host_header(request);
    let req_path = extract_request_path(request);
    let req_line = std::str::from_utf8(request)
        .ok()
        .and_then(|t| t.lines().next())
        .unwrap_or("?")
        .to_string();
    proxy_log(&format!("{} host={} tls={}", req_line, host.as_deref().unwrap_or("?"), is_tls));

    // HTTP→HTTPS redirect: if this is a plain HTTP request and the domain has a cert,
    // redirect to HTTPS so Origin headers match what CORS allow-lists expect.
    if !is_tls {
        if let Some(ref h) = host {
            if crate::core::ssl::cert_exists(h) || {
                h.find('.').map(|i| crate::core::ssl::cert_exists(&h[i+1..])).unwrap_or(false)
            } {
                let path = extract_request_path(request);
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

    // Find matching route
    let (route_match, route_table_dump) = {
        let table = routes.read().unwrap();
        let matched = find_route(&table, host.as_deref());
        let dump: Vec<String> = table.iter()
            .map(|r| format!("{}:{}", r.domain, r.target_port))
            .collect();
        (matched, dump)
    };

    let (target_port, counter_in, counter_out) = match route_match {
        Some((p, ci, co)) => (p, ci, co),
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

    proxy_log(&format!("→ {}:{} to :{}", host.as_deref().unwrap_or("?"), req_path, target_port));

    // Connect to target service
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

    let is_websocket = is_upgrade_request(request);

    // Count the initial request bytes
    counter_in.fetch_add(filled as u64, Ordering::Relaxed);

    if is_websocket {
        // ── WebSocket path ──────────────────────────────────────────────────
        let request = inject_websocket_headers(request, is_tls, host.as_deref());
        upstream.write_all(&request).await?;

        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut upstream_read, mut upstream_write) = upstream.split();

        let ci = counter_in.clone();
        let co = counter_out.clone();
        let _ = tokio::try_join!(
            async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match client_read.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            ci.fetch_add(n as u64, Ordering::Relaxed);
                            if upstream_write.write_all(&buf[..n]).await.is_err() { break; }
                        }
                        Err(_) => break,
                    }
                }
                Ok::<_, std::io::Error>(())
            },
            async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match upstream_read.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            co.fetch_add(n as u64, Ordering::Relaxed);
                            if client_write.write_all(&buf[..n]).await.is_err() { break; }
                        }
                        Err(_) => break,
                    }
                }
                Ok::<_, std::io::Error>(())
            },
        );
    } else {
        // ── Normal HTTP path ────────────────────────────────────────────────
        let request = inject_proxy_headers(request, is_tls, host.as_deref());
        upstream.write_all(&request).await?;

        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut upstream_read, mut upstream_write) = upstream.split();

        let ci = counter_in.clone();
        let co = counter_out.clone();

        // Task: forward remaining request body (for POST/PUT with bodies > 64KB)
        let fwd = async {
            let mut buf = vec![0u8; 65536];
            loop {
                match client_read.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        ci.fetch_add(n as u64, Ordering::Relaxed);
                        if upstream_write.write_all(&buf[..n]).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            Ok::<_, std::io::Error>(0u64)
        };

        // Task: read response headers, inject Connection: close, pipe the rest
        let resp = async {
            let mut buf = vec![0u8; 65536];
            let mut filled = 0usize;

            // Buffer until we have the full response headers
            loop {
                if filled >= 4 && buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if filled >= buf.len() { break; }
                match upstream_read.read(&mut buf[filled..]).await {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => break,
                }
            }
            if filled > 0 {
                let resp_line = std::str::from_utf8(&buf[..filled.min(80)])
                    .ok()
                    .and_then(|t| t.lines().next())
                    .unwrap_or("?");
                proxy_log(&format!("← {} ({} bytes) for {}{}", resp_line, filled, host.as_deref().unwrap_or("?"), req_path));

                let modified = inject_response_connection_close(&buf[..filled]);
                co.fetch_add(modified.len() as u64, Ordering::Relaxed);
                let _ = client_write.write_all(&modified).await;
                let _ = client_write.flush().await;

                let mut pipe_buf = vec![0u8; 65536];
                loop {
                    match upstream_read.read(&mut pipe_buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            co.fetch_add(n as u64, Ordering::Relaxed);
                            if client_write.write_all(&pipe_buf[..n]).await.is_err() {
                                break;
                            }
                            let _ = client_write.flush().await;
                        }
                        Err(_) => break,
                    }
                }
            }
            Ok::<_, std::io::Error>(0u64)
        };

        let _ = tokio::try_join!(fwd, resp);
    }

    Ok(())
}

/// Detect WebSocket upgrade requests (Connection: Upgrade + Upgrade: websocket).
fn is_upgrade_request(request: &[u8]) -> bool {
    let text = match std::str::from_utf8(request) {
        Ok(t) => t,
        Err(_) => {
            // Headers might be followed by binary body — only check header portion
            let end = request.windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(request.len());
            match std::str::from_utf8(&request[..end]) {
                Ok(t) => t,
                Err(_) => return false,
            }
        }
    };
    let lower = text.to_ascii_lowercase();
    lower.contains("upgrade: websocket")
}

/// Inject only X-Forwarded-* headers for WebSocket requests.
/// Preserves Connection: Upgrade and Upgrade: websocket intact.
fn inject_websocket_headers(request: &[u8], is_tls: bool, host: Option<&str>) -> Vec<u8> {
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

    let insert_pos = first_crlf + 2;
    let mut result = Vec::with_capacity(request.len() + extra_headers.len());
    result.extend_from_slice(&request[..insert_pos]);
    result.extend_from_slice(extra_headers.as_bytes());
    result.extend_from_slice(&request[insert_pos..]);
    result
}

/// Inject Connection: close into an HTTP response so the browser opens a new
/// connection for the next request (ensuring proper proxy header injection).
fn inject_response_connection_close(response: &[u8]) -> Vec<u8> {
    // Strip existing Connection header, then add Connection: close
    let cleaned = strip_header(response, b"connection:");
    let first_crlf = cleaned.windows(2).position(|w| w == b"\r\n");
    let Some(pos) = first_crlf else {
        return response.to_vec();
    };
    let insert_pos = pos + 2;
    let mut result = Vec::with_capacity(cleaned.len() + 20);
    result.extend_from_slice(&cleaned[..insert_pos]);
    result.extend_from_slice(b"Connection: close\r\n");
    result.extend_from_slice(&cleaned[insert_pos..]);
    result
}

/// Inject X-Forwarded-* headers and force Connection: close for upstream.
/// Connection: close ensures the upstream closes after responding, letting the
/// proxy detect the response boundary and loop back for the next keep-alive request.
fn inject_proxy_headers(request: &[u8], is_tls: bool, host: Option<&str>) -> Vec<u8> {
    // Find the first \r\n (end of request line) to insert headers after it
    let header_end = request.windows(2).position(|w| w == b"\r\n");
    let Some(first_crlf) = header_end else {
        return request.to_vec();
    };

    let proto = if is_tls { "https" } else { "http" };
    let mut extra_headers = format!(
        "Connection: close\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: 127.0.0.1\r\nX-Real-IP: 127.0.0.1\r\n",
        proto
    );
    if let Some(h) = host {
        extra_headers.push_str(&format!("X-Forwarded-Host: {}\r\n", h));
    }

    // Strip existing Connection header to avoid conflicts with our Connection: close
    let cleaned = strip_header(request, b"connection:");

    let insert_pos = first_crlf + 2; // after the first \r\n
    // Adjust insert position if we stripped a header before this point
    let actual_insert = cleaned.windows(2).position(|w| w == b"\r\n")
        .map(|p| p + 2)
        .unwrap_or(insert_pos);

    let mut result = Vec::with_capacity(cleaned.len() + extra_headers.len());
    result.extend_from_slice(&cleaned[..actual_insert]);
    result.extend_from_slice(extra_headers.as_bytes());
    result.extend_from_slice(&cleaned[actual_insert..]);
    result
}

/// Remove a header line (case-insensitive) from raw HTTP request bytes.
/// Only modifies the header section; body bytes after \r\n\r\n are preserved verbatim.
fn strip_header(request: &[u8], header_lower: &[u8]) -> Vec<u8> {
    // Find end of headers to avoid touching body (which may be binary)
    let headers_end = request.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4) // include the \r\n\r\n
        .unwrap_or(request.len());

    let header_bytes = &request[..headers_end];
    let body_bytes = &request[headers_end..];

    let header_text = match std::str::from_utf8(header_bytes) {
        Ok(t) => t,
        Err(_) => return request.to_vec(),
    };

    let mut result = Vec::with_capacity(request.len());
    for line in header_text.split("\r\n") {
        if line.to_ascii_lowercase().as_bytes().starts_with(header_lower) {
            continue; // skip this header
        }
        if !result.is_empty() {
            result.extend_from_slice(b"\r\n");
        }
        result.extend_from_slice(line.as_bytes());
    }
    result.extend_from_slice(body_bytes);
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

/// Find a matching route and return (port, bytes_in counter, bytes_out counter).
fn find_route(routes: &[ProxyRoute], host: Option<&str>) -> Option<(u16, Arc<AtomicU64>, Arc<AtomicU64>)> {
    let host = host?;
    for route in routes {
        if route.domain.eq_ignore_ascii_case(host) {
            return Some((route.target_port, route.bytes_in.clone(), route.bytes_out.clone()));
        }
    }
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

    // Always flush and reload — pfctl rules can silently go stale after sleep/reboot
    // and text-based checks on `pfctl -s nat` are unreliable. This is cheap (<10ms).
    let _ = std::process::Command::new("sudo")
        .args(["-n", "pfctl", "-F", "nat"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let tmp = std::env::temp_dir().join("rundev-pf-rdr");
    if std::fs::write(&tmp, rules).is_ok() {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "pfctl", "-mf", &tmp.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::remove_file(&tmp);
    }

    // Enable pf if not already active
    let _ = std::process::Command::new("sudo")
        .args(["-n", "pfctl", "-e"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Verify: try connecting to port 443 → should reach 1112
    // If it fails, log but don't crash — user can run `rundev setup`
    let verify = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 443)),
        std::time::Duration::from_millis(500),
    );
    if verify.is_err() {
        eprintln!("⚠️  port forwarding may not be active — try `rundev setup`");
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
    // Preserve byte counters for domains that already exist
    for new in &new_routes {
        if let Some(existing) = t.iter().find(|r| r.domain == new.domain) {
            new.bytes_in.store(existing.bytes_in.load(Ordering::Relaxed), Ordering::Relaxed);
            new.bytes_out.store(existing.bytes_out.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }
    *t = new_routes;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(domain: &str, port: u16) -> ProxyRoute {
        ProxyRoute::new(domain.to_string(), port)
    }

    /// Helper: extract just the port from find_route for easier assertions.
    fn find_port(routes: &[ProxyRoute], host: Option<&str>) -> Option<u16> {
        find_route(routes, host).map(|(p, _, _)| p)
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
        assert_eq!(find_port(&routes, Some("api.myapp.local")), Some(3001));
    }

    #[test]
    fn find_route_first_match_wins() {
        let routes = vec![route("app.local", 4000), route("app.local", 9999)];
        assert_eq!(find_port(&routes, Some("app.local")), Some(4000));
    }

    #[test]
    fn find_route_no_match_returns_none() {
        let routes = vec![route("other.local", 3000)];
        assert_eq!(find_port(&routes, Some("app.local")), None);
    }

    #[test]
    fn find_route_none_host_returns_none() {
        let routes = vec![route("app.local", 3000)];
        assert_eq!(find_port(&routes, None), None);
    }

    #[test]
    fn find_route_case_insensitive() {
        let routes = vec![route("MyApp.local", 5000)];
        assert_eq!(find_port(&routes, Some("myapp.local")), Some(5000));
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

    // ── inject_proxy_headers ────────────────────────────────────────────────

    #[test]
    fn inject_headers_adds_forwarded_proto_https() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n";
        let result = inject_proxy_headers(req, true, Some("app.local"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("X-Forwarded-Proto: https\r\n"));
        assert!(text.contains("X-Forwarded-Host: app.local\r\n"));
        assert!(text.contains("X-Forwarded-For: 127.0.0.1\r\n"));
        assert!(text.contains("X-Real-IP: 127.0.0.1\r\n"));
    }

    #[test]
    fn inject_headers_adds_forwarded_proto_http() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n";
        let result = inject_proxy_headers(req, false, Some("app.local"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("X-Forwarded-Proto: http\r\n"));
    }

    #[test]
    fn inject_headers_adds_connection_close() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n";
        let result = inject_proxy_headers(req, true, None);
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Connection: close\r\n"));
    }

    #[test]
    fn inject_headers_replaces_keepalive_with_close() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\nConnection: keep-alive\r\n\r\n";
        let result = inject_proxy_headers(req, true, Some("app.local"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Connection: close\r\n"));
        assert!(!text.contains("keep-alive"));
    }

    #[test]
    fn inject_headers_preserves_request_line() {
        let req = b"POST /api/v2/wallet/verify HTTP/1.1\r\nHost: win.wam.app\r\n\r\n";
        let result = inject_proxy_headers(req, true, Some("win.wam.app"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.starts_with("POST /api/v2/wallet/verify HTTP/1.1\r\n"));
    }

    #[test]
    fn inject_headers_preserves_existing_headers() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\nDPoP: eyJhbGciOi\r\nCookie: session=abc\r\n\r\n";
        let result = inject_proxy_headers(req, true, Some("app.local"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("DPoP: eyJhbGciOi\r\n"));
        assert!(text.contains("Cookie: session=abc\r\n"));
    }

    #[test]
    fn inject_headers_preserves_body() {
        let body = b"{\"key\":\"value\"}";
        let mut req = b"POST / HTTP/1.1\r\nHost: app.local\r\nContent-Length: 15\r\n\r\n".to_vec();
        req.extend_from_slice(body);
        let result = inject_proxy_headers(&req, true, Some("app.local"));
        assert!(result.ends_with(body));
    }

    #[test]
    fn inject_headers_preserves_header_terminator() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n";
        let result = inject_proxy_headers(req, true, None);
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("\r\n\r\n"), "must preserve \\r\\n\\r\\n terminator");
    }

    #[test]
    fn inject_headers_no_host_skips_forwarded_host() {
        let req = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        let result = inject_proxy_headers(req, true, None);
        let text = String::from_utf8(result).unwrap();
        assert!(!text.contains("X-Forwarded-Host"));
        assert!(text.contains("X-Forwarded-Proto: https\r\n"));
    }

    // ── strip_header ────────────────────────────────────────────────────────

    #[test]
    fn strip_header_removes_connection() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n";
        let result = strip_header(req, b"connection:");
        let text = String::from_utf8(result).unwrap();
        assert!(!text.contains("Connection"));
        assert!(!text.contains("keep-alive"));
        assert!(text.contains("Host: x\r\n"));
        assert!(text.contains("Accept: */*\r\n"));
    }

    #[test]
    fn strip_header_case_insensitive() {
        let req = b"GET / HTTP/1.1\r\nCONNECTION: keep-alive\r\n\r\n";
        let result = strip_header(req, b"connection:");
        let text = String::from_utf8(result).unwrap();
        assert!(!text.to_lowercase().contains("connection"));
    }

    #[test]
    fn strip_header_preserves_body_bytes() {
        let mut req = b"POST / HTTP/1.1\r\nConnection: close\r\n\r\n".to_vec();
        let body = vec![0u8, 1, 2, 255, 254]; // binary body
        req.extend_from_slice(&body);
        let result = strip_header(&req, b"connection:");
        assert!(result.ends_with(&body));
    }

    #[test]
    fn strip_header_no_match_returns_unchanged() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let result = strip_header(req, b"connection:");
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Host: x"));
        assert!(text.contains("\r\n\r\n"));
    }

    #[test]
    fn strip_header_preserves_double_crlf_terminator() {
        let req = b"GET / HTTP/1.1\r\nConnection: keep-alive\r\nHost: x\r\n\r\n";
        let result = strip_header(req, b"connection:");
        let text = String::from_utf8(result).unwrap();
        assert!(text.ends_with("\r\n\r\n"), "must end with \\r\\n\\r\\n, got: {:?}", text);
    }

    // ── is_upgrade_request ──────────────────────────────────────────────────

    #[test]
    fn is_upgrade_detects_websocket() {
        let req = b"GET /_nuxt/?token=abc HTTP/1.1\r\nHost: win.wam.app\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: x\r\n\r\n";
        assert!(is_upgrade_request(req));
    }

    #[test]
    fn is_upgrade_case_insensitive() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\nupgrade: WebSocket\r\nconnection: upgrade\r\n\r\n";
        assert!(is_upgrade_request(req));
    }

    #[test]
    fn is_upgrade_false_for_normal_request() {
        let req = b"GET / HTTP/1.1\r\nHost: app.local\r\nConnection: keep-alive\r\n\r\n";
        assert!(!is_upgrade_request(req));
    }

    // ── inject_websocket_headers ──────────────────────────────────────────

    #[test]
    fn websocket_headers_preserve_connection_upgrade() {
        let req = b"GET /_nuxt/ HTTP/1.1\r\nHost: win.wam.app\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        let result = inject_websocket_headers(req, true, Some("win.wam.app"));
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Connection: Upgrade\r\n"));
        assert!(text.contains("Upgrade: websocket\r\n"));
        assert!(text.contains("X-Forwarded-Proto: https\r\n"));
        assert!(!text.contains("Connection: close"));
    }

    // ── inject_response_connection_close ──────────────────────────────────

    #[test]
    fn response_close_replaces_keepalive() {
        let resp = b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Type: text/html\r\n\r\n<html>";
        let result = inject_response_connection_close(resp);
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Connection: close\r\n"));
        assert!(!text.contains("keep-alive"));
        assert!(text.ends_with("<html>"));
    }

    #[test]
    fn response_close_adds_when_missing() {
        let resp = b"HTTP/1.1 302 Found\r\nLocation: /dashboard\r\n\r\n";
        let result = inject_response_connection_close(resp);
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Connection: close\r\n"));
    }

    // ── extract_request_path ────────────────────────────────────────────────

    #[test]
    fn extract_path_simple() {
        let req = b"GET /api/v2/session HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(extract_request_path(req), "/api/v2/session");
    }

    #[test]
    fn extract_path_with_query() {
        let req = b"GET /search?q=test HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(extract_request_path(req), "/search?q=test");
    }

    #[test]
    fn extract_path_root() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(extract_request_path(req), "/");
    }

    #[test]
    fn extract_path_malformed_returns_slash() {
        let req = b"BADREQUEST\r\n\r\n";
        assert_eq!(extract_request_path(req), "/");
    }
}

fn proxy_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open("/tmp/rundev-proxy.log")
    {
        let _ = writeln!(f, "[proxy] {}", msg);
    }
}

fn tracing_or_eprintln(msg: String) {
    eprintln!("{}", msg);
}
