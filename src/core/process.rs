//! Process lifecycle management — spawn, stop, restart, and status tracking.
//!
//! Each managed service runs as a child process. stdout/stderr are captured into
//! a fixed-size ring buffer so the log panel always has recent output without
//! unbounded memory growth.
//!
//! Process state is persisted to `~/.config/rundev/state.json` so that `rundev down`
//! and other CLI commands can signal processes started by a previous session.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

#[cfg(unix)]
use std::os::unix::process::CommandExt as StdCommandExt;

use crate::core::config::state_path;

const MAX_LOG_LINES: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProcessStatus {
    Stopped,
    Starting,
    Running,
    Crashed {
        exit_code: i32,
        stderr_tail: String,
    },
    Restarting,
}

impl ProcessStatus {
    pub fn is_crashed(&self) -> bool {
        matches!(self, ProcessStatus::Crashed { .. })
    }

    pub fn label(&self) -> &str {
        match self {
            ProcessStatus::Stopped => "stopped",
            ProcessStatus::Starting => "starting",
            ProcessStatus::Running => "running",
            ProcessStatus::Crashed { .. } => "crashed",
            ProcessStatus::Restarting => "restarting",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManagedProcess {
    pub id: String,
    pub command: String,
    pub working_dir: PathBuf,
    pub port: u16,
    pub env: HashMap<String, String>,
    pub pid: Option<u32>,
    pub status: ProcessStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub crash_count: u32,
    pub last_stderr: VecDeque<String>,
    pub stdout_log: VecDeque<String>,
    /// All lines in chronological order (stdout + stderr interleaved). Used by the log panel.
    pub combined_log: VecDeque<String>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub proxied: bool,
    /// Bytes received through proxy (request data from clients).
    pub net_in: u64,
    /// Bytes sent through proxy (response data to clients).
    pub net_out: u64,
    /// Set by crash handler when EADDRINUSE is detected; app loop should respawn.
    pub needs_respawn: bool,
}

impl ManagedProcess {
    pub fn new(
        id: String,
        command: String,
        working_dir: PathBuf,
        port: u16,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            id,
            command,
            working_dir,
            port,
            env,
            pid: None,
            status: ProcessStatus::Stopped,
            started_at: None,
            crash_count: 0,
            last_stderr: VecDeque::with_capacity(MAX_LOG_LINES),
            stdout_log: VecDeque::with_capacity(MAX_LOG_LINES),
            combined_log: VecDeque::with_capacity(MAX_LOG_LINES),
            cpu_percent: 0.0,
            memory_bytes: 0,
            proxied: false,
            net_in: 0,
            net_out: 0,
            needs_respawn: false,
        }
    }

    pub fn push_stderr(&mut self, line: String) {
        // Auto-detect port from output when no port is configured yet.
        // Once port is found (non-zero), this stops naturally.
        if self.port == 0 {
            if let Some(p) = detect_port_in_line(&line) {
                self.port = p;
                // Port detected — service is now actually ready
                if self.status == ProcessStatus::Starting {
                    self.status = ProcessStatus::Running;
                }
            }
        }
        if self.last_stderr.len() >= MAX_LOG_LINES {
            self.last_stderr.pop_front();
        }
        self.last_stderr.push_back(line.clone());
        self.push_combined(format!("[err] {}", line));
    }

    pub fn push_stdout(&mut self, line: String) {
        if self.port == 0 {
            if let Some(p) = detect_port_in_line(&line) {
                self.port = p;
                if self.status == ProcessStatus::Starting {
                    self.status = ProcessStatus::Running;
                }
            }
        }
        if self.stdout_log.len() >= MAX_LOG_LINES {
            self.stdout_log.pop_front();
        }
        self.stdout_log.push_back(line.clone());
        self.push_combined(line);
    }

    fn push_combined(&mut self, line: String) {
        if self.combined_log.len() >= MAX_LOG_LINES {
            self.combined_log.pop_front();
        }
        self.combined_log.push_back(line);
    }

    pub fn stderr_tail(&self, n: usize) -> String {
        self.last_stderr
            .iter()
            .rev()
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub type SharedProcess = Arc<Mutex<ManagedProcess>>;

/// Kill any process currently listening on `port` using lsof.
/// Waits up to 500ms for it to exit, then SIGKILLs if needed.
#[cfg(unix)]
pub async fn kill_port(port: u16) {
    // Only kill processes LISTENING on this port — not every process with a
    // connection to it.  The old `lsof -ti :PORT` also matched rundev's own
    // proxy connections, which killed the entire TUI on restart.
    let output = tokio::process::Command::new("lsof")
        .args(["-ti", &format!("TCP:{}", port), "-sTCP:LISTEN"])
        .output()
        .await;

    let my_pid = std::process::id();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for pid_str in stdout.split_whitespace() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid == my_pid { continue; } // never kill ourselves
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
        }
        // Give processes a moment to exit before the caller spawns the replacement
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        // SIGKILL stragglers
        for pid_str in stdout.split_whitespace() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid == my_pid { continue; }
                if pid_exists(pid) {
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                }
            }
        }
    }
}

