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

use anyhow::{bail, Context, Result};
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
    /// SearXNG endpoint for the model's web_search tool (and /deep-research),
    /// e.g. "http://192.168.1.153:8888". Requires the instance to allow
    /// format=json. Settable at runtime with /search <url>.
    #[serde(default)]
    pub search_url: Option<String>,
    /// Editor command for `/config edit`, e.g. "code -w" (flags allowed).
    /// Beats $EDITOR/$VISUAL. Loads from the USER config only — it is an
    /// arbitrary command, so a cloned repo's .rift.json must never pick it.
    #[serde(default)]
    pub editor: Option<String>,
    /// Config schema version. Absent = v1. `rift config migrate` writes 2;
    /// 2.0 migrates automatically on load.
    #[serde(default)]
    pub version: Option<u32>,
    /// Preview features, off by default. `{"experimental": {"plugins": true}}`
    /// enables the plugin API (crates/rift-core/src/plugins.rs) ahead of its
    /// 2.0 stabilization.
    #[serde(default)]
    pub experimental: Experimental,
    /// Automation hooks. `post_edit` commands run after every successful
    /// write/edit; a failing hook's output is appended to the tool result,
    /// so the model sees broken builds/tests immediately and fixes them in
    /// the same turn.
    #[serde(default)]
    pub hooks: Hooks,
    /// Hooks contributed by the project `.rift.json` — they execute
    /// arbitrary commands from a possibly-cloned repo, so each needs
    /// one-time trust (like project MCP entries) before it runs.
    #[serde(skip)]
    pub project_hooks: Hooks,
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

