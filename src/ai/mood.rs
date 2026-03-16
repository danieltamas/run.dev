//! Personality and mood system — gives run.dev an expressive voice.
//!
//! The overall [`Mood`] is derived from the aggregate health of all running
//! services and displayed in the dashboard header. When a service crashes,
//! [`crash_message`] generates a contextual one-liner using the stderr tail
//! and resource stats, and [`auto_fix_action`] optionally suggests a fix command
//! for Claude to execute.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use crate::core::process::{ManagedProcess, ProcessStatus};

#[derive(Debug, Clone, PartialEq)]
pub enum Mood {
    Vibing,
    Chill,
    GotTheFlu,
    Wounded,
    Flatlined,
    Fixing,
}

impl Mood {
    pub fn emoji(&self) -> &str {
        match self {
            Mood::Vibing => "😎",
            Mood::Chill => "😌",
            Mood::GotTheFlu => "🤒",
            Mood::Wounded => "🤕",
            Mood::Flatlined => "💀",
            Mood::Fixing => "🔧",
        }
    }

    pub fn message(&self, context: &str) -> String {
        match self {
            Mood::Vibing => "vibing".to_string(),
            Mood::Chill => "chill — minor warnings".to_string(),
            Mood::GotTheFlu => format!("got the flu — {} needs attention", context),
            Mood::Wounded => format!("wounded — {} services down", context),
            Mood::Flatlined => "flatlined — everything is down".to_string(),
            Mood::Fixing => "fixing...".to_string(),
        }
    }
}

pub fn calculate_mood(processes: &[ManagedProcess]) -> Mood {
    if processes.is_empty() {
        return Mood::Flatlined;
    }

    let total = processes.len();
    let running = processes
        .iter()
        .filter(|p| p.status == ProcessStatus::Running)
        .count();
    let crashed = processes
        .iter()
        .filter(|p| p.status.is_crashed())
        .count();
    let restarting = processes
        .iter()
        .filter(|p| p.status == ProcessStatus::Restarting)
        .count();
    let high_mem = processes
        .iter()
        .filter(|p| {
            p.memory_bytes > 0
                && p.memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0) > 0.8
        })
        .count();

    if restarting > 0 {
        return Mood::Fixing;
    }

    if running == 0 {
        return Mood::Flatlined;
    }

    if crashed == 0 && running == total {
        if high_mem > 0 {
            return Mood::Chill;
        }
        return Mood::Vibing;
    }

    let down_ratio = crashed as f64 / total as f64;

    if down_ratio > 0.5 {
        Mood::Wounded
    } else if crashed > 0 {
        Mood::GotTheFlu
    } else {
        Mood::Chill
    }
}

// ── Error categorization ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ErrorKind {
    PortInUse(u16),
    ModuleNotFound(String),
    SyntaxError(String, String),
    ConnectionRefused(String, u16),
    OutOfMemory,
    Unknown,
}

pub struct CrashInfo {
    pub stderr_tail: String,
    pub port: u16,
    pub peak_memory_mb: u64,
}

pub fn categorize_error(stderr: &str) -> ErrorKind {
    if stderr.contains("EADDRINUSE") || stderr.contains("address already in use") {
        // Extract port from patterns like ":::8898", "port 8898", ":8898"
        let port = stderr.lines().find_map(|line| {
            if let Some(pos) = line.rfind(":::") {
                line[pos + 3..].split(|c: char| !c.is_ascii_digit()).next()
                    .and_then(|s| s.parse::<u16>().ok())
            } else if let Some(pos) = line.find("port:") {
                line[pos + 5..].trim().split(|c: char| !c.is_ascii_digit()).next()
                    .and_then(|s| s.parse::<u16>().ok())
            } else if let Some(pos) = line.rfind(':') {
                line[pos + 1..].split(|c: char| !c.is_ascii_digit()).next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .filter(|&p| p > 0)
            } else {
                None
            }
        }).unwrap_or(0);
        ErrorKind::PortInUse(port)
    } else if stderr.contains("Cannot find module") || stderr.contains("ModuleNotFoundError") {
        let module = extract_module_name(stderr);
        ErrorKind::ModuleNotFound(module)
    } else if stderr.contains("SyntaxError") {
        let (file, line) = extract_syntax_location(stderr);
        ErrorKind::SyntaxError(file, line)
    } else if stderr.contains("ECONNREFUSED") {
        let (host, port) = extract_connection_target(stderr);
        ErrorKind::ConnectionRefused(host, port)
    } else if stderr.contains("JavaScript heap out of memory") || stderr.contains("OOMKilled") {
        ErrorKind::OutOfMemory
    } else {
        ErrorKind::Unknown
    }
}