/// Kill all PIDs saved in state.json that are still alive (orphans from a
/// previous run.dev session). Clears the state file afterwards.
pub async fn kill_orphaned_pids() {
    let state = load_state();
    for (_, pid) in &state.pids {
        if pid_exists(*pid) {
            kill_pid(*pid).await;
        }
    }
    // Write a clean state
    save_state(&RunState::default());
}

pub async fn spawn_process(proc: SharedProcess) -> Result<()> {
    let (cmd, working_dir, env, id, port) = {
        let p = proc.lock().await;
        (
            p.command.clone(),
            p.working_dir.clone(),
            p.env.clone(),
            p.id.clone(),
            p.port,
        )
    };

    // If we know the port, evict any existing process holding it so we don't
    // get EADDRINUSE when the service tries to bind.
    #[cfg(unix)]
    if port > 0 {
        kill_port(port).await;
    }

    let tokens = shlex::split(&cmd)
        .ok_or_else(|| anyhow::anyhow!("Invalid command (unmatched quotes) for {}", id))?;
    let program = tokens
        .first()
        .ok_or_else(|| anyhow::anyhow!("Empty command for {}", id))?
        .clone();
    let args: Vec<String> = tokens.into_iter().skip(1).collect();

    {
        let mut p = proc.lock().await;
        p.status = ProcessStatus::Starting;
    }

    // Local dev services talk to themselves over HTTPS with mkcert certs.
    // Node.js doesn't trust the mkcert CA by default, causing server-to-server
    // requests (e.g. nuxt-auth session checks) to fail with "Failed to fetch".
    let mut full_env = env.clone();
    full_env.entry("NODE_TLS_REJECT_UNAUTHORIZED".to_string())
        .or_insert_with(|| "0".to_string());

    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .current_dir(&working_dir)
        .envs(&full_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0) // Create new process group so we can kill the entire tree
        .spawn()?;

    let pid = child.id().unwrap_or(0);

    {
        let mut p = proc.lock().await;
        p.pid = Some(pid);
        // If we already know the port, the service is effectively running.
        // Otherwise stay in Starting until port is auto-detected from output.
        p.status = if p.port > 0 {
            ProcessStatus::Running
        } else {
            ProcessStatus::Starting
        };
        p.started_at = Some(Utc::now());
    }

    persist_pid(&id, pid, port);

    // Read stdout
    if let Some(stdout) = child.stdout.take() {
        let proc_clone = proc.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                proc_clone.lock().await.push_stdout(line);
            }
        });
    }

    // Read stderr
    if let Some(stderr) = child.stderr.take() {
        let proc_clone = proc.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                proc_clone.lock().await.push_stderr(line);
            }
        });
    }

    // Wait for exit and mark crashed if unexpected
    let proc_clone = proc.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        let exit_status = child.wait().await;
        let mut p = proc_clone.lock().await;

        if matches!(p.status, ProcessStatus::Running | ProcessStatus::Starting) {
            let exit_code = exit_status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let stderr_tail = p.stderr_tail(20);

            // Auto-recover from EADDRINUSE: kill the port hog and flag for respawn
            if p.crash_count == 0
                && (stderr_tail.contains("EADDRINUSE") || stderr_tail.contains("address already in use"))
            {
                let port = extract_port_from_eaddrinuse(&stderr_tail).unwrap_or(p.port);
                if port > 0 {
                    p.crash_count += 1;
                    p.pid = None;
                    p.status = ProcessStatus::Stopped;
                    p.needs_respawn = true;
                    remove_pid(&id_clone);
                    drop(p);
                    kill_port(port).await;
                    return;
                }
            }

            p.status = ProcessStatus::Crashed { exit_code, stderr_tail };
            p.crash_count += 1;
            p.pid = None;
            remove_pid(&id_clone);
        }
    });

    Ok(())
}

