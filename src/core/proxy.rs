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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;

/// Cap on concurrent proxied connections. Anything over this gets accepted
/// and immediately dropped — a backstop against runaway concurrency. Each
/// connection holds two FDs (client + upstream), so this stays comfortably under
/// the raised FD limit (`raise_fd_limit()` in main lifts it to 65536).
const MAX_CONCURRENT_CONNS: usize = 2048;

/// Idle-read timeout. If neither side sends a byte for this long, the
/// connection is torn down. Catches zombie sockets from Wi-Fi sleep,
/// force-quits, or clients that vanish without sending FIN/RST.
const IDLE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Live count of in-flight proxied connections, for the `/__run` diagnostic.
/// Incremented when a connection task is spawned, decremented when it ends — so
/// a steadily-climbing value that never falls is the signature of an FD leak.
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

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
    proxy_log(&format!("Proxy listening on 127.0.0.1:{}", listen_port));
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let routes = routes.clone();
                let permit = match sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        proxy_log("HTTP proxy: connection cap reached, dropping connection");
                        drop(stream);
                        continue;
                    }
                };
                tokio::spawn(async move {
                    ACTIVE_CONNS.fetch_add(1, Ordering::Relaxed);
                    let _ = handle_connection(stream, routes, false).await;
                    ACTIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
                    drop(permit);
                });
            }
            Err(e) => {
                proxy_log(&format!("Proxy accept error: {}", e));
                // EMFILE/ENFILE: yield and back off so we don't spin and spam logs
                if matches!(e.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

pub async fn run_https_proxy(routes: RouteTable, listen_port: u16) -> Result<()> {
    let cert_dir = crate::core::ssl::certs_dir();
    let resolver = SniCertResolver::new(&cert_dir);
    if resolver.is_empty() {
        proxy_log(&format!("HTTPS proxy: no certs found in {}, skipping TLS listener", cert_dir.display()));
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
    proxy_log(&format!("HTTPS proxy listening on 127.0.0.1:{}", listen_port));
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let acceptor = acceptor.clone();
                let routes = routes.clone();
                let permit = match sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        proxy_log("HTTPS proxy: connection cap reached, dropping connection");
                        drop(stream);
                        continue;
                    }
                };
                tokio::spawn(async move {
                    ACTIVE_CONNS.fetch_add(1, Ordering::Relaxed);
                    // Timeout the TLS handshake so bad clients don't hold connections forever
                    let tls_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        acceptor.accept(stream),
                    ).await;
                    match tls_result {
                        Ok(Ok(tls_stream)) => { let _ = handle_connection(tls_stream, routes, true).await; }
                        _ => {} // TLS handshake failed or timed out — ignore
                    }
                    ACTIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
                    drop(permit);
                });
            }
            Err(e) => {
                proxy_log(&format!("HTTPS proxy accept error: {}", e));
                // EMFILE/ENFILE: yield and back off so we don't spin and spam logs
                if matches!(e.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
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
            let routes_str = if table.is_empty() {
                "run.dev proxy: no routes registered yet (services still starting?)".to_string()
            } else {
                let lines: Vec<String> = table.iter()
                    .map(|r| format!("  {} -> :{}", r.domain, r.target_port))
                    .collect();
                format!("run.dev proxy routes:\n{}", lines.join("\n"))
            };
            // Diagnostics: a steadily-climbing "active" that never drops means an
            // FD/connection leak; "active" near the cap or near the fd limit
            // means you're about to get EMFILE.
            format!(
                "{}\n\nactive connections: {} / {}\nfd soft limit: {}\n",
                routes_str,
                ACTIVE_CONNS.load(Ordering::Relaxed),
                MAX_CONCURRENT_CONNS,
                current_fd_soft_limit(),
            )
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
        // When EITHER direction ends (peer close, write error, or idle timeout),
        // tear the whole connection down so the other half's FDs are released
        // immediately. try_join! used to wait for BOTH directions — so when a
        // browser closed an HMR WebSocket on refresh, the upstream half lingered
        // (Vite keeps pinging) and leaked a client+upstream FD pair per refresh
        // until the proxy hit EMFILE and every domain went dark.
        tokio::select! {
            _ = async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match tokio::time::timeout(IDLE_READ_TIMEOUT, client_read.read(&mut buf)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            ci.fetch_add(n as u64, Ordering::Relaxed);
                            if upstream_write.write_all(&buf[..n]).await.is_err() { break; }
                        }
                        _ => break, // error or idle timeout
                    }
                }
            } => {}
            _ = async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match tokio::time::timeout(IDLE_READ_TIMEOUT, upstream_read.read(&mut buf)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            co.fetch_add(n as u64, Ordering::Relaxed);
                            if client_write.write_all(&buf[..n]).await.is_err() { break; }
                        }
                        _ => break, // error or idle timeout
                    }
                }
            } => {}
        }
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
                match tokio::time::timeout(IDLE_READ_TIMEOUT, client_read.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        ci.fetch_add(n as u64, Ordering::Relaxed);
                        if upstream_write.write_all(&buf[..n]).await.is_err() { break; }
                    }
                    _ => break, // error or idle timeout
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
                match tokio::time::timeout(IDLE_READ_TIMEOUT, upstream_read.read(&mut buf[filled..])).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => filled += n,
                    _ => break, // error or idle timeout — a silent upstream must not
                                // pin this task's permit + FDs forever (FD exhaustion)
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
                    match tokio::time::timeout(IDLE_READ_TIMEOUT, upstream_read.read(&mut pipe_buf)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            co.fetch_add(n as u64, Ordering::Relaxed);
                            if client_write.write_all(&pipe_buf[..n]).await.is_err() {
                                break;
                            }
                            let _ = client_write.flush().await;
                        }
                        _ => break, // error or idle timeout
                    }
                }
            }
            Ok::<_, std::io::Error>(0u64)
        };

        // One response per socket (we force Connection: close both ways), so the
        // transaction is over the instant `resp` finishes relaying the response.
        // Don't keep waiting on `fwd`: after a Connection: close response the
        // client can sit on the socket for up to IDLE_READ_TIMEOUT before
        // closing, which would pin this task's client+upstream FD pair that whole
        // time and leak descriptors under load (→ EMFILE → dead domains). Drive
        // on `resp`; `fwd` runs concurrently only to stream an oversized request
        // body, and is dropped (closing both sockets) once the response is out.
        tokio::pin!(fwd);
        tokio::pin!(resp);
        let mut fwd_done = false;
        loop {
            tokio::select! {
                _ = &mut resp => break,
                _ = &mut fwd, if !fwd_done => { fwd_done = true; }
            }
        }
    }

    Ok(())
}

