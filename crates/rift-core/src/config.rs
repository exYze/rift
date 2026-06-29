//! Configuration: `.rift.json` in the working directory (project
//! config) falling back to `~/.config/rift/config.json` (user config).
//!
//! ```json
//! {
//!   "host": "http://localhost:11434",
//!   "model": "gemma4:26b",
//!   "mcp": {
//!     "fetch": {"command": "uvx", "args": ["mcp-server-fetch"], "env": {}}
//!   },
//!   "permissions": {"bash_deny": ["docker push *"]}
//! }
//! ```
//!
//! `host` and `model` are startup defaults so `rift` can run with no flags; a
//! `--host`/`--model` flag or `RIFT_HOST`/`RIFT_MODEL` env var still overrides
//! them.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::mcp::McpServerConfig;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Default Ollama server URL. Overridden by `--host` / `RIFT_HOST`.
    #[serde(default)]
    pub host: Option<String>,
    /// Default model (must have the "tools" capability). Overridden by
    /// `--model` / `RIFT_MODEL`.
    #[serde(default)]
    pub model: Option<String>,
    /// Default context window to request (options.num_ctx). Overridden by `--num-ctx`.
    #[serde(default)]
    pub num_ctx: Option<u64>,
    /// Default sampling temperature. Overridden by `--temp`.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Default max agent-loop iterations per turn. Overridden by `--max-iterations`.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Default, Deserialize)]
pub struct Permissions {
    /// Extra glob patterns for shell commands to refuse (merged with the
    /// built-in deny list).
    #[serde(default)]
    pub bash_deny: Vec<String>,
    /// Pause for user approval before write/edit/bash (TUI sessions only).
    #[serde(default)]
    pub approve: bool,
}

impl Config {
    /// Project config wins over user config; absence of both is fine.
    pub fn load(cwd: &Path) -> Result<(Self, Option<std::path::PathBuf>)> {
        let candidates = [
            cwd.join(".rift.json"),
            dirs_config().join("rift/config.json"),
        ];
        for path in candidates {
            if path.exists() {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let cfg: Config = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                return Ok((cfg, Some(path)));
            }
        }
        Ok((Config::default(), None))
    }
}

fn dirs_config() -> std::path::PathBuf {
    // Falls back to a CWD-relative `.config` only if no home dir can be found
    // at all (no HOME, no USERPROFILE) — practically never.
    crate::paths::config_dir().unwrap_or_else(|| std::path::PathBuf::from(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_and_model() {
        let cfg: Config =
            serde_json::from_str(r#"{"host": "http://box:11434", "model": "qwen3"}"#).unwrap();
        assert_eq!(cfg.host.as_deref(), Some("http://box:11434"));
        assert_eq!(cfg.model.as_deref(), Some("qwen3"));
    }

    #[test]
    fn missing_fields_default_to_none() {
        // An empty object, or one with only other keys, must still parse — the
        // resolver treats absent host/model as "fall through to the default".
        let cfg: Config = serde_json::from_str(r#"{"permissions": {"approve": true}}"#).unwrap();
        assert!(cfg.host.is_none() && cfg.model.is_none());
        assert!(cfg.permissions.approve);
    }
}
