//! Configuration: `.rift.json` in the working directory (project
//! config) falling back to `~/.config/rift/config.json` (user config).
//!
//! ```json
//! {
//!   "mcp": {
//!     "fetch": {"command": "uvx", "args": ["mcp-server-fetch"], "env": {}}
//!   },
//!   "permissions": {"bash_deny": ["docker push *"]}
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::mcp::McpServerConfig;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
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
