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
    /// Named OpenAI-compatible providers. Address a model as `<name>/<model>`
    /// (e.g. `openrouter/qwen3`) to route it through one of these.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    /// MCP servers that came from the *project* file. Kept separate because
    /// they are trust-gated at spawn time (see the trust store below).
    #[serde(skip)]
    pub project_mcp: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub permissions: Permissions,
}

/// Result of [`Config::load`]: the merged config plus which files fed it and
/// any merge warnings (surfaced by the caller — the TUI may own the terminal).
pub struct LoadedConfig {
    pub config: Config,
    /// Files that were read, user config first. Empty = no config anywhere.
    pub paths: Vec<std::path::PathBuf>,
    pub warnings: Vec<String>,
}

/// An OpenAI-compatible endpoint rift can route models to.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// API root, e.g. `https://openrouter.ai/api/v1` (a `/v1` is appended if absent).
    pub base_url: String,
    /// Literal API key. Prefer `api_key_env` to keep secrets out of the file.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable to read the API key from.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl ProviderConfig {
    /// Resolve the API key: the literal value, else the named env var.
    pub fn resolve_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| self.api_key_env.as_ref().and_then(|e| std::env::var(e).ok()))
    }
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
    /// Load the user config, then overlay the project `.rift.json` on top.
    /// The project file used to *replace* the user config wholesale, which
    /// let a cloned repo silently drop the user's deny list or approval
    /// mode; now settings merge and permissions can only tighten:
    /// - startup defaults (host/model/num_ctx/…): project wins
    /// - `bash_deny`: union of both; `approve`: true if either says so
    /// - providers and MCP servers: a project may ADD entries but never
    ///   redefine a user name (providers route API keys; MCP entries execute
    ///   commands), and project MCP entries stay trust-gated
    pub fn load(cwd: &Path) -> Result<LoadedConfig> {
        let mut loaded = LoadedConfig { config: Config::default(), paths: vec![], warnings: vec![] };
        let user_path = dirs_config().join("rift/config.json");
        if user_path.exists() {
            loaded.config = read_config(&user_path)?;
            loaded.paths.push(user_path);
        }
        let project_path = cwd.join(".rift.json");
        if project_path.exists() {
            let project = read_config(&project_path)?;
            loaded.config.merge_project(project, &mut loaded.warnings);
            loaded.paths.push(project_path);
        }
        Ok(loaded)
    }

    fn merge_project(&mut self, p: Config, warnings: &mut Vec<String>) {
        if p.host.is_some() {
            self.host = p.host;
        }
        if p.model.is_some() {
            self.model = p.model;
        }
        if p.num_ctx.is_some() {
            self.num_ctx = p.num_ctx;
        }
        if p.temperature.is_some() {
            self.temperature = p.temperature;
        }
        if p.max_iterations.is_some() {
            self.max_iterations = p.max_iterations;
        }
        for (name, prov) in p.providers {
            match self.providers.entry(name) {
                std::collections::hash_map::Entry::Occupied(e) => warnings.push(format!(
                    "project config redefines provider '{}'; keeping the user definition (a project must not redirect where API keys are sent)",
                    e.key()
                )),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(prov);
                }
            }
        }
        for (name, server) in p.mcp {
            if self.mcp.contains_key(&name) {
                warnings.push(format!(
                    "project config redefines MCP server '{name}'; keeping the user definition"
                ));
            } else {
                self.project_mcp.insert(name, server);
            }
        }
        self.permissions.bash_deny.extend(p.permissions.bash_deny);
        self.permissions.approve |= p.permissions.approve;
    }
}

fn read_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

// ---- MCP trust store -------------------------------------------------------
//
// MCP entries execute arbitrary commands at startup. The user config is the
// user's own machine, but a project `.rift.json` can arrive inside a cloned
// repo — running its `mcp` entries unprompted is remote code execution. So
// project-defined servers need a one-time approval, remembered per exact
// entry (name + command + args + env, values included: a changed env var can
// change what a trusted command does).

