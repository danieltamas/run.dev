//! AI-powered crash diagnosis and interactive Q&A.
//!
//! Sends service stderr output (and optional context) to Claude to get a plain-
//! English explanation of what went wrong and a suggested fix. Also powers the
//! `[/]` command bar in the dashboard for free-form questions about your stack.
//!
//! The Anthropic API key is read from `~/.config/rundev/config.yaml`. When no key
//! is configured the module degrades gracefully — all functions return `None`.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

#[cfg(feature = "ai")]
use reqwest::Client;
use serde_json::json;

use crate::core::config::load_global_config;

fn system_prompt() -> String {
    let os = std::env::consts::OS; // "macos", "linux", "windows"

    let port_forwarding = if os == "macos" {
        "macOS pfctl (pf firewall): rules are in /etc/pf.anchors/rundev, loaded via `sudo pfctl -ef /etc/pf.anchors/rundev` on each launch. Requires a NOPASSWD sudoers entry for /sbin/pfctl in /etc/sudoers.d/rundev."
    } else if os == "linux" {
        "Linux iptables: rules added via `sudo iptables -t nat -A OUTPUT -p tcp --dport 80 -j REDIRECT --to-port 1111` (and 443→1112). Persisted via iptables-save to /etc/iptables/rules.v4."
    } else {
        "Port forwarding is not configured on this platform."
    };

    let config_dir = if os == "macos" {
        "~/Library/Application Support/rundev/"
    } else {
        "~/.config/rundev/"
    };

    format!(r#"You are a diagnostic assistant built into run.dev, an AI-native local development environment.

## What run.dev does
- Manages multiple local development services (Node, Rust, Go, Python, Rails, etc.) as child processes
- Provides a terminal UI (TUI) built with Rust/ratatui showing status, logs, CPU/memory per service
- Routes custom local domains (e.g. win.wam.app) to the correct service port via a reverse HTTP proxy
- Manages /etc/hosts entries and SSL certificates (self-signed, generated via rcgen) for each project domain
- Uses Claude AI for crash diagnosis and developer Q&A (that's you)

## Routing architecture
The full request path for a custom domain is:
  Browser → domain (e.g. win.wam.app)
    → /etc/hosts maps it to 127.0.0.1
    → Port forwarding redirects port 80 → 1111 (and 443 → 1112)
    → run.dev's reverse proxy (listening on 127.0.0.1:1111) reads the Host header
    → Routes to the correct local service port (e.g. localhost:5111)

Port forwarding on this system ({os}): {port_forwarding}

## Port detection
run.dev does NOT require manual port configuration. It auto-detects the port each service uses by scanning the first 50 lines of stdout/stderr for patterns like `:5111`, `port 5111`, `listening on 5111`, `http://localhost:5111`, etc. The detected port updates the proxy route table automatically (every ~2 seconds).

## Service lifecycle
- Services are spawned as child processes with stdout/stderr captured into 100-line ring buffers
- Status transitions: Stopped → Starting → Running | Crashed
- On crash, run.dev captures the exit code and stderr tail for AI diagnosis
- On run.dev exit, all services receive SIGTERM (graceful shutdown)

## Configuration
Projects and services are stored as YAML files in {config_dir}projects/<name>.yaml.
Each service has: path (working directory), command (e.g. `npm run dev`), optional subdomain.
The subdomain field controls routing: if empty and the service name contains a dot, the service name itself is treated as the full domain.

## Privileged helper
/etc/hosts is written through a tiny helper binary at /usr/local/bin/rundev-hosts-helper with a sudoers NOPASSWD rule in /etc/sudoers.d/rundev. This is installed once via `rundev setup`.

## Common issues and fixes
- ERR_CONNECTION_REFUSED: port forwarding not active, proxy not running, or service hasn't started yet
- ERR_NAME_NOT_RESOLVED / DNS_PROBE_FINISHED_NXDOMAIN: /etc/hosts missing the domain entry
- Wrong port: service may not have output its port yet — wait a few seconds for auto-detection
- Service crashes immediately: check the logs panel ([l] key) for the actual error
- pfctl not working: run `rundev setup` to (re-)install the sudoers rule for /sbin/pfctl

Be concise and direct. One or two short paragraphs max. Include a fix command when relevant."#,
        os = os,
        port_forwarding = port_forwarding,
        config_dir = config_dir,
    )
}

pub async fn ask_claude(prompt: &str) -> Option<String> {
    ask_claude_inner(prompt).await
}

#[cfg(feature = "ai")]
async fn ask_claude_inner(prompt: &str) -> Option<String> {
    let config = load_global_config();
    let proxy_url = config.claude_proxy?;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;

    let response = client
        .post(format!("{}/chat/completions", proxy_url))
        .json(&json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {
                    "role": "system",
                    "content": system_prompt()
                },
                {
                    "role": "user",
                    "content": prompt
                }
            ]
        }))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let data: serde_json::Value = response.json().await.ok()?;
    data["choices"][0]["message"]["content"]
        .as_str()
        .map(String::from)
}

#[cfg(not(feature = "ai"))]
async fn ask_claude_inner(_prompt: &str) -> Option<String> {
    None
}

pub async fn diagnose_crash(service: &str, stderr: &str) -> Option<String> {
    let prompt = format!(
        "Service '{}' crashed. Here's the stderr output:\n\n{}\n\nWhat went wrong and how do I fix it?",
        service, stderr
    );
    ask_claude(&prompt).await
}

pub async fn answer_question(question: &str, context: &str) -> Option<String> {
    let prompt = if context.is_empty() {
        question.to_string()
    } else {
        format!("Context about my dev environment:\n{}\n\nQuestion: {}", context, question)
    };
    ask_claude(&prompt).await
}
