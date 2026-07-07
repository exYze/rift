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
    /// Default reasoning effort (minimal|low|medium|high|xhigh|max) for
    /// thinking models. Overridden by `--effort` / `/think <level>`.
    #[serde(default)]
    pub effort: Option<String>,
    /// Optional named model roles for multi-model workflows, e.g.
    /// {"smart": "vllm/deepseek-ai/DeepSeek-V4-Flash", "fast": "ornith:35b"}.
    /// The agent tool accepts a role (or full model string) per delegated
    /// task, so one session can plan/review on one model and implement on
    /// another. Unset = single-model behavior, exactly as before.
    #[serde(default)]
    pub models: HashMap<String, String>,
    /// Default max agent-loop iterations per turn. Overridden by `--max-iterations`.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// TUI color theme name ("dark", "light", "mono"); runtime `/theme` overrides.
    #[serde(default)]
    pub theme: Option<String>,
    /// Named cloud providers. Address a model as `<name>/<model>` (e.g.
    /// `openrouter/qwen3`, `anthropic/claude-opus-4-8`) to route it through
    /// one of these. `kind` selects the wire protocol ("openai" default,
    /// "anthropic" for the native Messages API). The names `anthropic` and
    /// `openai` have built-in defaults that need only the ANTHROPIC_API_KEY /
    /// OPENAI_API_KEY env var — no config entry required.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Cost display rates ($ per million tokens) keyed by a model-name
    /// substring, for metered providers rift has no built-in rates for:
    /// `"pricing": {"gpt-5": {"input": 1.25, "output": 10.0}}`. Display
    /// only — never affects requests.
    #[serde(default)]
    pub pricing: HashMap<String, Pricing>,
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

/// A cloud/remote endpoint rift can route models to.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// API root, e.g. `https://openrouter.ai/api/v1` (a `/v1` is appended if absent).
    pub base_url: String,
    /// Wire protocol: `"openai"` (default — OpenAI-compatible chat
    /// completions) or `"anthropic"` (native Anthropic Messages API).
    #[serde(default)]
    pub kind: Option<String>,
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

/// USD per million tokens, for the cost display.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Permissions {
    /// Extra glob patterns for shell commands to refuse (merged with the
    /// built-in deny list).
    #[serde(default)]
    pub bash_deny: Vec<String>,
    /// Glob patterns for shell commands pre-approved to run WITHOUT an
    /// approval prompt (e.g. "git status", "cargo *"). Grown by the
    /// "always allow" choice on approval prompts. USER config only — a
    /// project .rift.json can never loosen permissions.
    #[serde(default)]
    pub bash_allow: Vec<String>,
    /// Pause for user approval before write/edit/bash (TUI sessions only).
    /// Unset = ON: interactive sessions ask by default (the Claude Code
    /// model); `"approve": false` in the user config or /yolo turns it off.
    #[serde(default)]
    pub approve: Option<bool>,
}

impl Permissions {
    /// The effective approval default: ask unless the user opted out.
    pub fn approve_effective(&self) -> bool {
        self.approve.unwrap_or(true)
    }
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
        if p.effort.is_some() {
            self.effort = p.effort;
        }
        // Roles are plain model names resolved through the (redefinition-
        // guarded) provider table, so a project adding/overriding them is
        // no more powerful than it setting `model`.
        self.models.extend(p.models);
        if p.temperature.is_some() {
            self.temperature = p.temperature;
        }
        if p.max_iterations.is_some() {
            self.max_iterations = p.max_iterations;
        }
        if p.theme.is_some() {
            self.theme = p.theme;
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
        // Pricing entries are display-only: a project may add rates for its
        // models but never redefine the user's.
        for (name, rates) in p.pricing {
            self.pricing.entry(name).or_insert(rates);
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
        // approve can only tighten: a project may force prompts ON, never off.
        if p.permissions.approve == Some(true) {
            self.permissions.approve = Some(true);
        }
        if !p.permissions.bash_allow.is_empty() {
            warnings.push(
                "project .rift.json 'bash_allow' ignored — allow patterns load from the user config only \
                 (a cloned repo must not be able to pre-approve commands)"
                    .into(),
            );
        }
    }
}

/// Persist an "always allow" bash pattern to the USER config
/// (`~/.config/rift/config.json`) — the only file allow patterns load from.
/// Merge-preserving: every other key in the file is left untouched.
pub fn append_user_bash_allow(pattern: &str) -> Result<std::path::PathBuf> {
    let path = dirs_config().join("rift/config.json");
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    let obj = root.as_object_mut().context("user config is not a JSON object")?;
    let perms = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("config 'permissions' is not a JSON object")?;
    let allow = perms
        .entry("bash_allow")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("config 'permissions.bash_allow' is not an array")?;
    if !allow.iter().any(|v| v.as_str() == Some(pattern)) {
        allow.push(serde_json::Value::String(pattern.to_string()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
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
        assert_eq!(cfg.permissions.approve, Some(true));
        // Unset approve = ask by default; an explicit false opts out.
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.permissions.approve.is_none() && cfg.permissions.approve_effective());
        let cfg: Config = serde_json::from_str(r#"{"permissions": {"approve": false}}"#).unwrap();
        assert!(!cfg.permissions.approve_effective());
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
                "permissions": {"bash_deny": ["terraform *"], "approve": false, "bash_allow": ["curl *"]}
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
        assert_eq!(warnings.len(), 3); // provider redef, mcp redef, project bash_allow
        // Permissions only tighten: deny union, approve stays ON, and the
        // project's allow patterns are refused.
        assert!(user.permissions.bash_deny.iter().any(|d| d == "docker push *"));
        assert!(user.permissions.bash_deny.iter().any(|d| d == "terraform *"));
        assert_eq!(user.permissions.approve, Some(true));
        assert!(user.permissions.bash_allow.is_empty());
    }
}
