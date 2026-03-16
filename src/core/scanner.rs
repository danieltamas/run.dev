//! Project directory scanner — auto-detects start commands and ports.
//!
//! Given a directory path, this module inspects common config files
//! (`package.json`, `Pipfile`, `Cargo.toml`, etc.) to suggest the most likely
//! start command and port for a new service, saving the user from typing them
//! manually.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DetectedCommand {
    pub label: String,       // display label, e.g. "npm run dev"
    pub command: String,     // actual shell command
    pub recommended: bool,
    #[allow(dead_code)]
    pub port: Option<u16>,
}

/// Scan a directory and return all detected runnable commands, recommended first.
pub fn detect_commands(dir: &Path) -> Vec<DetectedCommand> {
    let mut results = vec![];

    // package.json
    if dir.join("package.json").exists() {
        results.extend(detect_node_commands(dir));
        if !results.is_empty() {
            // Add "enter custom command..." sentinel at the end
            results.push(DetectedCommand {
                label: "enter custom command...".to_string(),
                command: String::new(),
                recommended: false,
                port: None,
            });
            return results;
        }
    }

    // Cargo.toml
    if dir.join("Cargo.toml").exists() {
        let port = read_env_port(dir).unwrap_or(8080);
        results.push(DetectedCommand {
            label: "cargo run".to_string(),
            command: "cargo run".to_string(),
            recommended: true,
            port: Some(port),
        });
        results.push(DetectedCommand {
            label: "cargo run --release".to_string(),
            command: "cargo run --release".to_string(),
            recommended: false,
            port: Some(port),
        });
    }

    // go.mod
    if dir.join("go.mod").exists() {
        let port = read_env_port(dir).unwrap_or(8080);
        results.push(DetectedCommand {
            label: "go run .".to_string(),
            command: "go run .".to_string(),
            recommended: true,
            port: Some(port),
        });
    }

    // Procfile
    if dir.join("Procfile").exists() {
        results.extend(detect_procfile_commands(dir));
    }

    // manage.py (Django)
    if dir.join("manage.py").exists() {
        let port = read_env_port(dir).unwrap_or(8000);
        results.push(DetectedCommand {
            label: "python manage.py runserver".to_string(),
            command: "python manage.py runserver".to_string(),
            recommended: true,
            port: Some(port),
        });
    }

    // Gemfile (Rails)
    if dir.join("Gemfile").exists() {
        let port = read_env_port(dir).unwrap_or(3000);
        results.push(DetectedCommand {
            label: "bundle exec rails server".to_string(),
            command: "bundle exec rails server".to_string(),
            recommended: true,
            port: Some(port),
        });
    }

    // docker-compose.yml — extract command fields but run directly (not via docker)
    if dir.join("docker-compose.yml").exists() || dir.join("docker-compose.yaml").exists() {
        results.extend(detect_compose_commands(dir));
    }

    // Always add custom command option
    results.push(DetectedCommand {
        label: "enter custom command...".to_string(),
        command: String::new(),
        recommended: false,
        port: None,
    });

    results
}

fn detect_node_commands(dir: &Path) -> Vec<DetectedCommand> {
    let content = match std::fs::read_to_string(dir.join("package.json")) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let json: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let scripts = match json.get("scripts").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return vec![],
    };

    // Priority order for recommendation
    let priority = ["dev", "start", "serve", "watch"];
    let mut results = vec![];
    let mut seen = std::collections::HashSet::new();

    // Recommended first
    for key in &priority {
        if let Some(cmd_val) = scripts.get(*key) {
            let cmd_str = cmd_val.as_str().unwrap_or("");
            let npm_cmd = format!("npm run {}", key);
            let port = infer_node_port(dir, &json, cmd_str);
            results.push(DetectedCommand {
                label: npm_cmd.clone(),
                command: npm_cmd,
                recommended: results.is_empty(), // first one is recommended
                port,
            });
            seen.insert(key.to_string());
        }
    }

    // All remaining scripts
    for (key, cmd_val) in scripts {
        if seen.contains(key) {
            continue;
        }
        // Skip build/test/lint/etc. — not useful as start commands
        if matches!(
            key.as_str(),
            "build" | "test" | "lint" | "format" | "type-check" | "typecheck" | "clean"
                | "postinstall" | "prepare" | "prepublish"
        ) {
            continue;
        }
        let cmd_str = cmd_val.as_str().unwrap_or("");
        let npm_cmd = format!("npm run {}", key);
        let port = infer_node_port(dir, &json, cmd_str);
        results.push(DetectedCommand {
            label: npm_cmd.clone(),
            command: npm_cmd,
            recommended: false,
            port,
        });
    }

    results
}

fn detect_procfile_commands(dir: &Path) -> Vec<DetectedCommand> {
    let content = match std::fs::read_to_string(dir.join("Procfile")) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    content
        .lines()
        .filter_map(|line| {
            let (name, cmd) = line.split_once(':')?;
            let cmd = cmd.trim().to_string();
            let port = extract_port_from_command(&cmd).or_else(|| read_env_port(dir));
            Some(DetectedCommand {
                label: format!("{}: {}", name.trim(), cmd),
                command: cmd,
                recommended: name.trim() == "web",
                port,
            })
        })
        .collect()
}