pub fn crash_message(service: &str, error: &CrashInfo) -> String {
    let kind = categorize_error(&error.stderr_tail);
    match kind {
        ErrorKind::PortInUse(port) => {
            let p = if port > 0 { port } else { error.port };
            format!(
                "bro, {} is ded. port {} is already taken.\ni know what's wrong. press [f] to let me fix it",
                service, p
            )
        }
        ErrorKind::ModuleNotFound(module) => format!(
            "{} crashed. missing module: {}.\nrun `npm install` maybe? press [f] and i'll do it",
            service, module
        ),
        ErrorKind::SyntaxError(file, line) => format!(
            "{} has a syntax error in {}:{}\ncan't auto-fix this one. press [l] to see the logs",
            service, file, line
        ),
        ErrorKind::ConnectionRefused(host, port) => format!(
            "{} can't reach {}:{}. is that service running?\npress [s] on it to start it",
            service, host, port
        ),
        ErrorKind::OutOfMemory => format!(
            "{} ran out of memory (was using {}MB).\nkill something else or give it more room",
            service, error.peak_memory_mb
        ),
        ErrorKind::Unknown => format!(
            "{} crashed. not sure why yet.\npress [l] for logs or [/] to ask me about it",
            service
        ),
    }
}

// ── Auto-fix ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum FixAction {
    KillPort(u16),
    RunCommand { cmd: String, cwd: std::path::PathBuf },
    StartDependency,
}

pub fn auto_fix_action(error: &ErrorKind, proc: &ManagedProcess) -> Option<FixAction> {
    match error {
        ErrorKind::PortInUse(port) => {
            let p = if *port > 0 { *port } else { proc.port };
            Some(FixAction::KillPort(p))
        }
        ErrorKind::ModuleNotFound(_) => Some(FixAction::RunCommand {
            cmd: "npm install".to_string(),
            cwd: proc.working_dir.clone(),
        }),
        ErrorKind::ConnectionRefused(_, _) => Some(FixAction::StartDependency),
        _ => None,
    }
}