pub async fn stop_process(proc: SharedProcess) -> Result<()> {
    let (pid, port, id) = {
        let mut p = proc.lock().await;
        let pid = p.pid.take();
        let port = p.port;
        let id = p.id.clone();
        p.status = ProcessStatus::Stopped;
        p.port = 0;
        (pid, port, id)
    };

    if let Some(pid) = pid {
        kill_pid(pid).await;
        remove_pid(&id);
    }

    // Kill anything still holding the port (child processes that survived SIGTERM)
    if port > 0 {
        kill_port(port).await;
    }

    Ok(())
}

pub async fn restart_process(proc: SharedProcess) -> Result<()> {
    let (pid, port) = {
        let mut p = proc.lock().await;
        p.status = ProcessStatus::Restarting;
        (p.pid, p.port)
    };

    if let Some(pid) = pid {
        kill_pid(pid).await;
    }

    // Also kill anything still holding the port (child processes that survived SIGTERM)
    if port > 0 {
        kill_port(port).await;
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    {
        let mut p = proc.lock().await;
        p.status = ProcessStatus::Starting;
        p.pid = None;
        p.port = 0; // Reset — will be re-detected from stdout/stderr
        p.combined_log.clear();
        p.last_stderr.clear();
    }

    spawn_process(proc).await
}

async fn kill_pid(pid: u32) {
    // Try process-group kill first (catches npm→node child trees), fall back
    // to single-pid if the group doesn't exist.
    let pgid = -(pid as i32);
    let ret = unsafe { libc::kill(pgid, libc::SIGTERM) };
    if ret != 0 {
        // Process group kill failed — kill the single process instead
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }

    for _ in 0..50 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        if !pid_exists(pid) {
            return;
        }
    }

    let ret = unsafe { libc::kill(pgid, libc::SIGKILL) };
    if ret != 0 {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}

pub fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ── State persistence ──────────────────────────────────────────────────────────

/// Simple map of service id → pid for background reconnect.
#[derive(Serialize, Deserialize, Default)]
pub struct RunState {
    /// service id (e.g. "myproject/api") → pid
    pub pids: HashMap<String, u32>,
}

pub fn load_state() -> RunState {
    let path = state_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn save_state(state: &RunState) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(state).unwrap_or_default());
}

fn persist_pid(id: &str, pid: u32, _port: u16) {
    let mut state = load_state();
    state.pids.insert(id.to_string(), pid);
    save_state(&state);
}

fn remove_pid(id: &str) {
    let mut state = load_state();
    state.pids.remove(id);
    save_state(&state);
}

/// Extract the port number from an EADDRINUSE error message.
fn extract_port_from_eaddrinuse(stderr: &str) -> Option<u16> {
    for line in stderr.lines() {
        // Match ":::8898" or "port: 8898" or ":8898"
        if let Some(pos) = line.rfind(":::") {
            if let Some(p) = line[pos + 3..].split(|c: char| !c.is_ascii_digit()).next()
                .and_then(|s| s.parse::<u16>().ok()).filter(|&p| p > 0) {
                return Some(p);
            }
        }
        if let Some(pos) = line.find("port:") {
            if let Some(p) = line[pos + 5..].trim().split(|c: char| !c.is_ascii_digit()).next()
                .and_then(|s| s.parse::<u16>().ok()).filter(|&p| p > 0) {
                return Some(p);
            }
        }
    }
    None
}