fn detect_compose_commands(dir: &Path) -> Vec<DetectedCommand> {
    let path = if dir.join("docker-compose.yml").exists() {
        dir.join("docker-compose.yml")
    } else {
        dir.join("docker-compose.yaml")
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let yaml: Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut results = vec![];
    if let Some(services) = yaml.get("services").and_then(|s| s.as_object()) {
        for (name, svc) in services {
            if let Some(command) = svc.get("command").and_then(|c| c.as_str()) {
                let port = svc
                    .get("ports")
                    .and_then(|p| p.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|p| p.as_str())
                    .and_then(|p| p.split(':').last())
                    .and_then(|p| p.parse().ok());

                results.push(DetectedCommand {
                    label: format!("{}: {}", name, command),
                    command: command.to_string(),
                    recommended: false,
                    port,
                });
            }
        }
    }
    results
}

// ── Port inference ─────────────────────────────────────────────────────────────

fn infer_node_port(dir: &Path, json: &Value, script_cmd: &str) -> Option<u16> {
    if let Some(p) = extract_port_from_command(script_cmd) {
        return Some(p);
    }
    if let Some(p) = read_env_port(dir) {
        return Some(p);
    }
    if let Some(proxy) = json.get("proxy").and_then(|v| v.as_str()) {
        if let Some(p) = extract_port_from_url(proxy) {
            return Some(p);
        }
    }
    Some(infer_framework_port(dir, json))
}

fn infer_framework_port(dir: &Path, json: &Value) -> u16 {
    let check_deps = |key: &str| -> Option<&Value> {
        json.get(key).and_then(|d| if d.is_object() { Some(d) } else { None })
    };

    for deps_key in &["dependencies", "devDependencies"] {
        if let Some(deps) = check_deps(deps_key) {
            if deps.get("next").is_some() { return 3000; }
            if deps.get("vite").is_some() { return 5173; }
            if deps.get("react-scripts").is_some() { return 3000; }
            if deps.get("nuxt").is_some() || deps.get("@nuxt/core").is_some() { return 3000; }
            if deps.get("express").is_some() || deps.get("fastify").is_some() { return 3000; }
        }
    }

    if dir.join("vite.config.ts").exists() || dir.join("vite.config.js").exists() {
        return 5173;
    }
    if dir.join("next.config.js").exists() || dir.join("next.config.ts").exists() {
        return 3000;
    }

    3000
}

pub fn read_env_port(dir: &Path) -> Option<u16> {
    for name in &[".env", ".env.local", ".env.development"] {
        if let Ok(content) = std::fs::read_to_string(dir.join(name)) {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() { continue; }
                if let Some((key, val)) = line.split_once('=') {
                    if key.trim() == "PORT" {
                        let val = val.trim().trim_matches('"').trim_matches('\'');
                        if let Ok(p) = val.parse::<u16>() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn extract_port_from_command(cmd: &str) -> Option<u16> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    for i in 0..tokens.len() {
        if tokens[i] == "--port" || tokens[i] == "-p" {
            if let Some(p) = tokens.get(i + 1) {
                return p.parse().ok();
            }
        }
        if let Some(p) = tokens[i].strip_prefix("--port=") {
            return p.parse().ok();
        }
    }
    None
}

fn extract_port_from_url(url: &str) -> Option<u16> {
    url.rsplit(':').next()?.parse().ok()
}

// ── Name cleaning ──────────────────────────────────────────────────────────────

/// Suggest a clean service name from a raw folder name.
/// Examples:
///   "api.wam.app.4.0"  → "api"
///   "backend-v2.3.1"   → "backend"
///   "my_frontend_app"  → "my-frontend-app"
///   "web"              → "web"
pub fn clean_service_name(folder_name: &str) -> String {
    // Take just the last path component
    let base = Path::new(folder_name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(folder_name);

    // Split on dots, hyphens, underscores
    let parts: Vec<&str> = base.split(|c| c == '.' || c == '-' || c == '_').collect();

    // Keep leading parts that don't look like version numbers
    let meaningful: Vec<&str> = parts
        .iter()
        .take_while(|&&p| !is_version_segment(p))
        .copied()
        .collect();

    let name = if meaningful.is_empty() {
        parts.first().copied().unwrap_or(base).to_string()
    } else {
        meaningful.join("-")
    };

    name.to_lowercase()
}

fn is_version_segment(s: &str) -> bool {
    if s.is_empty() { return false; }
    // Pure digits: "4", "2"
    if s.chars().all(|c| c.is_ascii_digit()) { return true; }
    // v-prefixed version: "v2", "v2.3"
    if s.starts_with('v') && s[1..].chars().all(|c| c.is_ascii_digit() || c == '.') {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── clean_service_name ───────────────────────────────────────────────────

    #[test]
    fn clean_name_strips_version_dots() {
        assert_eq!(clean_service_name("api.wam.app.4.0"), "api-wam-app");
    }

    #[test]
    fn clean_name_strips_semver_suffix() {
        assert_eq!(clean_service_name("backend-v2"), "backend");
    }

    #[test]
    fn clean_name_plain_name() {
        assert_eq!(clean_service_name("web"), "web");
    }

    #[test]
    fn clean_name_underscores_to_hyphens() {
        assert_eq!(clean_service_name("my_frontend_app"), "my-frontend-app");
    }

    #[test]
    fn clean_name_lowercases() {
        assert_eq!(clean_service_name("MyAPI"), "myapi");
    }

    #[test]
    fn clean_name_path_takes_last_component() {
        assert_eq!(clean_service_name("/home/user/projects/api.wam.4.0"), "api-wam");
    }

    // ── extract_port_from_command ────────────────────────────────────────────

    #[test]
    fn port_from_command_long_flag_space() {
        assert_eq!(extract_port_from_command("node server.js --port 4000"), Some(4000));
    }

    #[test]
    fn port_from_command_long_flag_equals() {
        assert_eq!(extract_port_from_command("vite --port=5173"), Some(5173));
    }

    #[test]
    fn port_from_command_short_flag() {
        assert_eq!(extract_port_from_command("python -m http.server -p 8888"), Some(8888));
    }

    #[test]
    fn port_from_command_no_port() {
        assert_eq!(extract_port_from_command("npm run dev"), None);
    }

    #[test]
    fn port_from_command_invalid_port() {
        assert_eq!(extract_port_from_command("node server.js --port notanumber"), None);
    }

    // ── read_env_port ────────────────────────────────────────────────────────

    #[test]
    fn env_port_reads_dotenv() {
        let dir = tempdir();
        fs::write(dir.join(".env"), "PORT=4321\n").unwrap();
        assert_eq!(read_env_port(&dir), Some(4321));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_port_dotenv_takes_priority_over_dotenv_local() {
        // .env is checked first in the priority list
        let dir = tempdir();
        fs::write(dir.join(".env"), "PORT=1111\n").unwrap();
        fs::write(dir.join(".env.local"), "PORT=2222\n").unwrap();
        assert_eq!(read_env_port(&dir), Some(1111));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_port_ignores_comments() {
        let dir = tempdir();
        fs::write(dir.join(".env"), "# PORT=9999\nPORT=3000\n").unwrap();
        assert_eq!(read_env_port(&dir), Some(3000));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_port_strips_quotes() {
        let dir = tempdir();
        fs::write(dir.join(".env"), "PORT=\"8080\"\n").unwrap();
        assert_eq!(read_env_port(&dir), Some(8080));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_port_missing_returns_none() {
        let dir = tempdir();
        assert_eq!(read_env_port(&dir), None);
        fs::remove_dir_all(&dir).ok();
    }

    // ── detect_commands ──────────────────────────────────────────────────────

    #[test]
    fn detect_cargo_project() {
        let dir = tempdir();
        fs::write(dir.join("Cargo.toml"), "[package]\nname=\"test\"\n").unwrap();
        let cmds = detect_commands(&dir);
        assert!(cmds.iter().any(|c| c.command == "cargo run"));
        let rec = cmds.iter().find(|c| c.command == "cargo run").unwrap();
        assert!(rec.recommended);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_node_project_marks_dev_recommended() {
        let dir = tempdir();
        fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"vite","build":"vite build"}}"#).unwrap();
        let cmds = detect_commands(&dir);
        let dev = cmds.iter().find(|c| c.command == "npm run dev").unwrap();
        assert!(dev.recommended);
        // build should not appear
        assert!(!cmds.iter().any(|c| c.command == "npm run build"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_node_project_includes_custom_sentinel() {
        let dir = tempdir();
        fs::write(dir.join("package.json"), r#"{"scripts":{"start":"node index.js"}}"#).unwrap();
        let cmds = detect_commands(&dir);
        assert!(cmds.last().map(|c| c.command.is_empty()).unwrap_or(false));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_django_project() {
        let dir = tempdir();
        fs::write(dir.join("manage.py"), "# django").unwrap();
        let cmds = detect_commands(&dir);
        assert!(cmds.iter().any(|c| c.command == "python manage.py runserver"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_procfile() {
        let dir = tempdir();
        fs::write(dir.join("Procfile"), "web: gunicorn app:app\nworker: celery worker\n").unwrap();
        let cmds = detect_commands(&dir);
        let web = cmds.iter().find(|c| c.label.starts_with("web:")).unwrap();
        assert!(web.recommended);
        assert!(cmds.iter().any(|c| c.label.starts_with("worker:")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_empty_dir_returns_only_custom_sentinel() {
        let dir = tempdir();
        let cmds = detect_commands(&dir);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].command.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    // ── helper ───────────────────────────────────────────────────────────────

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rundev_test_{}", rand_suffix()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64
    }
}
