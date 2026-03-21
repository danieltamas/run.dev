//! Node.js preload script management.
//!
//! Writes a small JS file to `~/.config/rundev/node-preload.js` that intercepts
//! `.env` file reads and merges in secrets from process environment. This lets
//! apps using the `dotenv` pattern receive secrets without them ever touching disk.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use std::path::PathBuf;

/// The preload script, embedded at compile time.
const PRELOAD_JS: &str = include_str!("node_preload.js");

/// Ensure the preload script exists on disk and return its path.
/// Overwrites on every call so the script stays in sync with the binary version.
pub fn ensure_node_preload() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rundev");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("node-preload.js");
    let _ = std::fs::write(&path, PRELOAD_JS);
    path
}

/// Returns `true` when the command looks like it will spawn a Node.js runtime.
pub fn is_node_command(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "node" | "npm" | "npx" | "yarn" | "pnpm" | "bun" | "bunx"
            | "next" | "nuxt" | "vite" | "tsx" | "ts-node"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_node_commands() {
        assert!(is_node_command("npm run dev"));
        assert!(is_node_command("node server.js"));
        assert!(is_node_command("yarn dev"));
        assert!(is_node_command("pnpm run start"));
        assert!(is_node_command("bun run dev"));
        assert!(is_node_command("next dev"));
        assert!(is_node_command("nuxt dev"));
        assert!(is_node_command("vite"));
        assert!(is_node_command("tsx watch src/index.ts"));
    }

    #[test]
    fn rejects_non_node_commands() {
        assert!(!is_node_command("cargo run"));
        assert!(!is_node_command("python manage.py runserver"));
        assert!(!is_node_command("go run ."));
        assert!(!is_node_command("bundle exec rails server"));
    }

    #[test]
    fn ensure_preload_creates_file() {
        let path = ensure_node_preload();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("__RUNDEV_ENV"));
    }
}
