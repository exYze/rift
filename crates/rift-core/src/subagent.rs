//! Sub-agents: the `agent` tool lets the model delegate self-contained tasks
//! to child agents that run CONCURRENTLY, each with its own context window,
//! tool set, and no access to the parent conversation. Foreground calls wait
//! and return every child's final report; `background=true` launches them as
//! background tasks that keep working across turns and report back through
//! the task-notification channel (crate::tasks).

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use rift_provider::{Provider, Role};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, AgentEvent};
use crate::tasks::{BgTasks, TaskKind};
use crate::tools::{Tool, ToolCtx, ToolRegistry};

/// Everything needed to construct child agents. The frontend installs it on
/// the root ToolCtx only — child ctxs get None, so delegation stays one level
/// deep — and Agent::run_turn refreshes client/cfg each turn so /model and
/// /host switches carry over (the routing parts are startup-fixed).
#[derive(Clone)]
pub struct SubAgentHandle {
    pub client: Arc<dyn Provider>,
    pub cfg: AgentConfig,
    /// Resolves a model string to a provider client (the swarm's factory).
    /// None = children always run on the session model.
    pub factory: Option<crate::swarm::ProviderFactory>,
    /// Named model roles from config `models` ({"fast": "ornith:35b", …});
    /// a task's `model` can be a role instead of a full model string.
    pub roles: std::collections::HashMap<String, String>,
    /// Custom sub-agent personas (`.rift/agents/*.md` + user-wide); tasks
    /// select one via their `agent` field.
    pub personas: Vec<AgentPersona>,
}

impl SubAgentHandle {
    /// The (client, cfg) a child runs on. No requested model (or the
    /// session model itself) inherits the parent's client and thinking
    /// setup; a different model gets a fresh client via the factory, with
    /// think/effort reset to server defaults — the parent's capability
    /// check doesn't transfer across models.
    fn child_target(&self, requested: Option<&str>) -> Result<(Arc<dyn Provider>, AgentConfig)> {
        let mut cfg = self.cfg.clone();
        cfg.always_task = false;
        let Some(requested) = requested.filter(|m| !m.trim().is_empty()) else {
            return Ok((self.client.clone(), cfg));
        };
        let name = self.roles.get(requested.trim()).map(String::as_str).unwrap_or(requested.trim());
        if name == self.cfg.model {
            return Ok((self.client.clone(), cfg));
        }
        let Some(factory) = &self.factory else {
            bail!("per-task models are not available in this session (no model router configured)");
        };
        let (client, actual) = factory(name)
            .map_err(|e| anyhow!("cannot route model '{requested}' (resolved to '{name}'): {e:#}"))?;
        cfg.model = actual;
        cfg.think = None;
        cfg.effort = None;
        Ok((client, cfg))
    }
}

/// Cap on tasks per call — each one is a full concurrent model conversation.
const SUBAGENT_MAX_TASKS: usize = 4;

/// A custom sub-agent persona (`.rift/agents/<name>.md`, or user-wide in
/// `~/.config/rift/agents/`): its own system-prompt body, optionally a
/// default model (role or full name) and a tool whitelist. The `agent`
/// tool's tasks select one by name.
#[derive(Debug, Clone)]
pub struct AgentPersona {
    pub name: String,
    pub description: String,
    /// Default model for this persona (a role name or full model string);
    /// a task-level `model` still overrides it.
    pub model: Option<String>,
    /// Tool whitelist (names from the standard set); None = all tools.
    pub tools: Option<Vec<String>>,
    pub body: String,
}

/// Parse a persona file: `---` frontmatter with name/description/model/tools
/// (tools comma-separated), then the prompt body.
fn parse_persona(text: &str, fallback_name: &str) -> AgentPersona {
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let mut model = None;
    let mut tools = None;
    let mut body = text;
    if let Some(rest) = text.strip_prefix("---") {
        if let Some((front, after)) = rest.split_once("\n---") {
            for line in front.lines() {
                let Some((key, value)) = line.split_once(':') else { continue };
                let value = value.trim();
                match key.trim() {
                    "name" if !value.is_empty() => name = value.to_string(),
                    "description" => description = value.to_string(),
                    "model" if !value.is_empty() => model = Some(value.to_string()),
                    "tools" if !value.is_empty() => {
                        tools = Some(value.split(',').map(|t| t.trim().to_string()).collect())
                    }
                    _ => {}
                }
            }
            body = after.trim_start_matches(['-']).trim_start();
        }
    }
    AgentPersona { name, description, model, tools, body: body.trim().to_string() }
}

