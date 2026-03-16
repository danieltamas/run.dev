//! Project and service configuration — loading, saving, and directory layout.
//!
//! Each project is stored as a YAML file in `~/.config/rundev/projects/<name>.yaml`.
//! A global config at `~/.config/rundev/config.yaml` holds the Anthropic API key
//! and any other app-wide settings.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub path: String,
    pub command: String,
    pub port: u16,
    #[serde(default)]
    pub subdomain: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Node.js version to use via nvm (e.g. "22.9", "20", "lts").
    /// The command is wrapped with `. "$NVM_DIR/nvm.sh" && nvm use <version>`.
    #[serde(default)]
    pub node_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub domain: String,
    pub services: HashMap<String, ServiceConfig>,
    #[serde(skip)]
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub claude_proxy: Option<String>,
    #[serde(default)]
    pub premium: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "default".to_string()
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            claude_proxy: Some("http://localhost:3456/v1".to_string()),
            premium: false,
            theme: "default".to_string(),
        }
    }
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let mut config: ProjectConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        config.config_path = path.to_path_buf();
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let content = serde_yaml::to_string(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn all_domains(&self) -> Vec<String> {
        let mut domains = vec![self.domain.clone()];
        for (svc_name, svc) in &self.services {
            let d = if !svc.subdomain.is_empty() {
                resolve_domain(&svc.subdomain, &self.domain)
            } else if svc_name.contains('.') {
                // Service name is itself a full domain (e.g. "win.wam.app")
                svc_name.clone()
            } else {
                continue; // resolves to project domain, already in list
            };
            domains.push(d);
        }
        domains.dedup();
        domains
    }
}

/// Build the full domain for a service. If `subdomain` already contains a dot
/// it is treated as a fully-qualified domain and returned as-is; otherwise it
/// is prepended to `project_domain`.
pub fn resolve_domain(subdomain: &str, project_domain: &str) -> String {
    if subdomain.is_empty() {
        project_domain.to_string()
    } else if subdomain.contains('.') {
        subdomain.to_string()
    } else {
        format!("{}.{}", subdomain, project_domain)
    }
}

pub fn load_global_config() -> GlobalConfig {
    let path = global_config_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_yaml::from_str(&c).ok())
            .unwrap_or_default()
    } else {
        GlobalConfig::default()
    }
}

pub fn global_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rundev")
        .join("config.yaml")
}

pub fn projects_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rundev")
        .join("projects")
}

pub fn load_all_projects() -> Vec<ProjectConfig> {
    let dir = projects_dir();
    if !dir.exists() {
        return vec![];
    }
    let mut projects = vec![];
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                if let Ok(p) = ProjectConfig::load(&path) {
                    projects.push(p);
                }
            }
        }
    }
    projects
}

pub fn save_project(config: &ProjectConfig) -> Result<()> {
    let dir = projects_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.yaml", config.name));
    config.save(&path)
}

pub fn state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rundev")
        .join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn service(port: u16, subdomain: &str) -> ServiceConfig {
        ServiceConfig {
            path: "/tmp/svc".to_string(),
            command: "npm start".to_string(),
            port,
            subdomain: subdomain.to_string(),
            env: HashMap::new(),
            node_version: None,
        }
    }

    fn project_with_services(domain: &str, svcs: Vec<(&str, ServiceConfig)>) -> ProjectConfig {
        let mut services = HashMap::new();
        for (name, s) in svcs {
            services.insert(name.to_string(), s);
        }
        ProjectConfig {
            name: "test".to_string(),
            domain: domain.to_string(),
            services,
            config_path: PathBuf::new(),
        }
    }

    // ── all_domains ──────────────────────────────────────────────────────────

    #[test]
    fn all_domains_no_subdomains() {
        let p = project_with_services("myapp.local", vec![("api", service(3000, ""))]);
        let domains = p.all_domains();
        assert_eq!(domains, vec!["myapp.local"]);
    }

    #[test]
    fn all_domains_with_subdomain() {
        let p = project_with_services(
            "myapp.local",
            vec![("api", service(3001, "api")), ("web", service(3000, ""))],
        );
        let mut domains = p.all_domains();
        domains.sort();
        assert!(domains.contains(&"myapp.local".to_string()));
        assert!(domains.contains(&"api.myapp.local".to_string()));
    }

    #[test]
    fn all_domains_multiple_subdomains() {
        let p = project_with_services(
            "proj.local",
            vec![
                ("api", service(3001, "api")),
                ("admin", service(3002, "admin")),
                ("web", service(3000, "")),
            ],
        );
        let domains = p.all_domains();
        assert_eq!(domains.len(), 3);
    }

    // ── save / load round-trip ───────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rundev_cfg_test_{}", timestamp()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("myproject.yaml");

        let mut p = project_with_services("proj.local", vec![("api", service(3001, "api"))]);
        p.name = "myproject".to_string();

        p.save(&path).unwrap();

        let loaded = ProjectConfig::load(&path).unwrap();
        assert_eq!(loaded.name, "myproject");
        assert_eq!(loaded.domain, "proj.local");
        assert!(loaded.services.contains_key("api"));
        assert_eq!(loaded.services["api"].port, 3001);
        assert_eq!(loaded.services["api"].subdomain, "api");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_sets_config_path() {
        let dir = std::env::temp_dir().join(format!("rundev_cfg_test_{}", timestamp()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proj.yaml");

        let p = project_with_services("x.local", vec![]);
        p.save(&path).unwrap();

        let loaded = ProjectConfig::load(&path).unwrap();
        assert_eq!(loaded.config_path, path);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_returns_error() {
        let result = ProjectConfig::load(&PathBuf::from("/nonexistent/path.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn service_config_env_defaults_empty() {
        let s = service(3000, "");
        assert!(s.env.is_empty());
    }

    // ── resolve_domain ──────────────────────────────────────────────────────

    #[test]
    fn resolve_domain_empty_subdomain_returns_project() {
        assert_eq!(resolve_domain("", "myapp.local"), "myapp.local");
    }

    #[test]
    fn resolve_domain_simple_subdomain_prepends() {
        assert_eq!(resolve_domain("api", "myapp.local"), "api.myapp.local");
    }

    #[test]
    fn resolve_domain_fqdn_returned_as_is() {
        assert_eq!(resolve_domain("win.wam.app", "wam.local"), "win.wam.app");
    }

    #[test]
    fn resolve_domain_dotted_subdomain_is_fqdn() {
        assert_eq!(resolve_domain("backend.wam.app", "wam.local"), "backend.wam.app");
    }

    // ── all_domains with fqdn service names ─────────────────────────────────

    #[test]
    fn all_domains_includes_fqdn_service_names() {
        let p = project_with_services(
            "wam.local",
            vec![("win.wam.app", service(5111, "win.wam.app"))],
        );
        let domains = p.all_domains();
        assert!(domains.contains(&"win.wam.app".to_string()));
    }

    // ── node_version serialization ──────────────────────────────────────────

    #[test]
    fn node_version_none_by_default() {
        let yaml = "path: /tmp\ncommand: npm start\nport: 3000\nsubdomain: api\n";
        let s: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s.node_version, None);
    }

    #[test]
    fn node_version_deserializes() {
        let yaml = "path: /tmp\ncommand: npm start\nport: 3000\nsubdomain: api\nnode_version: '22.9'\n";
        let s: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s.node_version, Some("22.9".to_string()));
    }

    fn timestamp() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64
    }
}