/// Scan a log line for a port number.
/// Recognises patterns like `:5111`, `port 5111`, `PORT=5111`,
/// `localhost:5111`, `0.0.0.0:5111`, `ready on 5111`, `listening on 5111`.
pub fn detect_port_in_line(line: &str) -> Option<u16> {
    let lower = line.to_ascii_lowercase();

    // Check colon-prefixed port with context: "localhost:NNNN", "0.0.0.0:NNNN",
    // "127.0.0.1:NNNN", ":::NNNN", "http://...:NNNN"
    // Avoids false positives from timestamps, PIDs, or random numbers after colons.
    let port_prefixes = ["localhost:", "127.0.0.1:", "0.0.0.0:", ":::", "[::]:",
                         "http://localhost:", "https://localhost:",
                         "http://0.0.0.0:", "http://127.0.0.1:"];
    for prefix in &port_prefixes {
        if let Some(pos) = lower.find(prefix) {
            let after = &line[pos + prefix.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(p) = digits.parse::<u16>() {
                    if p > 1024 && p < 65535 {
                        return Some(p);
                    }
                }
            }
        }
    }

    // Check keyword + number patterns (covers Express, Fastify, Koa, Django, Rails, etc.)
    for kw in &["port ", "port=", "port:", "listening on ", "running on ",
                "ready on ", "started on ", "server on ", "available at ",
                "listening at ", "bound to ", "serving on ", "started at ",
                "live on ", "accessible at ", "running at "] {
        if let Some(pos) = lower.find(kw) {
            let after = &line[pos + kw.len()..];
            let digits: String = after.chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(p) = digits.parse::<u16>() {
                if p > 1024 && p < 65535 {
                    return Some(p);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_proc(id: &str) -> ManagedProcess {
        ManagedProcess::new(
            id.to_string(),
            "echo hello".to_string(),
            std::path::PathBuf::from("/tmp"),
            3000,
            HashMap::new(),
        )
    }

    // ── ProcessStatus ────────────────────────────────────────────────────────

    #[test]
    fn status_label_running() {
        assert_eq!(ProcessStatus::Running.label(), "running");
    }

    #[test]
    fn status_label_stopped() {
        assert_eq!(ProcessStatus::Stopped.label(), "stopped");
    }

    #[test]
    fn status_label_crashed() {
        let s = ProcessStatus::Crashed { exit_code: 1, stderr_tail: "err".to_string() };
        assert_eq!(s.label(), "crashed");
    }

    #[test]
    fn status_is_crashed_true() {
        let s = ProcessStatus::Crashed { exit_code: 1, stderr_tail: String::new() };
        assert!(s.is_crashed());
    }

    #[test]
    fn status_is_crashed_false_for_running() {
        assert!(!ProcessStatus::Running.is_crashed());
    }

    #[test]
    fn status_eq() {
        assert_eq!(ProcessStatus::Running, ProcessStatus::Running);
        assert_ne!(ProcessStatus::Running, ProcessStatus::Stopped);
    }

    // ── ManagedProcess ───────────────────────────────────────────────────────

    #[test]
    fn new_process_starts_stopped() {
        let p = make_proc("test/api");
        assert_eq!(p.status, ProcessStatus::Stopped);
        assert_eq!(p.pid, None);
        assert_eq!(p.crash_count, 0);
    }

    #[test]
    fn new_process_stores_fields() {
        let p = make_proc("myproj/web");
        assert_eq!(p.id, "myproj/web");
        assert_eq!(p.command, "echo hello");
        assert_eq!(p.port, 3000);
    }

    // ── Log buffers ──────────────────────────────────────────────────────────

    #[test]
    fn push_stdout_stores_lines() {
        let mut p = make_proc("t/t");
        p.push_stdout("line 1".to_string());
        p.push_stdout("line 2".to_string());
        assert_eq!(p.stdout_log.len(), 2);
        assert_eq!(p.stdout_log[0], "line 1");
    }

    #[test]
    fn push_stderr_ring_buffer_evicts_oldest() {
        let mut p = make_proc("t/t");
        for i in 0..105 {
            p.push_stderr(format!("err {}", i));
        }
        assert_eq!(p.last_stderr.len(), 100);
        // oldest lines (0-4) should be gone; newest should be present
        assert_eq!(p.last_stderr.back().unwrap(), "err 104");
        assert!(!p.last_stderr.contains(&"err 0".to_string()));
    }

    #[test]
    fn stderr_tail_returns_last_n_lines() {
        let mut p = make_proc("t/t");
        for i in 0..10 {
            p.push_stderr(format!("line {}", i));
        }
        let tail = p.stderr_tail(3);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line 7");
        assert_eq!(lines[2], "line 9");
    }

    #[test]
    fn stderr_tail_fewer_lines_than_requested() {
        let mut p = make_proc("t/t");
        p.push_stderr("only line".to_string());
        let tail = p.stderr_tail(10);
        assert_eq!(tail, "only line");
    }

    // ── detect_port_in_line ─────────────────────────────────────────────────

    #[test]
    fn detect_port_localhost_colon() {
        assert_eq!(detect_port_in_line("  Local: http://localhost:5111/"), Some(5111));
    }

    #[test]
    fn detect_port_zero_addr() {
        assert_eq!(detect_port_in_line("Listening on 0.0.0.0:3000"), Some(3000));
    }

    #[test]
    fn detect_port_127_addr() {
        assert_eq!(detect_port_in_line("http://127.0.0.1:4000"), Some(4000));
    }

    #[test]
    fn detect_port_triple_colon() {
        assert_eq!(detect_port_in_line("Listening on :::8898"), Some(8898));
    }

    #[test]
    fn detect_port_keyword_listening_on() {
        assert_eq!(detect_port_in_line("Server listening on port 5173"), Some(5173));
    }

    #[test]
    fn detect_port_keyword_port_equals() {
        assert_eq!(detect_port_in_line("PORT=4000"), Some(4000));
    }

    #[test]
    fn detect_port_keyword_ready_on() {
        assert_eq!(detect_port_in_line("ready on 3001"), Some(3001));
    }

    #[test]
    fn detect_port_keyword_running_at() {
        assert_eq!(detect_port_in_line("running at http://localhost:5111"), Some(5111));
    }

    #[test]
    fn detect_port_rejects_privileged_ports() {
        // Ports <= 1024 are filtered out to avoid false positives
        assert_eq!(detect_port_in_line("localhost:80"), None);
        assert_eq!(detect_port_in_line("localhost:443"), None);
    }

    #[test]
    fn detect_port_rejects_out_of_range() {
        assert_eq!(detect_port_in_line("localhost:65535"), None);
        assert_eq!(detect_port_in_line("localhost:70000"), None);
    }

    #[test]
    fn detect_port_no_false_positive_timestamp() {
        // Timestamps like "12:34:56" shouldn't match
        assert_eq!(detect_port_in_line("2026-03-16 12:34:56 INFO started"), None);
    }

    #[test]
    fn detect_port_no_false_positive_random_text() {
        assert_eq!(detect_port_in_line("Compiling rundev v0.1.0"), None);
    }

    #[test]
    fn detect_port_nuxt_output() {
        assert_eq!(
            detect_port_in_line("  ➜ Local:    http://localhost:3000/"),
            Some(3000),
        );
    }

    #[test]
    fn detect_port_express_output() {
        assert_eq!(
            detect_port_in_line("Express server listening on port 8898"),
            Some(8898),
        );
    }

    // ── push_stdout / push_stderr auto-detect port + status transition ──────

    #[test]
    fn push_stdout_detects_port_and_transitions_to_running() {
        let mut p = make_proc("t/t");
        p.port = 0;
        p.status = ProcessStatus::Starting;
        p.push_stdout("Listening on http://localhost:4567".to_string());
        assert_eq!(p.port, 4567);
        assert_eq!(p.status, ProcessStatus::Running);
    }

    #[test]
    fn push_stderr_detects_port_and_transitions_to_running() {
        let mut p = make_proc("t/t");
        p.port = 0;
        p.status = ProcessStatus::Starting;
        p.push_stderr("Server ready on port 9090".to_string());
        assert_eq!(p.port, 9090);
        assert_eq!(p.status, ProcessStatus::Running);
    }

    #[test]
    fn push_stdout_does_not_overwrite_known_port() {
        let mut p = make_proc("t/t");
        p.port = 3000; // already known
        p.push_stdout("Now listening on port 9999".to_string());
        assert_eq!(p.port, 3000, "should not overwrite a known port");
    }

    #[test]
    fn push_stdout_no_port_stays_starting() {
        let mut p = make_proc("t/t");
        p.port = 0;
        p.status = ProcessStatus::Starting;
        p.push_stdout("Compiling...".to_string());
        assert_eq!(p.port, 0);
        assert_eq!(p.status, ProcessStatus::Starting);
    }
}