/// Load personas: project `.rift/agents/*.md` first, then user-wide
/// `~/.config/rift/agents/*.md` (project wins name conflicts).
pub fn load_personas(cwd: &std::path::Path) -> Vec<AgentPersona> {
    let mut personas: Vec<AgentPersona> = Vec::new();
    let user_dir = crate::paths::config_dir().map(|d| d.join("rift/agents"));
    let dirs = std::iter::once(cwd.join(".rift/agents")).chain(user_dir);
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("agent").to_string();
            let persona = parse_persona(&text, &stem);
            if !personas.iter().any(|p| p.name == persona.name) {
                personas.push(persona);
            }
        }
    }
    personas.sort_by(|a, b| a.name.cmp(&b.name));
    personas
}

pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "Delegate work to sub-agents. Each task runs as an independent agent with its own context \
         window and the full tool set; ALL tasks in one call run CONCURRENTLY — the way to \
         parallelize independent work (explore three modules at once, run a long test suite while \
         fixing something else). Prompts must be fully self-contained: sub-agents see nothing of \
         this conversation. Each task may set `model` to run on a configured role (e.g. 'fast') \
         or another model — route mechanical work to cheap models, keep judgment on strong ones. \
         By default the call waits and returns every sub-agent's final report; \
         set background=true to launch them asynchronously and keep working — each finished agent \
         then sends a [task notification], and the task tool shows progress/results."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["tasks"],
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "1-4 delegated tasks; all of them run concurrently",
                    "items": {
                        "type": "object",
                        "required": ["description", "prompt"],
                        "properties": {
                            "description": {"type": "string", "description": "3-6 word label shown to the user"},
                            "prompt": {"type": "string", "description": "Complete self-contained instructions — include paths and context; the sub-agent cannot see this conversation"},
                            "model": {"type": "string", "description": "Optional: run this task on a different model — a configured role name (e.g. 'fast', 'smart') or a full model string. Default: the session model. Route mechanical work (write code from a clear spec, run tests, fix straightforward errors) to a cheaper role when one exists; keep judgment work on the stronger model."},
                            "agent": {"type": "string", "description": "Optional: run this task as a configured persona (from .rift/agents/) — a specialized system prompt, default model, and tool set (e.g. a read-only 'reviewer')."}
                        }
                    }
                },
                "background": {
                    "type": "boolean",
                    "description": "true = return immediately and keep the sub-agents running across turns; results arrive as [task notification]s"
                }
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let specs = parse_tasks(args)?;
        let Some(handle) = ctx.subagent_handle() else {
            bail!("the agent tool is not available here (sub-agents cannot spawn further sub-agents)");
        };
        let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
        if background {
            run_background(&handle, ctx, specs)
        } else {
            Ok(run_foreground(&handle, ctx, specs).await)
        }
    }
}

/// Accept the documented `tasks` array, or a lenient single
/// `{description, prompt}` at the top level (weak models do this).
/// One delegated task: label, prompt, and (optionally) which model and
/// which persona run it.
struct TaskSpec {
    label: String,
    prompt: String,
    model: Option<String>,
    agent: Option<String>,
}

fn parse_tasks(args: &Map<String, Value>) -> Result<Vec<TaskSpec>> {
    let items: Vec<Value> = match args.get("tasks").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None if args.contains_key("prompt") => vec![Value::Object(args.clone())],
        None => bail!("missing required array parameter 'tasks' ([{{description, prompt}}, …])"),
    };
    if items.is_empty() {
        bail!("'tasks' must contain at least one {{description, prompt}} entry");
    }
    if items.len() > SUBAGENT_MAX_TASKS {
        bail!("too many tasks ({}); at most {SUBAGENT_MAX_TASKS} sub-agents per call", items.len());
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let obj = item.as_object().ok_or_else(|| anyhow!("tasks[{i}] must be an object"))?;
            let prompt = obj
                .get("prompt")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow!("tasks[{i}] is missing a non-empty 'prompt'"))?;
            let desc = obj
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("delegated task");
            Ok(TaskSpec {
                label: desc.trim().to_string(),
                prompt: prompt.to_string(),
                model: obj.get("model").and_then(|v| v.as_str()).map(|s| s.trim().to_string()),
                agent: obj.get("agent").and_then(|v| v.as_str()).map(|s| s.trim().to_string()),
            })
        })
        .collect()
}