pub async fn execute_fix(action: &FixAction) -> Result<String, String> {
    match action {
        FixAction::KillPort(port) => {
            // Find and kill process using this port
            let _output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(format!("lsof -ti :{} | xargs kill -9 2>/dev/null; echo done", port))
                .output()
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("killed process on port {}", port))
        }
        FixAction::RunCommand { cmd, cwd } => {
            let mut parts = cmd.split_whitespace();
            let program = parts.next().unwrap_or("sh");
            let args: Vec<&str> = parts.collect();
            let output = tokio::process::Command::new(program)
                .args(&args)
                .current_dir(cwd)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if output.status.success() {
                Ok(format!("ran `{}`", cmd))
            } else {
                Err(String::from_utf8_lossy(&output.stderr).to_string())
            }
        }
        FixAction::StartDependency => Err("need to start dependency manually".to_string()),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn extract_module_name(stderr: &str) -> String {
    // "Cannot find module 'foo'" or "ModuleNotFoundError: No module named 'foo'"
    if let Some(start) = stderr.find("Cannot find module '") {
        let rest = &stderr[start + 20..];
        if let Some(end) = rest.find('\'') {
            return rest[..end].to_string();
        }
    }
    if let Some(start) = stderr.find("No module named '") {
        let rest = &stderr[start + 17..];
        if let Some(end) = rest.find('\'') {
            return rest[..end].to_string();
        }
    }
    "unknown".to_string()
}

fn extract_syntax_location(stderr: &str) -> (String, String) {
    // Look for "file.js:42" pattern
    for line in stderr.lines() {
        if line.contains("SyntaxError") {
            continue;
        }
        // Find pattern like "at /path/to/file.js:42"
        if let Some(at) = line.find("at ") {
            let rest = &line[at + 3..];
            if let Some(colon) = rest.rfind(':') {
                let file = rest[..colon].to_string();
                let line_num = rest[colon + 1..].trim().to_string();
                if line_num.parse::<u32>().is_ok() {
                    return (file, line_num);
                }
            }
        }
    }
    ("unknown".to_string(), "?".to_string())
}

fn extract_connection_target(stderr: &str) -> (String, u16) {
    // Look for "ECONNREFUSED 127.0.0.1:3000"
    if let Some(idx) = stderr.find("ECONNREFUSED ") {
        let rest = &stderr[idx + 13..];
        let target = rest.split_whitespace().next().unwrap_or("");
        if let Some(colon) = target.rfind(':') {
            let host = target[..colon].to_string();
            let port = target[colon + 1..].parse().unwrap_or(0);
            return (host, port);
        }
    }
    ("localhost".to_string(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::process::{ManagedProcess, ProcessStatus};
    use std::collections::HashMap;

    fn proc_with_status(status: ProcessStatus) -> ManagedProcess {
        let mut p = ManagedProcess::new(
            "proj/svc".to_string(),
            "echo".to_string(),
            std::path::PathBuf::from("/tmp"),
            3000,
            HashMap::new(),
        );
        p.status = status;
        p
    }

    // ── Mood::emoji / message ────────────────────────────────────────────────

    #[test]
    fn mood_vibing_emoji() {
        assert_eq!(Mood::Vibing.emoji(), "😎");
    }

    #[test]
    fn mood_message_vibing() {
        assert_eq!(Mood::Vibing.message(""), "vibing");
    }

    #[test]
    fn mood_message_with_context() {
        let msg = Mood::GotTheFlu.message("api");
        assert!(msg.contains("api"));
    }

    // ── calculate_mood ───────────────────────────────────────────────────────

    #[test]
    fn mood_empty_processes_is_flatlined() {
        assert_eq!(calculate_mood(&[]), Mood::Flatlined);
    }

    #[test]
    fn mood_all_running_is_vibing() {
        let procs = vec![
            proc_with_status(ProcessStatus::Running),
            proc_with_status(ProcessStatus::Running),
        ];
        assert_eq!(calculate_mood(&procs), Mood::Vibing);
    }

    #[test]
    fn mood_all_stopped_no_running_is_flatlined() {
        let procs = vec![
            proc_with_status(ProcessStatus::Stopped),
            proc_with_status(ProcessStatus::Stopped),
        ];
        assert_eq!(calculate_mood(&procs), Mood::Flatlined);
    }

    #[test]
    fn mood_any_restarting_is_fixing() {
        let procs = vec![
            proc_with_status(ProcessStatus::Running),
            proc_with_status(ProcessStatus::Restarting),
        ];
        assert_eq!(calculate_mood(&procs), Mood::Fixing);
    }

    #[test]
    fn mood_one_crashed_minority_is_got_the_flu() {
        let procs = vec![
            proc_with_status(ProcessStatus::Running),
            proc_with_status(ProcessStatus::Running),
            proc_with_status(ProcessStatus::Crashed { exit_code: 1, stderr_tail: String::new() }),
        ];
        assert_eq!(calculate_mood(&procs), Mood::GotTheFlu);
    }

    #[test]
    fn mood_majority_crashed_is_wounded() {
        let procs = vec![
            proc_with_status(ProcessStatus::Running),
            proc_with_status(ProcessStatus::Crashed { exit_code: 1, stderr_tail: String::new() }),
            proc_with_status(ProcessStatus::Crashed { exit_code: 1, stderr_tail: String::new() }),
        ];
        assert_eq!(calculate_mood(&procs), Mood::Wounded);
    }

    #[test]
    fn mood_high_memory_is_chill_not_vibing() {
        let mut p = proc_with_status(ProcessStatus::Running);
        p.memory_bytes = (0.9 * 1024.0 * 1024.0 * 1024.0) as u64; // 900MB > 800MB threshold
        assert_eq!(calculate_mood(&[p]), Mood::Chill);
    }

    // ── categorize_error ─────────────────────────────────────────────────────

    #[test]
    fn categorize_port_in_use() {
        let kind = categorize_error("Error: listen EADDRINUSE :::3000");
        assert!(matches!(kind, ErrorKind::PortInUse(3000)));
    }

    #[test]
    fn categorize_port_in_use_alt_message() {
        let kind = categorize_error("address already in use");
        assert!(matches!(kind, ErrorKind::PortInUse(_)));
    }

    #[test]
    fn categorize_module_not_found_node() {
        let kind = categorize_error("Error: Cannot find module 'express'");
        match kind {
            ErrorKind::ModuleNotFound(m) => assert_eq!(m, "express"),
            _ => panic!("expected ModuleNotFound"),
        }
    }

    #[test]
    fn categorize_module_not_found_python() {
        let kind = categorize_error("ModuleNotFoundError: No module named 'django'");
        match kind {
            ErrorKind::ModuleNotFound(m) => assert_eq!(m, "django"),
            _ => panic!("expected ModuleNotFound"),
        }
    }

    #[test]
    fn categorize_syntax_error() {
        let kind = categorize_error("SyntaxError: Unexpected token\n  at /app/server.js:42");
        assert!(matches!(kind, ErrorKind::SyntaxError(_, _)));
    }

    #[test]
    fn categorize_connection_refused() {
        let kind = categorize_error("Error: connect ECONNREFUSED 127.0.0.1:5432");
        match kind {
            ErrorKind::ConnectionRefused(host, port) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 5432);
            }
            _ => panic!("expected ConnectionRefused"),
        }
    }

    #[test]
    fn categorize_oom() {
        let kind = categorize_error("FATAL ERROR: JavaScript heap out of memory");
        assert!(matches!(kind, ErrorKind::OutOfMemory));
    }

    #[test]
    fn categorize_unknown() {
        let kind = categorize_error("something weird happened");
        assert!(matches!(kind, ErrorKind::Unknown));
    }

    // ── crash_message ────────────────────────────────────────────────────────

    #[test]
    fn crash_message_port_in_use_mentions_service_and_port() {
        let info = CrashInfo {
            stderr_tail: "EADDRINUSE :::3000".to_string(),
            port: 3000,
            peak_memory_mb: 0,
        };
        let msg = crash_message("api", &info);
        assert!(msg.contains("api"));
        assert!(msg.contains("3000"));
    }

    #[test]
    fn crash_message_unknown_mentions_service() {
        let info = CrashInfo {
            stderr_tail: "something random".to_string(),
            port: 3000,
            peak_memory_mb: 0,
        };
        let msg = crash_message("worker", &info);
        assert!(msg.contains("worker"));
    }

    #[test]
    fn crash_message_oom_mentions_memory() {
        let info = CrashInfo {
            stderr_tail: "JavaScript heap out of memory".to_string(),
            port: 3000,
            peak_memory_mb: 1024,
        };
        let msg = crash_message("frontend", &info);
        assert!(msg.contains("1024"));
    }
}
