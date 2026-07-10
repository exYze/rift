//! Experimental plugin API — the 2.0 platform, previewed behind a flag
//! (config `"experimental": {"plugins": true}`).
//!
//! A plugin is a directory containing `plugin.json`:
//!
//! ```json
//! {
//!   "name": "standup",
//!   "description": "team standup helpers",
//!   "commands": [
//!     {"name": "standup", "description": "summarize recent work",
//!      "prompt": "Summarize the git log since yesterday. Focus: {args}"}
//!   ],
//!   "tools": [
//!     {"name": "ticket_lookup", "description": "Look up a ticket by id",
//!      "command": "python3 lookup.py",
//!      "parameters": {"type": "object", "properties": {"id": {"type": "string"}},
//!                     "required": ["id"]}}
//!   ]
//! }
//! ```
//!
//! Discovery: `.rift/plugins/<dir>/plugin.json` in the project, then
//! `~/.config/rift/plugins/<dir>/plugin.json` — project wins name
//! collisions, mirroring skills.
//!
//! Security model (the preview's deliberate cut):
//! - **Commands** are prompt templates — text sent to the model as a user
//!   turn (`{args}` = whatever followed the command). Inert by themselves,
//!   so project plugins may contribute them freely; they surface exactly
//!   like skills (`/skill:<name>`, listed to the model by description).
//! - **Tools** execute a subprocess (args JSON on stdin, stdout = result,
//!   nonzero exit = error), so a cloned repo must never register one:
//!   tools load from USER plugins only. Project-plugin tools land in 2.0
//!   behind the same one-time trust prompt hooks and project MCP use.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::skills::Skill;
use crate::tools::{Tool, ToolCtx, ToolRegistry};

#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    #[serde(skip)]
    pub source: PathBuf,
    /// True when loaded from the project `.rift/plugins/` (affects what it
    /// may contribute — see the module doc).
    #[serde(skip)]
    pub project: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Prompt template; `{args}` is replaced with the invocation's argument
    /// text (empty string when none).
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Shell command line, run from the working directory with the tool
    /// call's arguments as JSON on stdin.
    pub command: String,
    /// JSON Schema for the arguments object (defaults to an open object).
    #[serde(default)]
    pub parameters: Option<Value>,
}

fn scan_dir(dir: &Path, project: bool, out: &mut Vec<Plugin>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let manifest = path.join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
        match serde_json::from_str::<Plugin>(&text) {
            Ok(mut p) => {
                p.source = manifest;
                p.project = project;
                // First writer wins — project dirs are scanned first.
                if !out.iter().any(|e| e.name == p.name) {
                    out.push(p);
                }
            }
            Err(e) => eprintln!("warning: skipping plugin {}: {e}", manifest.display()),
        }
    }
}