/// A fresh child agent: routed provider/config, standard tools (no agent tool
/// state → no recursion), isolated plan/undo state, shared permission policy.
/// A persona layers on its own prompt body, default model, and tool whitelist.
fn build_child(
    handle: &SubAgentHandle,
    ctx: &ToolCtx,
    model: Option<&str>,
    persona: Option<&str>,
) -> Result<Agent> {
    let persona = match persona.filter(|p| !p.trim().is_empty()) {
        Some(name) => Some(handle.personas.iter().find(|p| p.name == name.trim()).ok_or_else(|| {
            anyhow!(
                "unknown agent persona '{name}' — available: {}",
                if handle.personas.is_empty() {
                    "none configured (.rift/agents/*.md)".to_string()
                } else {
                    handle.personas.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
                }
            )
        })?),
        None => None,
    };
    // Model precedence: task model > persona model > session model.
    let model = model.or(persona.and_then(|p| p.model.as_deref()));
    let (client, cfg) = handle.child_target(model)?;
    let mut registry = ToolRegistry::standard();
    if let Some(allowed) = persona.and_then(|p| p.tools.as_ref()) {
        registry.retain_tools(allowed);
    }
    // The base prompt keeps the tool mechanics local models depend on; the
    // persona body layers role instructions on top.
    let mut prompt = crate::system_prompt_with_guide(&cfg.model, &ctx.cwd).0;
    if let Some(p) = persona {
        prompt.push_str(&format!("\n\n[persona: {}]\n{}", p.name, p.body));
    }
    prompt.push_str(
        "\n\nYou are a sub-agent handling ONE delegated task from a main agent. Complete it, \
         then reply with a concise final report (findings, files changed, verification results) — \
         that report is all the main agent receives, so include everything it needs and nothing else.",
    );
    Ok(Agent::new(client, cfg, registry, ctx.subagent_ctx(), prompt))
}

/// The last plain-text assistant message — the child's final report.
fn final_text(agent: &Agent) -> Option<String> {
    agent
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty() && !m.content.trim().is_empty())
        .map(|m| m.content.trim().to_string())
}

/// Mirror a child's tool activity into the frontend as tagged sub-agent
/// events. Content/thinking streams are skipped — N concurrent children
/// streaming prose would drown the log.
fn spawn_forwarder(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    reg: BgTasks,
    tag: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let fwd = match ev {
                AgentEvent::ToolStart { name, args } => {
                    let args: String = args.chars().take(80).collect();
                    AgentEvent::SubAgentActivity {
                        tag: tag.clone(),
                        text: format!("→ {name} {args}"),
                        warn: false,
                    }
                }
                AgentEvent::ToolResult { name, ok, .. } => AgentEvent::SubAgentActivity {
                    tag: tag.clone(),
                    text: format!("{} {name}", if ok { '✓' } else { '✗' }),
                    warn: false,
                },
                AgentEvent::Warning(w) => AgentEvent::SubAgentActivity {
                    tag: tag.clone(),
                    text: format!("! {w}"),
                    warn: true,
                },
                AgentEvent::Done(s) => {
                    AgentEvent::SubAgentFinished { tag: tag.clone(), steps: s.iterations }
                }
                _ => continue,
            };
            reg.emit(fwd);
        }
    })
}