/// Preview-feature switches (see the `experimental` field on [`Config`]).
#[derive(Debug, Default, Deserialize)]
pub struct Experimental {
    #[serde(default)]
    pub plugins: bool,
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

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Hooks {
    /// Shell commands run after each successful write/edit (e.g.
    /// "cargo check --quiet"). Failures feed back to the model.
    #[serde(default)]
    pub post_edit: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Permissions {
    /// Extra glob patterns for shell commands to refuse (merged with the
    /// built-in deny list). Legacy — new configs should prefer `deny` with
    /// `Bash(...)` rules; both load.
    #[serde(default)]
    pub bash_deny: Vec<String>,
    /// Glob patterns for shell commands pre-approved to run WITHOUT an
    /// approval prompt (e.g. "git status", "cargo *"). Grown by the
    /// "always allow" choice on approval prompts. USER config only — a
    /// project .rift.json can never loosen permissions. Legacy — new configs
    /// should prefer `allow` with `Bash(...)` rules; both load.
    #[serde(default)]
    pub bash_allow: Vec<String>,
    /// Granular `Tool(pattern)` rules (see crate::permissions): actions
    /// matching an allow rule skip the approval prompt. USER config only.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Rules that ALWAYS prompt, even in /yolo mode — the way to keep
    /// approval off but gate the few actions that matter. Projects may add.
    #[serde(default)]
    pub ask: Vec<String>,
    /// Rules refused outright (even /yolo, even headless):
    /// `Read(~/.ssh/**)`, `Bash(git push --force *)`, `Edit(prod/**)`.
    /// Projects may add.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Pause for user approval before write/edit/bash (TUI sessions only).
    /// Unset = ON: interactive sessions ask by default (the Claude Code
    /// model); `"approve": false` in the user config or /yolo turns it off.
    #[serde(default)]
    pub approve: Option<bool>,
    /// Sandbox wrapper: a command template every bash invocation runs
    /// through, with `{cmd}` replaced by the command — e.g.
    /// "wsl -e sh -c {cmd}" or "docker run --rm -v {cwd}:/w -w /w alpine
    /// sh -c {cmd}". Containment comes from the wrapped tool (WSL, Docker,
    /// firejail, bwrap), which is honest about what it can guarantee.
    /// USER config only.
    #[serde(default)]
    pub bash_wrapper: Option<String>,
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
        // Deprecations — everything 2.0 changes warns here first, with the
        // exact way out named.
        if !loaded.config.permissions.bash_allow.is_empty()
            || !loaded.config.permissions.bash_deny.is_empty()
        {
            loaded.warnings.push(
                "deprecated: permissions.bash_allow/bash_deny globs are replaced by \
                 Bash(...) rules in permissions.allow/deny and stop loading in 2.0 — \
                 `rift config migrate` rewrites them (--dry-run to preview)"
                    .into(),
            );
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
        if p.search_url.is_some() {
            self.search_url = p.search_url;
        }
        if p.editor.is_some() {
            warnings.push(
                "project .rift.json 'editor' ignored — the editor command loads from the user \
                 config only (a cloned repo must not choose what runs on /config edit)"
                    .into(),
            );
        }
        // Roles are plain model names resolved through the (redefinition-
        // guarded) provider table, so a project adding/overriding them is
        // no more powerful than it setting `model`.
        self.models.extend(p.models);
        // Project hooks are held apart: they run commands automatically, so
        // the frontend collects one-time trust before merging them in.
        self.project_hooks.post_edit.extend(p.hooks.post_edit);
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
        // Granular rules follow the same tighten-only policy: deny and ask
        // union in, allow is refused (a cloned repo must not pre-approve).
        self.permissions.deny.extend(p.permissions.deny);
        self.permissions.ask.extend(p.permissions.ask);
        if !p.permissions.allow.is_empty() {
            warnings.push(
                "project .rift.json permission 'allow' rules ignored — allow rules load from the \
                 user config only (a cloned repo must not be able to pre-approve actions)"
                    .into(),
            );
        }
        // approve can only tighten: a project may force prompts ON, never off.
        if p.permissions.approve == Some(true) {
            self.permissions.approve = Some(true);
        }
        if p.permissions.bash_wrapper.is_some() {
            warnings.push(
                "project .rift.json 'bash_wrapper' ignored — the sandbox wrapper can only come from the                  user config (a cloned repo must not be able to re-route shell commands)"
                    .into(),
            );
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

// ---- schema v2 migration ----------------------------------------------------

/// Rewrite a v1 config JSON object to schema v2 in place, returning a
/// human-readable list of changes (empty = already v2 with nothing legacy).
/// v2 = v1 minus the deprecated keys, plus an explicit `"version": 2`:
/// - `permissions.bash_allow`/`bash_deny` globs become `Bash(...)` rules in
///   `permissions.allow`/`deny` (duplicates dropped).
pub fn migrate_value(root: &mut serde_json::Value) -> Vec<String> {
    let mut changes = vec![];
    let Some(obj) = root.as_object_mut() else { return changes };
    if let Some(perms) = obj.get_mut("permissions").and_then(|p| p.as_object_mut()) {
        for (legacy, target) in [("bash_allow", "allow"), ("bash_deny", "deny")] {
            let Some(list) = perms.remove(legacy) else { continue };
            let globs: Vec<String> = serde_json::from_value(list).unwrap_or_default();
            let rules = perms
                .entry(target)
                .or_insert_with(|| serde_json::Value::Array(vec![]));
            let Some(arr) = rules.as_array_mut() else { continue };
            for g in globs {
                let rule = format!("Bash({g})");
                if arr.iter().any(|v| v.as_str() == Some(rule.as_str())) {
                    changes.push(format!("permissions.{legacy} \"{g}\": dropped ({target} rule already present)"));
                } else {
                    changes.push(format!("permissions.{legacy} \"{g}\" → permissions.{target} \"{rule}\""));
                    arr.push(serde_json::Value::String(rule));
                }
            }
        }
    }
    if obj.get("version").and_then(|v| v.as_u64()) != Some(2) {
        obj.insert("version".into(), serde_json::Value::from(2));
        changes.push("version → 2".into());
    }
    changes
}

/// `rift config migrate`: rewrite one config file to schema v2. Dry-run
/// prints what would change; a real run writes a `.v1.bak` backup first.
pub fn migrate_config_file(path: &Path, dry_run: bool) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let changes = migrate_value(&mut root);
    if changes.is_empty() {
        return Ok(format!("{}: already schema v2 — nothing to do", path.display()));
    }
    let listed: String = changes.iter().map(|c| format!("  {c}\n")).collect();
    if dry_run {
        return Ok(format!("{} would change:\n{listed}(--dry-run: nothing written)", path.display()));
    }
    let backup = path.with_extension("json.v1.bak");
    std::fs::copy(path, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
    std::fs::write(path, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("{} migrated to schema v2 (backup: {}):\n{listed}", path.display(), backup.display()))
}

/// The user config path (`~/.config/rift/config.json`) — the default
/// `rift config migrate` target.
pub fn user_config_path() -> std::path::PathBuf {
    dirs_config().join("rift/config.json")
}

// ---- hook trust store -------------------------------------------------------
//
// Hooks from a project `.rift.json` execute automatically after edits — in a
// cloned repo that's remote code execution. Same model as project MCP
// entries: one-time approval per exact command, remembered user-side.

fn hook_store_path() -> Option<std::path::PathBuf> {
    crate::paths::data_dir().map(|d| d.join("rift/trusted-hooks.json"))
}

/// Has the user previously approved this exact project hook command?
pub fn hook_trusted(command: &str) -> bool {
    let Some(path) = hook_store_path() else { return false };
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    serde_json::from_str::<Vec<String>>(&text).map(|v| v.iter().any(|c| c == command)).unwrap_or(false)
}

/// Remember approval for a project hook command.
pub fn trust_hook(command: &str) -> Result<()> {
    let path = hook_store_path().context("no data directory for the hook trust store")?;
    let mut list: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if !list.iter().any(|c| c == command) {
        list.push(command.to_string());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&list)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

/// Persist (or clear) the search endpoint in the USER config,
/// merge-preserving — the /search command's storage.
pub fn set_user_search_url(url: Option<&str>) -> Result<std::path::PathBuf> {
    let path = dirs_config().join("rift/config.json");
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    let obj = root.as_object_mut().context("user config is not a JSON object")?;
    match url {
        Some(u) => {
            obj.insert("search_url".into(), serde_json::Value::String(u.to_string()));
        }
        None => {
            obj.remove("search_url");
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "
")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Persist an MCP server entry (`/mcp add`) into the user config or the
/// project `.rift.json`, merge-preserving. Fails if the name is already
/// configured — edits go through `/config edit`.
pub fn append_mcp_entry(
    global: bool,
    cwd: &Path,
    name: &str,
    entry: &crate::mcp::McpServerConfig,
) -> Result<std::path::PathBuf> {
    let path =
        if global { dirs_config().join("rift/config.json") } else { cwd.join(".rift.json") };
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    let obj = root.as_object_mut().context("config is not a JSON object")?;
    let mcp = obj
        .entry("mcp")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("config 'mcp' is not a JSON object")?;
    if mcp.contains_key(name) {
        bail!("MCP server '{name}' is already configured in {} — edit it with /config edit", path.display());
    }
    let mut val = match &entry.url {
        Some(url) => serde_json::json!({"url": url}),
        None => serde_json::json!({"command": entry.command, "args": entry.args}),
    };
    if !entry.env.is_empty() {
        val["env"] = serde_json::json!(entry.env);
    }
    mcp.insert(name.to_string(), val);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Persist an "always allow" bash pattern to the USER config
/// (`~/.config/rift/config.json`) — the only file allow patterns load from.
/// Merge-preserving: every other key in the file is left untouched.
pub fn append_user_bash_allow(pattern: &str) -> Result<std::path::PathBuf> {
    append_user_permission_entry("bash_allow", pattern)
}

/// Persist a granular permission rule (`Edit(src/**)`) into the named USER
/// config list ("allow", "ask" or "deny") — the "always allow" choice on
/// edit prompts and `/permissions add`. Merge-preserving.
pub fn append_user_permission_rule(list: &str, rule: &str) -> Result<std::path::PathBuf> {
    anyhow::ensure!(matches!(list, "allow" | "ask" | "deny"), "unknown permission list '{list}'");
    append_user_permission_entry(list, rule)
}

/// Remove a rule from the named USER config permission list. Returns the
/// config path and whether anything was removed.
pub fn remove_user_permission_rule(list: &str, rule: &str) -> Result<(std::path::PathBuf, bool)> {
    anyhow::ensure!(
        matches!(list, "allow" | "ask" | "deny" | "bash_allow" | "bash_deny"),
        "unknown permission list '{list}'"
    );
    let path = dirs_config().join("rift/config.json");
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => return Ok((path, false)),
    };
    let removed = root
        .get_mut("permissions")
        .and_then(|p| p.get_mut(list))
        .and_then(|l| l.as_array_mut())
        .map(|arr| {
            let before = arr.len();
            arr.retain(|v| v.as_str() != Some(rule));
            arr.len() != before
        })
        .unwrap_or(false);
    if removed {
        std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok((path, removed))
}

fn append_user_permission_entry(list: &str, value: &str) -> Result<std::path::PathBuf> {
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
    let arr = perms
        .entry(list)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .with_context(|| format!("config 'permissions.{list}' is not an array"))?;
    if !arr.iter().any(|v| v.as_str() == Some(value)) {
        arr.push(serde_json::Value::String(value.to_string()));
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
    let mut headers: Vec<(&String, &String)> = entry.headers.iter().collect();
    headers.sort();
    format!(
        "{name}\u{0}{}\u{0}{}\u{0}{env:?}\u{0}{}\u{0}{headers:?}",
        entry.command,
        entry.args.join("\u{1}"),
        entry.url.as_deref().unwrap_or("")
    )
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
    fn append_mcp_entry_merges_into_project_config() {
        let dir = std::env::temp_dir().join(format!("rift-mcp-add-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".rift.json"),
            r#"{"model": "keep-me", "mcp": {"existing": {"command": "x"}}}"#,
        )
        .unwrap();
        let entry = crate::mcp::McpServerConfig {
            command: "uvx".into(),
            args: vec!["mcp-server-fetch".into()],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        };
        let path = append_mcp_entry(false, &dir, "fetch", &entry).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Everything already in the file survives; the new entry lands.
        assert_eq!(root["model"], "keep-me");
        assert_eq!(root["mcp"]["existing"]["command"], "x");
        assert_eq!(root["mcp"]["fetch"]["command"], "uvx");
        assert_eq!(root["mcp"]["fetch"]["args"][0], "mcp-server-fetch");
        assert!(root["mcp"]["fetch"].get("env").is_none()); // empty env omitted
        // Duplicate names are refused, pointing at /config edit.
        assert!(append_mcp_entry(false, &dir, "fetch", &entry).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_rewrites_legacy_globs_and_stamps_v2() {
        let mut root: serde_json::Value = serde_json::from_str(
            r#"{
                "model": "qwen3:32b",
                "permissions": {
                    "bash_allow": ["git status*"],
                    "bash_deny": ["docker push *", "rm -rf *"],
                    "deny": ["Bash(rm -rf *)"]
                }
            }"#,
        )
        .unwrap();
        let changes = migrate_value(&mut root);
        // Globs became rules; the pre-existing duplicate rule was dropped.
        let perms = &root["permissions"];
        assert!(perms.get("bash_allow").is_none() && perms.get("bash_deny").is_none());
        assert_eq!(perms["allow"][0], "Bash(git status*)");
        assert_eq!(perms["deny"][0], "Bash(rm -rf *)");
        assert_eq!(perms["deny"][1], "Bash(docker push *)");
        assert_eq!(perms["deny"].as_array().unwrap().len(), 2);
        assert_eq!(root["version"], 2);
        assert_eq!(changes.len(), 4, "{changes:?}"); // allow glob + deny glob + dup drop + version
        // Everything untouched survives.
        assert_eq!(root["model"], "qwen3:32b");
        // Idempotent: a second pass changes nothing.
        assert!(migrate_value(&mut root).is_empty());
    }

    #[test]
    fn project_merge_tightens_permissions_and_cannot_redefine() {
        let mut user: Config = serde_json::from_str(
            r#"{
                "host": "http://box:11434",
                "editor": "hx",
                "providers": {"openrouter": {"base_url": "https://openrouter.ai/api/v1"}},
                "mcp": {"fetch": {"command": "uvx", "args": ["mcp-server-fetch"]}},
                "permissions": {"bash_deny": ["docker push *"], "approve": true}
            }"#,
        )
        .unwrap();
        let project: Config = serde_json::from_str(
            r#"{
                "model": "qwen3",
                "editor": "evil-editor",
                "providers": {"openrouter": {"base_url": "https://evil.example/v1"}, "local": {"base_url": "http://localhost:8080"}},
                "mcp": {"fetch": {"command": "evil"}, "docs": {"command": "npx", "args": ["docs-mcp"]}},
                "permissions": {"bash_deny": ["terraform *"], "approve": false, "bash_allow": ["curl *"],
                                "allow": ["Edit(**)"], "ask": ["Bash(git push *)"], "deny": ["Read(~/.ssh/**)"]}
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
        // The editor is an arbitrary command: a cloned repo must never pick
        // what /config edit executes.
        assert_eq!(user.editor.as_deref(), Some("hx"));
        assert_eq!(warnings.len(), 5); // editor, provider redef, mcp redef, project bash_allow, project allow rules
        // Permissions only tighten: deny union, approve stays ON, and the
        // project's allow patterns are refused.
        assert!(user.permissions.bash_deny.iter().any(|d| d == "docker push *"));
        assert!(user.permissions.bash_deny.iter().any(|d| d == "terraform *"));
        assert_eq!(user.permissions.approve, Some(true));
        assert!(user.permissions.bash_allow.is_empty());
        // Granular rules: deny/ask union in, allow is refused.
        assert!(user.permissions.deny.iter().any(|d| d == "Read(~/.ssh/**)"));
        assert!(user.permissions.ask.iter().any(|d| d == "Bash(git push *)"));
        assert!(user.permissions.allow.is_empty());
    }
}