/// All plugins visible from `cwd`: project `.rift/plugins/` first (wins on
/// name collisions), then user `~/.config/rift/plugins/`. Callers gate this
/// on the `experimental.plugins` config flag.
pub fn load_plugins(cwd: &Path) -> Vec<Plugin> {
    let mut out = vec![];
    scan_dir(&cwd.join(".rift/plugins"), true, &mut out);
    if let Some(cfg) = crate::paths::config_dir() {
        scan_dir(&cfg.join("rift/plugins"), false, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Plugin commands as skills: same palette (`/skill:<name>`), same
/// progressive disclosure to the model, zero new dispatch paths.
pub fn commands_as_skills(plugins: &[Plugin]) -> Vec<Skill> {
    let mut out = vec![];
    for p in plugins {
        for c in &p.commands {
            out.push(Skill {
                name: c.name.clone(),
                description: if c.description.is_empty() {
                    format!("(plugin {})", p.name)
                } else {
                    c.description.clone()
                },
                body: c.prompt.clone(),
                source: p.source.clone(),
            });
        }
    }
    out
}

/// Register plugin tools. USER plugins only — a project tool executes a
/// command from a possibly-cloned repo, so it is skipped with a warning
/// (returned for the caller to surface) until 2.0's trust flow.
pub fn register_tools(registry: &mut ToolRegistry, plugins: &[Plugin]) -> Vec<String> {
    let mut warnings = vec![];
    for p in plugins {
        if p.tools.is_empty() {
            continue;
        }
        if p.project {
            warnings.push(format!(
                "plugin '{}': project-plugin tools are not loaded in the experimental preview \
                 (a cloned repo must not register commands to execute) — install it under \
                 ~/.config/rift/plugins/ to use its tools",
                p.name
            ));
            continue;
        }
        for t in &p.tools {
            registry.register(Box::new(PluginTool::new(t.clone(), &p.name)));
        }
    }
    warnings
}

/// A manifest-declared subprocess tool: arguments JSON on stdin, stdout is
/// the result, nonzero exit (or stderr on failure) becomes the error.
struct PluginTool {
    def: PluginToolDef,
    /// "<manifest description> (plugin: <name>)", precomputed because the
    /// Tool trait hands out &str.
    description: String,
}

impl PluginTool {
    fn new(def: PluginToolDef, plugin: &str) -> Self {
        let description = format!("{} (plugin: {plugin})", def.description);
        Self { def, description }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.def.parameters.clone().unwrap_or_else(|| serde_json::json!({"type": "object"}))
    }

    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        #[cfg(windows)]
        let mut cmd = {
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
            let mut c = tokio::process::Command::new(shell);
            c.arg("/C").arg(&self.def.command);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(&self.def.command);
            c
        };
        let mut child = cmd
            .current_dir(&ctx.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning plugin tool '{}'", self.def.name))?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            stdin.write_all(serde_json::to_string(args)?.as_bytes()).await?;
            // Drop closes the pipe — tools that read stdin to EOF finish.
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
            .await
            .map_err(|_| anyhow!("plugin tool '{}' timed out after 60s", self.def.name))??;
        let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = if stderr.trim().is_empty() { stdout.as_str() } else { stderr.trim() };
            bail!("plugin tool '{}' failed ({}): {detail}", self.def.name, out.status);
        }
        Ok(if stdout.is_empty() { "(no output)".into() } else { stdout })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(dir: &Path, name: &str, json: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.json"), json).unwrap();
    }

    #[test]
    fn loads_commands_as_skills_and_gates_project_tools() {
        let root = std::env::temp_dir().join(format!("rift-plugins-test-{}", std::process::id()));
        let proj = root.join("proj/.rift/plugins");
        write_plugin(
            &proj,
            "standup",
            r#"{"name":"standup","description":"d",
                "commands":[{"name":"standup","description":"sum up","prompt":"Summarize: {args}"}],
                "tools":[{"name":"evil","description":"e","command":"echo pwned"}]}"#,
        );

        let mut plugins = vec![];
        scan_dir(&proj, true, &mut plugins);
        assert_eq!(plugins.len(), 1);

        // Commands surface as skills, template intact.
        let skills = commands_as_skills(&plugins);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "standup");
        assert!(skills[0].body.contains("{args}"));

        // Project tools are refused with a pointed warning; nothing lands
        // in the registry.
        let mut reg = ToolRegistry::standard();
        let warnings = register_tools(&mut reg, &plugins);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("project-plugin tools are not loaded"));
        assert!(reg.get("evil").is_none());

        // The same manifest as a USER plugin registers its tool.
        plugins[0].project = false;
        let warnings = register_tools(&mut reg, &plugins);
        assert!(warnings.is_empty());
        assert!(reg.get("evil").is_some());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn plugin_tool_pipes_args_and_captures_output() {
        let tool = PluginTool::new(
            PluginToolDef {
                name: "echoer".into(),
                description: "echo stdin".into(),
                command: "cat".into(),
                parameters: None,
            },
            "test",
        );
        let mut args = Map::new();
        args.insert("id".into(), Value::String("T-1".into()));
        let ctx = ToolCtx::new("/tmp");
        let out = tool.execute(&args, &ctx).await.unwrap();
        assert_eq!(out, "{\"id\":\"T-1\"}");

        // Failure path: nonzero exit surfaces stderr.
        let bad = PluginTool::new(
            PluginToolDef {
                name: "bad".into(),
                description: String::new(),
                command: "echo nope >&2; exit 3".into(),
                parameters: None,
            },
            "test",
        );
        let err = bad.execute(&Map::new(), &ctx).await.unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }
}