/// Run all children concurrently and wait: the tool result is the combined
/// reports. Cancelling the parent turn drops these futures, which aborts the
/// children's in-flight requests too.
async fn run_foreground(handle: &SubAgentHandle, ctx: &ToolCtx, specs: Vec<TaskSpec>) -> String {
    let reg = ctx.bg().clone();
    let many = specs.len() > 1;
    let futs = specs.into_iter().enumerate().map(|(i, spec)| {
        let TaskSpec { label, prompt, model, agent } = spec;
        let child = build_child(handle, ctx, model.as_deref(), agent.as_deref());
        let reg = reg.clone();
        async move {
            let mut agent = match child {
                Ok(a) => a,
                Err(e) => return (label, format!("ERROR: {e:#}")),
            };
            let tag = format!("agent {}", i + 1);
            reg.emit(AgentEvent::SubAgentStarted {
                tag: tag.clone(),
                model: agent.cfg.model.clone(),
                label: label.clone(),
            });
            let (tx, rx) = mpsc::unbounded_channel();
            let fwd = spawn_forwarder(rx, reg.clone(), tag);
            let res = agent.run_turn(&prompt, &tx, &CancellationToken::new()).await;
            drop(tx);
            let _ = fwd.await;
            let report = match res {
                Ok(_) => final_text(&agent)
                    .unwrap_or_else(|| "(the sub-agent finished without a final report)".into()),
                Err(e) => format!("ERROR: sub-agent failed: {e:#}"),
            };
            (label, report)
        }
    });
    let results = futures_util::future::join_all(futs).await;
    if !many {
        return results.into_iter().next().map(|(_, r)| r).unwrap_or_default();
    }
    results
        .into_iter()
        .enumerate()
        .map(|(i, (label, report))| format!("### agent {} — {label}\n{report}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Launch the children detached and return immediately. Each is a background
/// task: its report lands in the task output buffer, and completion emits the
/// TaskFinished notification the frontend turns into a [task notification].
fn run_background(handle: &SubAgentHandle, ctx: &ToolCtx, specs: Vec<TaskSpec>) -> Result<String> {
    let reg = ctx.bg().clone();
    let mut lines = vec![];
    for TaskSpec { label, prompt, model, agent: persona } in specs {
        // Routing failures surface as the tool result, before anything runs.
        let mut agent = build_child(handle, ctx, model.as_deref(), persona.as_deref())?;
        let (id, cancel) = reg.register(TaskKind::Agent, &label, None)?;
        lines.push(format!("  #{id} — {label} ({})", agent.cfg.model));
        let reg2 = reg.clone();
        tokio::spawn(async move {
            let (tx, rx) = mpsc::unbounded_channel();
            let fwd = spawn_forwarder(rx, reg2.clone(), format!("task #{id}"));
            let res = agent.run_turn(&prompt, &tx, &cancel).await;
            drop(tx);
            let _ = fwd.await;
            if cancel.is_cancelled() {
                return; // killed via the task tool — the registry already marked it
            }
            match res {
                Ok(_) => {
                    let report = final_text(&agent)
                        .unwrap_or_else(|| "(the sub-agent finished without a final report)".into());
                    reg2.append_output(id, &report);
                    reg2.finish(id, Some(0));
                }
                Err(e) => {
                    reg2.append_output(id, &format!("ERROR: sub-agent failed: {e:#}"));
                    reg2.finish(id, Some(1));
                }
            }
        });
    }
    Ok(format!(
        "launched {} background agent(s):\n{}\nThey keep running while the conversation continues. \
         Each sends a [task notification] with its report when done; the task tool (id=N) shows \
         progress and output meanwhile. Do not wait idly — continue other work or end your turn.",
        lines.len(),
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasks_accepts_array_and_bare_prompt() {
        let args: Map<String, Value> = serde_json::from_value(json!({
            "tasks": [
                {"description": "audit tests", "prompt": "Audit the test suite."},
                {"description": "check docs", "prompt": "Check the docs."}
            ]
        }))
        .unwrap();
        let specs = parse_tasks(&args).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].label, "audit tests");
        assert!(specs[0].model.is_none());

        // Lenient form: a single task passed at the top level.
        let bare: Map<String, Value> =
            serde_json::from_value(json!({"description": "solo", "prompt": "Do the thing."})).unwrap();
        let specs = parse_tasks(&bare).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].prompt, "Do the thing.");

        // Per-task model requests parse through.
        let routed: Map<String, Value> = serde_json::from_value(json!({
            "tasks": [{"description": "impl", "prompt": "Write it.", "model": "fast"}]
        }))
        .unwrap();
        assert_eq!(parse_tasks(&routed).unwrap()[0].model.as_deref(), Some("fast"));
    }

    /// A provider stub, enough for routing tests (never actually called).
    struct StubProvider(String);
    #[async_trait]
    impl Provider for StubProvider {
        fn base_url(&self) -> &str {
            &self.0
        }
        async fn tags(&self) -> Result<Vec<rift_provider::ModelEntry>> {
            Ok(vec![])
        }
        async fn show(&self, _m: &str) -> Result<rift_provider::ModelCapabilities> {
            Ok(rift_provider::ModelCapabilities::default())
        }
        async fn chat_stream(
            &self,
            _req: &rift_provider::ChatRequest,
            _on_delta: &mut (dyn FnMut(rift_provider::StreamDelta) + Send),
        ) -> Result<rift_provider::ChatOutcome> {
            bail!("stub")
        }
    }

    fn routing_handle() -> SubAgentHandle {
        SubAgentHandle {
            client: Arc::new(StubProvider("session".into())),
            cfg: AgentConfig {
                model: "session-model".into(),
                think: Some(true),
                effort: Some("max".into()),
                ..Default::default()
            },
            factory: Some(Arc::new(|model: &str| {
                Ok((Arc::new(StubProvider(format!("routed:{model}"))) as Arc<dyn Provider>, model.to_string()))
            })),
            roles: std::collections::HashMap::from([("fast".to_string(), "cheap:7b".to_string())]),
            personas: vec![],
        }
    }

    #[test]
    fn persona_files_parse_and_load() {
        let text = "---\nname: reviewer\ndescription: read-only code reviewer\nmodel: fast\ntools: read, grep, glob\n---\n\nReview code without changing it.";
        let p = parse_persona(text, "fallback");
        assert_eq!(p.name, "reviewer");
        assert_eq!(p.description, "read-only code reviewer");
        assert_eq!(p.model.as_deref(), Some("fast"));
        assert_eq!(p.tools.as_deref(), Some(&["read".to_string(), "grep".into(), "glob".into()][..]));
        assert_eq!(p.body, "Review code without changing it.");
        // No frontmatter: whole file is the body, filename is the name.
        let bare = parse_persona("Just do things.", "doer");
        assert_eq!(bare.name, "doer");
        assert!(bare.model.is_none() && bare.tools.is_none());
        assert_eq!(bare.body, "Just do things.");

        // Loading: project dir wins name conflicts, sorted output.
        let dir = std::env::temp_dir().join(format!("rift-personas-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".rift/agents")).unwrap();
        std::fs::write(dir.join(".rift/agents/reviewer.md"), text).unwrap();
        std::fs::write(dir.join(".rift/agents/tester.md"), "---\nname: tester\ndescription: runs tests\n---\nRun the tests.").unwrap();
        let personas = load_personas(&dir);
        assert_eq!(personas.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["reviewer", "tester"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persona_selection_and_tool_whitelist() {
        let mut handle = routing_handle();
        handle.personas = vec![AgentPersona {
            name: "reviewer".into(),
            description: "read-only".into(),
            model: Some("fast".into()),
            tools: Some(vec!["read".into(), "grep".into()]),
            body: "Review only.".into(),
        }];
        let ctx = ToolCtx::new(std::env::temp_dir());
        // Persona model applies when the task sets none.
        let agent = build_child(&handle, &ctx, None, Some("reviewer")).unwrap();
        assert_eq!(agent.cfg.model, "cheap:7b");
        assert_eq!(agent.registry().names(), vec!["read".to_string(), "grep".into()]);
        // Task model outranks the persona's.
        let agent = build_child(&handle, &ctx, Some("other:1b"), Some("reviewer")).unwrap();
        assert_eq!(agent.cfg.model, "other:1b");
        // Unknown personas fail with the available list.
        let err = match build_child(&handle, &ctx, None, Some("nope")) {
            Err(e) => e,
            Ok(_) => panic!("unknown persona must fail"),
        };
        assert!(err.to_string().contains("reviewer"), "got: {err}");
    }

    #[test]
    fn child_target_routes_roles_and_resets_thinking() {
        let handle = routing_handle();
        // No request → session model, thinking setup inherited.
        let (client, cfg) = handle.child_target(None).unwrap();
        assert_eq!(client.base_url(), "session");
        assert_eq!(cfg.model, "session-model");
        assert_eq!(cfg.effort.as_deref(), Some("max"));
        assert!(!cfg.always_task);
        // Role name → resolved model via the factory; think/effort reset
        // (the parent's capability check doesn't transfer across models).
        let (client, cfg) = handle.child_target(Some("fast")).unwrap();
        assert_eq!(client.base_url(), "routed:cheap:7b");
        assert_eq!(cfg.model, "cheap:7b");
        assert!(cfg.think.is_none() && cfg.effort.is_none());
        // A full model string routes as-is; the session model short-circuits.
        let (_, cfg) = handle.child_target(Some("other:34b")).unwrap();
        assert_eq!(cfg.model, "other:34b");
        let (client, _) = handle.child_target(Some("session-model")).unwrap();
        assert_eq!(client.base_url(), "session");
        // No factory + a different model = a clear error.
        let mut bare = routing_handle();
        bare.factory = None;
        assert!(bare.child_target(Some("fast")).is_err());
        assert!(bare.child_target(None).is_ok());
    }

    #[test]
    fn parse_tasks_rejects_bad_shapes() {
        let empty: Map<String, Value> = serde_json::from_value(json!({"tasks": []})).unwrap();
        assert!(parse_tasks(&empty).is_err());
        let none: Map<String, Value> = serde_json::from_value(json!({})).unwrap();
        assert!(parse_tasks(&none).is_err());
        let missing_prompt: Map<String, Value> =
            serde_json::from_value(json!({"tasks": [{"description": "x"}]})).unwrap();
        assert!(parse_tasks(&missing_prompt).is_err());
        let too_many: Map<String, Value> = serde_json::from_value(json!({
            "tasks": (0..5).map(|i| json!({"description": "t", "prompt": format!("p{i}")})).collect::<Vec<_>>()
        }))
        .unwrap();
        assert!(parse_tasks(&too_many).is_err());
    }

    #[tokio::test]
    async fn forwarder_translates_child_events_to_tagged_subagent_events() {
        let reg = crate::tasks::BgTasks::default();
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
        reg.set_notify(notify_tx);

        let (tx, rx) = mpsc::unbounded_channel();
        let fwd = spawn_forwarder(rx, reg, "agent 1".into());
        tx.send(AgentEvent::ToolStart { name: "read".into(), args: "path=x".into() }).unwrap();
        tx.send(AgentEvent::ToolResult { name: "read".into(), ok: true, preview: String::new() })
            .unwrap();
        tx.send(AgentEvent::Warning("slow".into())).unwrap();
        // Content/thinking must NOT forward — N children streaming prose
        // would drown the frontend.
        tx.send(AgentEvent::Content("prose".into())).unwrap();
        tx.send(AgentEvent::Done(crate::TurnStats { iterations: 3, ..Default::default() }))
            .unwrap();
        drop(tx);
        fwd.await.unwrap();

        match notify_rx.try_recv().unwrap() {
            AgentEvent::SubAgentActivity { tag, text, warn } => {
                assert_eq!(tag, "agent 1");
                assert_eq!(text, "→ read path=x");
                assert!(!warn);
            }
            other => panic!("expected activity, got {other:?}"),
        }
        matches!(notify_rx.try_recv().unwrap(), AgentEvent::SubAgentActivity { .. });
        match notify_rx.try_recv().unwrap() {
            AgentEvent::SubAgentActivity { text, warn, .. } => {
                assert_eq!(text, "! slow");
                assert!(warn);
            }
            other => panic!("expected warning activity, got {other:?}"),
        }
        match notify_rx.try_recv().unwrap() {
            AgentEvent::SubAgentFinished { tag, steps } => {
                assert_eq!(tag, "agent 1");
                assert_eq!(steps, 3);
            }
            other => panic!("expected finished, got {other:?}"),
        }
        assert!(notify_rx.try_recv().is_err(), "content must not be forwarded");
    }

    #[tokio::test]
    async fn unavailable_without_handle() {
        // A bare ctx (like a sub-agent's own) has no handle: the tool must
        // refuse instead of recursing.
        let ctx = ToolCtx::new(std::env::temp_dir());
        let args: Map<String, Value> =
            serde_json::from_value(json!({"tasks": [{"description": "x", "prompt": "y"}]})).unwrap();
        let err = AgentTool.execute(&args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("not available"));
    }
}
