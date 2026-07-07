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
                            "model": {"type": "string", "description": "Optional: run this task on a different model — a configured role name (e.g. 'fast', 'smart') or a full model string. Default: the session model. Route mechanical work (write code from a clear spec, run tests, fix straightforward errors) to a cheaper role when one exists; keep judgment work on the stronger model."}
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
/// One delegated task: label, prompt, and (optionally) which model runs it.
struct TaskSpec {
    label: String,
    prompt: String,
    model: Option<String>,
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
            })
        })
        .collect()
}

/// A fresh child agent: routed provider/config, standard tools (no agent tool
/// state → no recursion), isolated plan/undo state, shared permission policy.
fn build_child(handle: &SubAgentHandle, ctx: &ToolCtx, model: Option<&str>) -> Result<Agent> {
    let (client, cfg) = handle.child_target(model)?;
    let mut prompt = crate::system_prompt_with_guide(&cfg.model, &ctx.cwd).0;
    prompt.push_str(
        "\n\nYou are a sub-agent handling ONE delegated task from a main agent. Complete it, \
         then reply with a concise final report (findings, files changed, verification results) — \
         that report is all the main agent receives, so include everything it needs and nothing else.",
    );
    Ok(Agent::new(client, cfg, ToolRegistry::standard(), ctx.subagent_ctx(), prompt))
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

/// Mirror a child's tool activity into the frontend's activity log as tagged
/// Info lines. Content/thinking streams are skipped — N concurrent children
/// streaming prose would drown the log.
fn spawn_forwarder(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    reg: BgTasks,
    tag: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let line = match ev {
                AgentEvent::ToolStart { name, args } => {
                    let args: String = args.chars().take(80).collect();
                    Some(format!("[{tag}] → {name} {args}"))
                }
                AgentEvent::ToolResult { name, ok, .. } => {
                    Some(format!("[{tag}] {} {name}", if ok { '✓' } else { '✗' }))
                }
                AgentEvent::Warning(w) => Some(format!("[{tag}] ! {w}")),
                AgentEvent::Done(s) => Some(format!("[{tag}] finished — {} step(s)", s.iterations)),
                _ => None,
            };
            if let Some(l) = line {
                reg.emit(AgentEvent::Info(l));
            }
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
        let TaskSpec { label, prompt, model } = spec;
        let child = build_child(handle, ctx, model.as_deref());
        let reg = reg.clone();
        async move {
            let mut agent = match child {
                Ok(a) => a,
                Err(e) => return (label, format!("ERROR: {e:#}")),
            };
            let tag = format!("agent {}", i + 1);
            reg.emit(AgentEvent::Info(format!("⧉ {tag} started ({}): {label}", agent.cfg.model)));
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
    for TaskSpec { label, prompt, model } in specs {
        // Routing failures surface as the tool result, before anything runs.
        let mut agent = build_child(handle, ctx, model.as_deref())?;
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
        }
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