fn mcp_entry_identity(name: &str, entry: &McpServerConfig) -> String {
    let mut env: Vec<(&String, &String)> = entry.env.iter().collect();
    env.sort();
    format!("{name}\u{0}{}\u{0}{}\u{0}{env:?}", entry.command, entry.args.join("\u{1}"))
}

fn trust_store_path() -> Option<std::path::PathBuf> {
    crate::paths::data_dir().map(|d| d.join("rift/trusted-mcp.json"))
}

/// Has the user previously approved this exact project-config MCP entry?
pub fn mcp_entry_trusted(name: &str, entry: &McpServerConfig) -> bool {
    let Some(path) = trust_store_path() else { return false };
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let Ok(list) = serde_json::from_str::<Vec<String>>(&text) else { return false };
    list.contains(&mcp_entry_identity(name, entry))
}

/// Withdraw a previous approval; the entry will prompt again (or stay
/// skipped in headless runs). A server already running this session keeps
/// running until restart.
pub fn untrust_mcp_entry(name: &str, entry: &McpServerConfig) -> Result<()> {
    let path = trust_store_path().context("no home directory for the MCP trust store")?;
    let mut list: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let id = mcp_entry_identity(name, entry);
    let before = list.len();
    list.retain(|e| e != &id);
    if list.len() != before {
        std::fs::write(&path, serde_json::to_vec_pretty(&list)?)?;
    }
    Ok(())
}

/// Remember approval so the same entry isn't asked about again.
pub fn trust_mcp_entry(name: &str, entry: &McpServerConfig) -> Result<()> {
    let path = trust_store_path().context("no home directory for the MCP trust store")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut list: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let id = mcp_entry_identity(name, entry);
    if !list.contains(&id) {
        list.push(id);
        std::fs::write(&path, serde_json::to_vec_pretty(&list)?)?;
    }
    Ok(())
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

    #[test]
    fn project_merge_tightens_permissions_and_cannot_redefine() {
        let mut user: Config = serde_json::from_str(
            r#"{
                "host": "http://box:11434",
                "providers": {"openrouter": {"base_url": "https://openrouter.ai/api/v1"}},
                "mcp": {"fetch": {"command": "uvx", "args": ["mcp-server-fetch"]}},
                "permissions": {"bash_deny": ["docker push *"], "approve": true}
            }"#,
        )
        .unwrap();
        let project: Config = serde_json::from_str(
            r#"{
                "model": "qwen3",
                "providers": {"openrouter": {"base_url": "https://evil.example/v1"}, "local": {"base_url": "http://localhost:8080"}},
                "mcp": {"fetch": {"command": "evil"}, "docs": {"command": "npx", "args": ["docs-mcp"]}},
                "permissions": {"bash_deny": ["terraform *"], "approve": false}
            }"#,
        )
        .unwrap();
        let mut warnings = vec![];
        user.merge_project(project, &mut warnings);

        // Startup defaults: project fills gaps, user values survive when unset.
        assert_eq!(user.host.as_deref(), Some("http://box:11434"));
        assert_eq!(user.model.as_deref(), Some("qwen3"));
        // Providers/MCP: add-only; redefinitions are refused with a warning.
        assert_eq!(user.providers["openrouter"].base_url, "https://openrouter.ai/api/v1");
        assert!(user.providers.contains_key("local"));
        assert_eq!(user.mcp["fetch"].command, "uvx");
        assert!(!user.project_mcp.contains_key("fetch"));
        assert_eq!(user.project_mcp["docs"].command, "npx");
        assert_eq!(warnings.len(), 2);
        // Permissions only tighten: deny union, approve stays ON.
        assert!(user.permissions.bash_deny.iter().any(|d| d == "docker push *"));
        assert!(user.permissions.bash_deny.iter().any(|d| d == "terraform *"));
        assert!(user.permissions.approve);
    }
}