/// The process's current soft open-file limit (RLIMIT_NOFILE), for the `/__run`
/// diagnostic.
fn current_fd_soft_limit() -> u64 {
    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            lim.rlim_cur as u64
        } else {
            0
        }
    }
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

/// Port we probe to decide whether the 80/443 → 1111/1112 redirect is live.
///
/// We probe **:80** rather than :443 because the HTTP listener on 1111 always
/// exists, whereas the HTTPS listener on 1112 is skipped when no certs are
/// present — probing :443 would then false-negative. pf/iptables installs the
/// :80 and :443 redirect rules atomically, so :80 reachability stands in for the
/// whole ruleset. A loopback connect to :80 follows the exact same rdr/REDIRECT
/// path the browser does, so it faithfully reports what the user will experience.
const REDIRECT_PROBE_PORT: u16 = 80;

/// True if the port-forward redirect is currently routing loopback traffic to our
/// listeners. Cheap — one short-lived TCP connect with a tight timeout — so it's
/// safe to call on a timer. When pf/iptables rules are flushed (sleep/wake on
/// macOS, or Docker/firewalld reloading their tables on Linux) nothing answers on
/// :80 and this returns `false`, which is the signal to re-apply the redirect.
pub async fn redirect_reachable() -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(400),
            TcpStream::connect(("127.0.0.1", REDIRECT_PROBE_PORT)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// One iteration of the self-heal loop: probe the redirect and, only if it's
/// down, re-apply port forwarding. Returns the observed reachability so the
/// caller can surface it in the UI.
///
/// Generic over the probe/apply steps so the decision logic ("re-apply iff the
/// redirect is down") is unit-testable without touching the network or pfctl —
/// the production caller passes [`redirect_reachable`] and
/// [`activate_port_forwarding`].
pub async fn self_heal_once<P, PF, A, AF>(probe: P, apply: A) -> bool
where
    P: FnOnce() -> PF,
    PF: std::future::Future<Output = bool>,
    A: FnOnce() -> AF,
    AF: std::future::Future<Output = ()>,
{
    let reachable = probe().await;
    if !reachable {
        apply().await;
    }
    reachable
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
        // Route to the proxy log, not stderr: this runs under the TUI's alternate
        // screen (both at startup and from the periodic self-heal task), and stray
        // stderr corrupts ratatui's cell model and garbles the dashboard.
        proxy_log("⚠️  port forwarding may not be active — try `rundev setup`");
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

    // ── self_heal_once (redirect re-assertion decision) ───────────────────────
    // Regression for "proxy shows running but domains stop resolving until
    // restart": the pf/iptables redirect was applied once at startup and never
    // re-asserted, so a sleep/wake (macOS) or Docker/firewalld reload (Linux)
    // left the listeners up but unreachable. The self-heal loop must re-apply
    // forwarding when — and only when — the redirect is actually down.

    #[tokio::test]
    async fn self_heal_reapplies_when_redirect_is_down() {
        use std::sync::atomic::AtomicBool;
        let applied = Arc::new(AtomicBool::new(false));
        let a = applied.clone();
        let reachable = self_heal_once(
            || async { false },
            || async move { a.store(true, Ordering::Relaxed); },
        )
        .await;
        assert!(!reachable);
        assert!(
            applied.load(Ordering::Relaxed),
            "must re-apply port forwarding when the redirect is down"
        );
    }

    #[tokio::test]
    async fn self_heal_leaves_pf_alone_when_redirect_is_healthy() {
        use std::sync::atomic::AtomicBool;
        let applied = Arc::new(AtomicBool::new(false));
        let a = applied.clone();
        let reachable = self_heal_once(
            || async { true },
            || async move { a.store(true, Ordering::Relaxed); },
        )
        .await;
        assert!(reachable);
        assert!(
            !applied.load(Ordering::Relaxed),
            "must not touch pfctl/iptables while the redirect is working (no sudo spam, \
             no disturbing other nat rules)"
        );
    }

    // ── handle_connection idle timeout (permit/FD leak) ───────────────────────

    #[tokio::test(start_paused = true)]
    async fn handle_connection_aborts_on_silent_upstream() {
        // Regression: the upstream response-header read had no idle timeout, so an
        // upstream that accepts the TCP connection but never replies pinned this
        // connection's semaphore permit + FDs forever, eventually exhausting the
        // 512-permit budget (proxy "running" but every new connection dropped).
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _accepted = upstream.accept().await.unwrap();
            // Hold the socket open, never write a byte; outlive the idle timeout.
            tokio::time::sleep(std::time::Duration::from_secs(86_400)).await;
        });

        let table = new_route_table();
        update_routes(&table, vec![route("app.local", port)]).await;

        // In-memory client connection; feed the request through the test half.
        let (client_side, mut test_side) = tokio::io::duplex(65536);
        test_side
            .write_all(b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n")
            .await
            .unwrap();

        // With the idle timeout in place this resolves (in virtual time, thanks to
        // start_paused) rather than hanging on the upstream's missing headers.
        let res = tokio::time::timeout(
            IDLE_READ_TIMEOUT + std::time::Duration::from_secs(60),
            handle_connection(client_side, table, false),
        )
        .await;
        assert!(
            res.is_ok(),
            "handle_connection hung past the idle timeout on a silent upstream"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn handle_connection_releases_when_response_done_even_if_client_lingers() {
        // Regression: the HTTP path used try_join!(fwd, resp) and waited for BOTH
        // directions. After a Connection: close response, a client that holds its
        // socket open (no FIN) pinned this connection's client+upstream FDs until
        // IDLE_READ_TIMEOUT (300s) — an FD leak that, under refresh load, hit
        // EMFILE ("Too many open files") and made every domain go dark until a
        // restart. The fix drives on `resp`: the connection ends when the
        // response is fully relayed, regardless of the lingering client.

        // Upstream that replies in full, then closes.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await; // consume the proxied request
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
                .await;
            // Drop `sock` → upstream EOF → `resp` completes.
        });

        let table = new_route_table();
        update_routes(&table, vec![route("app.local", port)]).await;

        // Client that sends a request and then NEVER closes its socket.
        let (client_side, mut test_side) = tokio::io::duplex(65536);
        test_side
            .write_all(b"GET / HTTP/1.1\r\nHost: app.local\r\n\r\n")
            .await
            .unwrap();

        // Must return well before the 300s idle timeout — lingering would be the
        // FD leak. (start_paused makes the old, broken behavior fail fast too.)
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            handle_connection(client_side, table, false),
        )
        .await;
        assert!(
            res.is_ok(),
            "handle_connection lingered after the response was relayed (FD leak)"
        );
        drop(test_side); // keep the client socket open across the call above
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

const PROXY_LOG_PATH: &str = "/tmp/rundev-proxy.log";
/// Rotate the proxy log once it passes this size, keeping one previous file
/// (`<path>.1`). Caps disk use at ~2× this — the log used to grow without bound
/// (it had reached 600k+ lines).
const PROXY_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

struct ProxyLog {
    file: Option<std::fs::File>,
    written: u64,
}

fn proxy_log(msg: &str) {
    use std::io::Write;

    // One shared handle behind a mutex: no FD churn per line (the old per-call
    // open failed under FD pressure and interleaved concurrent writes), and a
    // running byte count so we can rotate.
    static STATE: std::sync::OnceLock<std::sync::Mutex<ProxyLog>> = std::sync::OnceLock::new();
    let state = STATE.get_or_init(|| std::sync::Mutex::new(ProxyLog { file: None, written: 0 }));

    // Logging must never panic or block the proxy.
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(), // recover from a poisoned lock
    };

    // Open once; reuse the handle. Seed `written` from the existing file size.
    if guard.file.is_none() {
        match std::fs::OpenOptions::new().create(true).append(true).open(PROXY_LOG_PATH) {
            Ok(f) => {
                guard.written = f.metadata().map(|m| m.len()).unwrap_or(0);
                guard.file = Some(f);
            }
            Err(_) => return, // can't open — drop the line
        }
    }

    // Rotate when too big: move the current log to `<path>.1`, start fresh.
    if guard.written >= PROXY_LOG_MAX_BYTES {
        guard.file = None; // close before renaming
        let _ = std::fs::rename(PROXY_LOG_PATH, format!("{PROXY_LOG_PATH}.1"));
        match std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(PROXY_LOG_PATH) {
            Ok(f) => {
                guard.written = 0;
                guard.file = Some(f);
            }
            Err(_) => {
                guard.written = 0;
                return;
            }
        }
    }

    // Disjoint field borrows (file vs. written) via a single &mut reborrow.
    let st = &mut *guard;
    if let Some(f) = st.file.as_mut() {
        if writeln!(f, "[proxy] {msg}").is_ok() {
            st.written += msg.len() as u64 + 9; // "[proxy] " (8) + "\n" (1)
        }
    }
}

