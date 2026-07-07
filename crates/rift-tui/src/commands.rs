//! Slash commands: lines starting with `/` are intercepted by the TUI and
//! handled here instead of being sent to the model. Commands run inside the
//! agent task (which owns the `Agent`); output flows back to the UI as
//! `UiEffect`s. Esc cancels long-running commands (`/compact`, `/swarm`)
//! through the same CancellationToken as normal turns.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rift_core::{
    builtin_bash_deny, compact, run_swarm, Agent, AgentEvent, Candidate, McpClient, McpTool,
    SessionStore, Swarm,
};
use rift_ollama::{Message, OllamaClient, Provider, Role};
use rift_openai::OpenAiClient;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::app::Kind;

/// One row of an interactive picker: `value` is substituted into the
/// command template; `label`/`detail` are what the user sees.
pub struct PickerItem {
    pub value: String,
    pub label: String,
    pub detail: String,
}

/// UI mutations a command can request; drained by the TUI event loop.
pub enum UiEffect {
    /// Open an interactive list; Enter runs `template` with `{}` replaced by
    /// the chosen item's value (e.g. "/model {}").
    Picker { title: String, items: Vec<PickerItem>, template: String },
    /// Styled block appended to the transcript pane.
    Out(Kind, String),
    /// Styled block appended to the activity-log pane.
    Log(Kind, String),
    /// Raw unified diff — the UI classifies and colors each line.
    Diff(String),
    /// Wipe both panes (e.g. `/clear`).
    Clear,
    /// Wipe both panes and rebuild the transcript from message history.
    Seed(Vec<Message>),
    /// Model name changed; update the status bar.
    Model(String),
    /// Default host changed (/host); tracked so /restart relaunches
    /// against the CURRENT server, not the startup one.
    Host(String),
    /// /restart: tear down the TUI and relaunch resuming this session.
    Restart,
    /// Replace the pinned plan checklist (e.g. /plan clear).
    Plan(Vec<rift_core::PlanItem>),
    /// Suspend the TUI, open the file in $EDITOR, then hot-reload config.
    EditFile(PathBuf),
    /// Ask the terminal to set the clipboard (OSC 52) — emitted by the UI
    /// loop, which owns stdout.
    Osc52(String),
    /// Update the status line only — for UI-side async tasks whose late
    /// completion must not reset a newer turn's busy/cancel state.
    Status(String),
    /// Fresh `git diff` output for the live diff pane (UI-side refresh task).
    TurnDiff(String),
    /// A /btw side question finished (UI-side task): render the answer and,
    /// on success, remember the exchange for follow-up side questions.
    Btw { question: String, reply: String, ok: bool },
    /// Command finished; status-line text. Always the final effect.
    Done(String),
}

/// State owned by the agent task that commands need beyond the Agent itself.
pub struct CmdCx {
    pub store: SessionStore,
    pub cwd: PathBuf,
    /// (server name, registered tool count) for each configured MCP server.
    pub mcp: Vec<(String, usize)>,
    pub config_path: Option<PathBuf>,
    /// Default Ollama host + configured providers, for provider-aware `/model`.
    pub host: String,
    pub providers: std::collections::HashMap<String, rift_core::ProviderConfig>,
}

/// (name, argument hint, one-line description) — the single source of truth
/// driving both `/help` and the input popup palette.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/approve", "[on|off]", "toggle approval mode for write/edit/bash"),
    ("/btw", "<question>|clear", "quick side question — sees the conversation, no tools, never joins the history; works while the agent is busy"),
    ("/clear", "", "wipe the conversation (keeps the session file)"),
    ("/config", "[edit]", "show or edit .rift.json (hot-reloads permissions)"),
    ("/compact", "", "force history compaction now"),
    ("/copy", "[all|log]", "copy last reply / whole transcript / activity log"),
    ("/diff", "", "git diff of the working tree"),
    ("/export", "", "save the transcript as markdown"),
    ("/goal", "<condition>|clear", "work until the model verifies the goal is met (auto-continues turns)"),
    ("/help", "", "list commands and keys"),
    ("/host", "[url]", "show or switch the model server — Ollama or OpenAI-compatible, auto-detected"),
    ("/init", "", "generate a RIFT.md project guide"),
    ("/loop", "[30s|5m|2h] <prompt>|stop", "re-run a prompt or /command on an interval (or back-to-back)"),
    ("/mcp", "[add [--global] <name> <cmd> [args…]|new [--global] <desc>|trust <name>]", "list MCP servers, connect an existing one, generate one, manage trust"),
    ("/merge", "<name> [--cleanup]", "apply a swarm candidate's patch"),
    ("/model", "[name]", "list models on the server, or switch model"),
    ("/permissions", "", "active shell deny patterns"),
    ("/plan", "[clear]", "show or clear the agent's task checklist"),
    ("/sessions", "[n]", "list saved sessions, or resume the nth"),
    ("/save", "<name>", "name this session (keeps autosaving to it)"),
    ("/skills", "[new [--global] <desc>]", "list skills, or generate one (project or user-wide)"),
    ("/swarm", "<task> [--models a,b] [--judge m]", "WarpDrive race in isolated worktrees"),
    ("/tasks", "[kill <id>]", "background tasks (shells + agents): list, or kill one"),
    ("/worktrees", "", "list swarm worktrees + patches"),
    ("/think", "[on|off|auto|minimal|low|medium|high|xhigh|max]", "thinking mode and reasoning effort (capability-checked)"),
    ("/tokens", "", "context budget, usage estimate, calibration"),
    ("/yolo", "[off]", "stop asking before write/edit/bash (deny list still applies); /yolo off restores prompts"),
    ("/stats", "", "session totals: turns, tokens, tools, compactions"),
    ("/system", "[text]", "show or override the system prompt"),
    ("/temp", "<0.0-2.0>", "set sampling temperature"),
    ("/theme", "[name]", "browse/switch color themes (13 built-in: dark, light, mono, dracula, nord, gruvbox, …)"),
    ("/ctx", "<n>", "set context window (num_ctx)"),
    ("/retry", "", "re-run the last prompt"),
    ("/rewind", "[n]", "rewind n turns (default 1): restore write/edit changes AND the conversation"),
    ("/quit", "", "exit rift"),
    ("/tools", "", "tools the model can call (builtin + MCP)"),
    ("/undo", "", "revert last turn's write/edit changes"),
    ("/restart", "", "relaunch rift and resume this session (loads updates)"),
    ("/update", "", "update rift to the latest release"),
];

fn help_text() -> String {
    let mut out = String::from("commands:\n");
    for (name, args, desc) in COMMANDS {
        let left = if args.is_empty() { (*name).to_string() } else { format!("{name} {args}") };
        out.push_str(&format!("  {left:<30}{desc}\n"));
    }
    out.push_str(
        "\nkeys: Enter send · Ctrl+J newline · Tab focus · Ctrl+L log · Ctrl+D live diff · Ctrl+T toggle \
         mouse capture (off = select/copy text natively) · Esc cancel · /quit exit\n\
         @path in a prompt attaches a file outline; @photo.png attaches the image itself \
         (vision models) — Tab completes either",
    );
    out
}

/// Execute one slash-command line. Always emits `UiEffect::Done` last.
pub async fn run_command(
    line: &str,
    agent: &mut Agent,
    cx: &mut CmdCx,
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) {
    let line = line.trim();
    let (cmd, rest) = match line.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    let result = match cmd {
        "/help" => {
            let _ = fx.send(UiEffect::Out(Kind::Info, help_text()));
            Ok("ready".into())
        }
        "/approve" => cmd_approve(rest, agent, fx),
        "/yolo" => cmd_yolo(rest, agent, fx),
        "/config" => cmd_config(rest, agent, cx, fx),
        "/model" => cmd_model(rest, agent, cx, fx).await,
        "/clear" => cmd_clear(agent, cx, fx),
        "/compact" => cmd_compact(agent, fx, cancel).await,
        "/copy" => cmd_copy(rest, agent, fx).await,
        "/tokens" => cmd_tokens(agent, fx),
        "/sessions" => cmd_sessions(rest, agent, cx, fx),
        "/tools" => cmd_tools(agent, fx),
        "/mcp" => cmd_mcp(rest, agent, cx, fx).await,
        "/permissions" => cmd_permissions(agent, cx, fx),
        "/plan" => cmd_plan(rest, agent, fx),
        "/swarm" => cmd_swarm(rest, agent, cx, fx, cancel).await,
        "/merge" => cmd_merge(rest, cx, fx).await,
        "/undo" => cmd_undo(agent, fx),
        "/update" => cmd_update(fx, cancel).await,
        "/restart" => {
            let _ = fx.send(UiEffect::Restart);
            Ok("restarting…".into())
        }
        "/diff" => cmd_diff(cx, fx).await,
        "/host" => cmd_host(rest, agent, cx, fx, cancel).await,
        "/think" => cmd_think(rest, agent, fx).await,
        "/export" => cmd_export(agent, cx, fx),
        "/system" => cmd_system(rest, agent, fx),
        "/temp" => cmd_temp(rest, agent, fx),
        "/ctx" => cmd_ctx(rest, agent, fx),
        "/save" => cmd_save(rest, agent, cx, fx),
        "/tasks" => cmd_tasks(rest, agent, fx),
        "/rewind" => cmd_rewind(rest, agent, cx, fx),
        "/worktrees" => cmd_worktrees(cx, fx).await,
        // Handled in the chat input (they drive UI state directly);
        // reachable here only via odd nesting like a /loop body.
        "/goal" | "/loop" | "/btw" | "/theme" => Err(anyhow!("{cmd} runs from the chat input directly")),
        other => Err(anyhow!("unknown command '{other}' — /help lists available commands")),
    };
    let status = match result {
        Ok(s) => s,
        Err(e) => {
            let _ = fx.send(UiEffect::Out(Kind::Warn, format!("! {e:#}")));
            "command failed".into()
        }
    };
    let _ = fx.send(UiEffect::Done(status));
}

/// /tasks — the user-facing view of the background task table (the model
/// uses the `task` tool for the same registry).
fn cmd_tasks(arg: &str, agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if let Some(rest) = arg.strip_prefix("kill") {
        let id: u64 = rest
            .trim()
            .parse()
            .map_err(|_| anyhow!("usage: /tasks kill <id> (bare /tasks lists ids)"))?;
        let view = agent.ctx().bg().kill(id)?;
        let _ = fx.send(UiEffect::Out(Kind::Info, format!("⚙ killed task #{id} ({})", view.label)));
        return Ok(format!("task #{id} killed"));
    }
    if !arg.is_empty() {
        bail!("usage: /tasks — list · /tasks kill <id> — terminate one");
    }
    let tasks = agent.ctx().bg().list();
    if tasks.is_empty() {
        let _ = fx.send(UiEffect::Out(
            Kind::Info,
            "no background tasks this session — the model starts them with bash \
             run_in_background=true or agent background=true"
                .into(),
        ));
        return Ok("no background tasks".into());
    }
    let running = tasks.iter().filter(|t| t.status == rift_core::TaskStatus::Running).count();
    let mut out = String::from("background tasks:\n");
    for t in &tasks {
        out.push_str(&format!(
            "  #{} [{}] {} ({}, {}s, {} bytes of output)\n",
            t.id,
            t.status.describe(),
            t.label,
            t.kind.label(),
            t.elapsed_secs,
            t.output_bytes
        ));
    }
    out.push_str("kill one with /tasks kill <id>");
    let _ = fx.send(UiEffect::Out(Kind::Info, out));
    Ok(format!("{running} running / {} total", tasks.len()))
}

async fn cmd_model(
    arg: &str,
    agent: &mut Agent,
    cx: &CmdCx,
    fx: &UnboundedSender<UiEffect>,
) -> Result<String> {
    if arg.is_empty() {
        let models = agent.client().tags().await.context("listing models")?;
        if models.is_empty() {
            bail!("no models installed on {}", agent.client().base_url());
        }
        let count = models.len();
        // Configured roles first — the named tiers of a multi-model setup.
        let mut items: Vec<PickerItem> = agent
            .ctx()
            .subagent_handle()
            .map(|h| {
                let mut roles: Vec<(String, String)> = h.roles.into_iter().collect();
                roles.sort();
                roles
                    .into_iter()
                    .map(|(role, model)| PickerItem {
                        value: model.clone(),
                        label: format!("{role} → {model}"),
                        detail: "configured role".into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.extend(models.into_iter().map(|m| {
            let detail = if m.name == agent.cfg.model {
                "current".to_string()
            } else {
                m.capabilities.join(", ")
            };
            PickerItem { value: m.name.clone(), label: m.name, detail }
        }));
        let _ = fx.send(UiEffect::Picker {
            title: format!("select model — {}", agent.client().base_url()),
            items,
            template: "/model {}".into(),
        });
        return Ok(format!("{count} model(s) — ↑↓ select, Enter switch, Esc cancel"));
    }

    // Route `provider/model` through a configured provider; a bare name uses
    // the default Ollama host. Preflight the *target* client, then swap it in
    // so the rest of the session talks to the right endpoint.
    let (client, actual) = crate::build_provider(arg, &cx.host, &cx.providers);
    // Same preflight as startup: capability check + num_ctx clamp.
    let show = client
        .show(&actual)
        .await
        .map_err(|e| anyhow!("model '{arg}' not usable: {e}"))?;
    if !show.supports("tools") {
        bail!("model '{arg}' does not have the 'tools' capability");
    }
    agent.cfg.think = if show.supports("thinking") { None } else { Some(false) };
    let mut note = String::new();
    if let Some(max) = show.context_length() {
        // Mirrors startup: provider-routed models don't send num_ctx to the
        // server, so a bigger reported context is adopted as the working
        // budget (capped — huge hosted contexts would bloat every request).
        const ADOPT_CTX_MAX: u64 = 131_072;
        let provider_routed = actual != arg;
        if agent.cfg.num_ctx > max {
            note = format!(" (num_ctx clamped {} → {max})", agent.cfg.num_ctx);
            agent.cfg.num_ctx = agent.cfg.num_ctx.min(max);
        } else if provider_routed && max > agent.cfg.num_ctx {
            let adopted = max.min(ADOPT_CTX_MAX);
            if adopted > agent.cfg.num_ctx {
                note = format!(" (context budget {} → {adopted}; /ctx overrides)", agent.cfg.num_ctx);
                agent.cfg.num_ctx = adopted;
            }
        }
    }
    agent.cfg.model = actual.clone();
    agent.set_client(client);
    // The UI carries the ADDRESSABLE name (provider prefix intact) — it's
    // what /restart relaunches with and what the status line shows.
    let _ = fx.send(UiEffect::Model(arg.to_string()));
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("switched to {arg}{note}")));
    Ok(format!("model: {arg}"))
}

fn cmd_clear(agent: &mut Agent, cx: &mut CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    agent.messages.truncate(1); // keep the system prompt
    let cwd = cx.cwd.display().to_string();
    cx.store.save(&agent.cfg.model, &cwd, &agent.messages)?;
    let _ = fx.send(UiEffect::Clear);
    Ok("conversation cleared".into())
}

fn cmd_temp(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if arg.is_empty() {
        let cur = agent.cfg.temperature.map_or("model default".into(), |t| t.to_string());
        let _ = fx.send(UiEffect::Out(Kind::Info, format!("temperature: {cur}  (set with /temp <0.0-2.0>)")));
        return Ok("temperature shown".into());
    }
    let t: f64 = arg.parse().map_err(|_| anyhow!("not a number: '{arg}' — usage: /temp <0.0-2.0>"))?;
    if !(0.0..=2.0).contains(&t) {
        bail!("temperature must be between 0.0 and 2.0 (low = more reliable tool calling)");
    }
    agent.cfg.temperature = Some(t);
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("temperature set to {t}")));
    Ok(format!("temperature: {t}"))
}

fn cmd_ctx(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if arg.is_empty() {
        let _ = fx.send(UiEffect::Out(Kind::Info, format!("num_ctx: {}  (set with /ctx <n>)", agent.cfg.num_ctx)));
        return Ok("num_ctx shown".into());
    }
    let n: u64 = arg.parse().map_err(|_| anyhow!("not an integer: '{arg}' — usage: /ctx <n>"))?;
    if n < 512 {
        bail!("num_ctx too small (minimum 512)");
    }
    agent.cfg.num_ctx = n;
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!("num_ctx set to {n} (effective next turn; /model re-clamps to the model's max)"),
    ));
    Ok(format!("num_ctx: {n}"))
}

fn cmd_system(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if arg.is_empty() {
        let current = agent.messages.first().map_or_else(String::new, |m| m.content.clone());
        let _ = fx.send(UiEffect::Out(Kind::Info, format!("system prompt:\n{current}")));
        return Ok("system prompt shown".into());
    }
    let has_system = agent.messages.first().is_some_and(|m| m.role == Role::System);
    let sys = Message::system(arg.to_string());
    if has_system {
        agent.messages[0] = sys;
    } else {
        agent.messages.insert(0, sys);
    }
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        "system prompt overridden for this session (kept across /clear; restart to reset)".into(),
    ));
    Ok("system prompt set".into())
}

fn cmd_save(arg: &str, agent: &Agent, cx: &mut CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if arg.is_empty() {
        bail!("usage: /save <name>");
    }
    let cwd = cx.cwd.display().to_string();
    let path = cx.store.save_as(arg, &agent.cfg.model, &cwd, &agent.messages)?;
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or(arg).to_string();
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!("session named '{name}' — autosaving to {}", path.display()),
    ));
    Ok(format!("saved as {name}"))
}

async fn cmd_worktrees(cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let swarm = Swarm::discover(&cx.cwd).await?;
    let worktrees = swarm.list_worktrees();
    let patches = swarm.list_patches();
    if worktrees.is_empty() && patches.is_empty() {
        let _ = fx.send(UiEffect::Out(
            Kind::Info,
            "no swarm worktrees or patches — start a race with /swarm <task>".into(),
        ));
        return Ok("no worktrees".into());
    }
    let mut out = String::new();
    if !worktrees.is_empty() {
        out.push_str(&format!("worktrees ({}) under .rift/worktrees/:\n", worktrees.len()));
        for w in &worktrees {
            out.push_str(&format!("  {w}\n"));
        }
    }
    if !patches.is_empty() {
        out.push_str(&format!("patches ({}):\n", patches.len()));
        for (name, _) in &patches {
            out.push_str(&format!("  {name}\n"));
        }
    }
    out.push_str("\napply a winner: /merge <name>   ·   add --cleanup to also remove all worktrees");
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().to_string()));
    Ok(format!("{} worktree(s), {} patch(es)", worktrees.len(), patches.len()))
}

async fn cmd_compact(
    agent: &mut Agent,
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) -> Result<String> {
    if agent.messages.len() <= 2 {
        bail!("nothing to compact yet");
    }
    let overhead = compact::estimate_tokens(
        &serde_json::to_string(&agent.registry().tool_defs()).unwrap_or_default(),
    );
    let cal = agent.calibration();
    let before = compact::estimate_prompt_tokens(&agent.messages, overhead, cal);
    let touched = compact::prune_old_turns(&mut agent.messages);
    let after_prune = compact::estimate_prompt_tokens(&agent.messages, overhead, cal);
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!("pruned {touched} old output(s): ~{before} → ~{after_prune} tok\nsummarizing earlier history…"),
    ));
    let rebuilt = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = compact::summarize_history(&**agent.client(), &agent.cfg.model, agent.cfg.num_ctx, &agent.messages) => r?,
    };
    agent.messages = rebuilt;
    let after = compact::estimate_prompt_tokens(&agent.messages, overhead, cal);
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!("compacted: ~{before} → ~{after} tok ({} messages)", agent.messages.len()),
    ));
    Ok(format!("compacted to ~{after} tok"))
}

fn cmd_tokens(agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let overhead = compact::estimate_tokens(
        &serde_json::to_string(&agent.registry().tool_defs()).unwrap_or_default(),
    );
    let cal = agent.calibration();
    let est = compact::estimate_prompt_tokens(&agent.messages, overhead, cal);
    let budget = compact::usable_budget(agent.cfg.num_ctx);
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!(
            "model:       {} @ {}\n\
             num_ctx:     {} ({} usable after output reserve + safety margin)\n\
             history:     {} messages, ~{est} tok estimated (tool schemas ~{overhead} tok)\n\
             headroom:    ~{} tok\n\
             calibration: {cal:.2}× (actual/estimated, learned from prompt_eval_count)",
            agent.cfg.model,
            agent.client().base_url(),
            agent.cfg.num_ctx,
            budget,
            agent.messages.len(),
            budget.saturating_sub(est),
        ),
    ));
    Ok(format!("~{est}/{budget} tok"))
}

fn fmt_age(saved_at: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(saved_at);
    let secs = now.saturating_sub(saved_at);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn cmd_sessions(
    arg: &str,
    agent: &mut Agent,
    cx: &mut CmdCx,
    fx: &UnboundedSender<UiEffect>,
) -> Result<String> {
    let paths = SessionStore::list()?;
    if paths.is_empty() {
        bail!("no saved sessions");
    }

    if arg.is_empty() {
        let items: Vec<PickerItem> = paths
            .iter()
            .take(20)
            .enumerate()
            .map(|(i, path)| match SessionStore::load(path) {
                Ok(s) => {
                    let stem = path.file_stem().and_then(|p| p.to_str()).unwrap_or("");
                    // Named sessions (from /save) show their name; timestamped ones don't.
                    let name = if stem.chars().all(|c| c.is_ascii_digit() || c == '-') {
                        String::new()
                    } else {
                        stem.to_string()
                    };
                    PickerItem {
                        value: (i + 1).to_string(),
                        label: format!("{name:<14} {:<9} {:<16} {:>3} msgs", fmt_age(s.saved_at), s.model, s.messages.len()),
                        detail: if path == cx.store.path() { "current".into() } else { String::new() },
                    }
                }
                Err(_) => PickerItem {
                    value: (i + 1).to_string(),
                    label: format!("(unreadable) {}", path.display()),
                    detail: String::new(),
                },
            })
            .collect();
        let count = paths.len();
        let _ = fx.send(UiEffect::Picker {
            title: "resume session".into(),
            items,
            template: "/sessions {}".into(),
        });
        return Ok(format!("{count} session(s) — ↑↓ select, Enter resume, Esc cancel"));
    }

    let n: usize = arg.parse().context("usage: /sessions <number>")?;
    let path = paths.get(n.saturating_sub(1)).ok_or_else(|| anyhow!("no session #{n}"))?;
    let saved = SessionStore::load(path)?;
    let mut messages = saved.messages;
    // Keep the freshly composed system prompt (cwd may differ from then).
    if messages.first().is_some_and(|m| m.role == Role::System) {
        messages[0] = agent.messages[0].clone();
    }
    agent.messages = messages.clone();
    cx.store = SessionStore::at(path.clone());
    let count = messages.len();
    let _ = fx.send(UiEffect::Seed(messages));
    Ok(format!("resumed session #{n} ({count} messages)"))
}

fn cmd_tools(agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let defs = agent.registry().tool_defs();
    let mut out = String::from("tools the model can call:\n");
    for def in &defs {
        let desc: String = def.function.description.chars().take(70).collect();
        out.push_str(&format!("  {:<14} {desc}\n", def.function.name));
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!("{} tool(s)", defs.len()))
}

async fn cmd_mcp(
    arg: &str,
    agent: &mut Agent,
    cx: &mut CmdCx,
    fx: &UnboundedSender<UiEffect>,
) -> Result<String> {
    // Re-read the config so trust operations see current file contents, not
    // the startup snapshot.
    let config = rift_core::Config::load(&cx.cwd)?.config;
    let (sub, name) = match arg.split_once(char::is_whitespace) {
        Some((s, n)) => (s, n.trim()),
        None => (arg, ""),
    };
    match (sub, name) {
        ("", _) => {
            let mut out = String::new();
            match &cx.config_path {
                Some(p) => out.push_str(&format!("config: {}\n", p.display())),
                None => out.push_str("config: none found (.rift.json or ~/.config/rift/config.json)\n"),
            }
            if config.mcp.is_empty() && config.project_mcp.is_empty() {
                out.push_str("no MCP servers configured");
            } else {
                let active = |n: &str| {
                    cx.mcp
                        .iter()
                        .find(|(an, _)| an == n)
                        .map(|(_, c)| format!("running, {c} tool(s) as {n}_<tool>"))
                        .unwrap_or_else(|| "not running".into())
                };
                out.push_str("MCP servers:\n");
                for (n, s) in &config.mcp {
                    out.push_str(&format!("  {n}: {} — user config, {}\n", s.command, active(n)));
                }
                for (n, s) in &config.project_mcp {
                    let trust = if rift_core::mcp_entry_trusted(n, s) { "trusted" } else { "NOT trusted" };
                    out.push_str(&format!("  {n}: {} — project config, {trust}, {}\n", s.command, active(n)));
                }
                out.push_str("(/mcp trust <name> starts an untrusted project server; /mcp untrust <name> revokes)");
            }
            let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
            Ok(format!("{} server(s) running", cx.mcp.len()))
        }
        ("trust", "") | ("untrust", "") => bail!("usage: /mcp {sub} <name>"),
        ("trust", n) => {
            let server = config
                .project_mcp
                .get(n)
                .ok_or_else(|| anyhow!("no MCP server '{n}' in the project config (only project entries need trusting)"))?;
            rift_core::trust_mcp_entry(n, server)?;
            if cx.mcp.iter().any(|(an, _)| an == n) {
                let msg = format!("mcp '{n}' already running; trust recorded");
                let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
                return Ok(msg);
            }
            // Spawn live — tool defs rebuild per request, so it's usable now.
            let mcp = McpClient::spawn(n, server).await?;
            let tools = mcp.list_tools().await?;
            let count = tools.len();
            for info in tools {
                agent.register_tool(Box::new(McpTool::new(mcp.clone(), info)));
            }
            cx.mcp.push((n.to_string(), count));
            let msg = format!("mcp '{n}' trusted and started: {count} tool(s) registered as {n}_<tool>");
            let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
            Ok(msg)
        }
        ("untrust", n) => {
            let server = config
                .project_mcp
                .get(n)
                .ok_or_else(|| anyhow!("no MCP server '{n}' in the project config"))?;
            rift_core::untrust_mcp_entry(n, server)?;
            let running = cx.mcp.iter().any(|(an, _)| an == n);
            let msg = format!(
                "mcp '{n}' untrusted — it won't start next session{}",
                if running { " (still running in this one)" } else { "" }
            );
            let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
            Ok(msg)
        }
        // Connect a preconfigured/off-the-shelf stdio MCP server and persist
        // it: /mcp add [--global] <name> <command> [args…]. Registered live
        // (no restart) and written to the project .rift.json (default) or
        // the user config (--global).
        ("add", rest) => {
            let (global, rest) = match rest.strip_prefix("--global").or_else(|| rest.strip_prefix("-g")) {
                Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim()),
                _ => (false, rest),
            };
            let mut words = rest.split_whitespace();
            let (Some(name), Some(command)) = (words.next(), words.next()) else {
                bail!("usage: /mcp add [--global] <name> <command> [args…] — e.g. /mcp add fetch uvx mcp-server-fetch");
            };
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                bail!("server name '{name}' must be alphanumeric/dash/underscore (tools register as {name}_<tool>)");
            }
            if cx.mcp.iter().any(|(an, _)| an == name)
                || config.mcp.contains_key(name)
                || config.project_mcp.contains_key(name)
            {
                bail!("MCP server '{name}' already exists — /mcp lists servers, /config edit changes them");
            }
            let entry = rift_core::mcp::McpServerConfig {
                command: command.to_string(),
                args: words.map(str::to_string).collect(),
                env: Default::default(),
            };
            // Prove it works BEFORE persisting anything: spawn, handshake,
            // and list tools, bounded so a wedged command can't hang the TUI.
            let connect = async {
                let mcp = McpClient::spawn(name, &entry).await?;
                let tools = mcp.list_tools().await?;
                anyhow::Ok((mcp, tools))
            };
            let (mcp, tools) = tokio::time::timeout(std::time::Duration::from_secs(30), connect)
                .await
                .map_err(|_| anyhow!("MCP server '{name}' did not answer within 30s — is `{command}` right?"))??;
            if tools.is_empty() {
                bail!("MCP server '{name}' started but exposes no tools — not saving it");
            }
            let count = tools.len();
            let names: Vec<String> = tools.iter().map(|t| format!("{name}_{}", t.name)).collect();
            for info in tools {
                agent.register_tool(Box::new(McpTool::new(mcp.clone(), info)));
            }
            cx.mcp.push((name.to_string(), count));
            let path = rift_core::config::append_mcp_entry(global, &cx.cwd, name, &entry)?;
            if !global {
                // The user typed this entry themselves — that IS the consent
                // the project-config trust gate exists to collect.
                rift_core::trust_mcp_entry(name, &entry)?;
            }
            let msg = format!(
                "mcp '{name}' connected: {count} tool(s) — {}\nsaved to {} ({})",
                names.join(", "),
                path.display(),
                if global { "user-wide" } else { "this project, pre-trusted" }
            );
            let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
            Ok(format!("mcp '{name}': {count} tool(s) registered"))
        }
        (other, _) => bail!("usage: /mcp [add [--global] <name> <cmd> [args…] | trust|untrust <name>] — got '{other}'"),
    }
}

fn cmd_permissions(agent: &Agent, cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let user = agent.ctx().user_deny_patterns();
    let allow = agent.ctx().user_allow_patterns();
    let mut out = format!(
        "approval mode: {}\n\n",
        if agent.ctx().approval_enabled() {
            "ON — write/edit/bash ask first (/yolo stops asking)"
        } else {
            "off (YOLO) — write/edit/bash run without asking (/yolo off restores prompts)"
        },
    );
    out.push_str("allowed (run without an approval prompt; permissions.bash_allow, grown by 'always allow'):\n");
    if allow.is_empty() {
        out.push_str("  none yet — choose \"always allow '<pattern>'\" on an approval prompt to add one\n");
    } else {
        for pat in &allow {
            out.push_str(&format!("  {pat}\n"));
        }
    }
    out.push_str("\nbanned (always refused, even in YOLO mode):\n\nbuilt-in:\n");
    for pat in builtin_bash_deny() {
        out.push_str(&format!("  {pat}\n"));
    }
    if user.is_empty() {
        out.push_str("\nuser (permissions.bash_deny): none configured");
    } else {
        out.push_str("\nuser (permissions.bash_deny):\n");
        for pat in &user {
            out.push_str(&format!("  {pat}\n"));
        }
    }
    if let Some(p) = &cx.config_path {
        out.push_str(&format!("\nconfig: {}", p.display()));
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!(
        "{} allowed · {} builtin + {} user deny pattern(s)",
        allow.len(),
        builtin_bash_deny().len(),
        user.len()
    ))
}

async fn cmd_swarm(
    rest: &str,
    agent: &Agent,
    cx: &CmdCx,
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) -> Result<String> {
    // Parse: task words, optional `--models a,b` / `--judge model` / `--explore`.
    let mut task_words: Vec<&str> = vec![];
    let mut models = agent.cfg.model.clone();
    let mut explore = false;
    let mut judge: Option<String> = None;
    let mut words = rest.split_whitespace().peekable();
    while let Some(w) = words.next() {
        match w {
            "--models" => {
                models = words.next().ok_or_else(|| anyhow!("--models needs a value"))?.to_string();
            }
            "--judge" => {
                judge = Some(words.next().ok_or_else(|| anyhow!("--judge needs a model"))?.to_string());
            }
            "--explore" => explore = true,
            _ => task_words.push(w),
        }
    }
    let task = task_words.join(" ");
    if task.is_empty() {
        bail!("usage: /swarm <task> [--models a,b] [--judge model] [--explore]");
    }

    let swarm = Swarm::discover(&cx.cwd).await?;
    let mut candidates: Vec<Candidate> = models
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, m)| Candidate::from_model(m, i))
        .collect();
    if explore {
        let extra: Vec<Candidate> = candidates
            .iter()
            .map(|c| Candidate {
                name: format!("{}-hot", c.name),
                model: c.model.clone(),
                temperature: Some(0.8),
            })
            .collect();
        candidates.extend(extra);
    }
    let names: Vec<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let _ = fx.send(UiEffect::Out(
        Kind::Info,
        format!("WarpDrive: racing {} candidate(s) in isolated worktrees — Esc cancels\n  {}", names.len(), names.join("\n  ")),
    ));

    // Forward race progress (not content streams) into the activity log.
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<(usize, AgentEvent)>();
    let fx2 = fx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some((idx, ev)) = erx.recv().await {
            let effect = match ev {
                AgentEvent::Iteration(i) => Some((Kind::Info, format!("[{idx}] step {i}"))),
                AgentEvent::ToolStart { name, .. } => Some((Kind::Tool, format!("[{idx}] → {name}"))),
                AgentEvent::ToolResult { name, ok, .. } => Some((
                    if ok { Kind::Tool } else { Kind::ToolErr },
                    format!("[{idx}] {} {name}", if ok { '✓' } else { '✗' }),
                )),
                AgentEvent::Warning(w) => Some((Kind::Warn, format!("[{idx}] ! {w}"))),
                AgentEvent::Done(s) => Some((Kind::Info, format!("[{idx}] finished — {} steps", s.iterations))),
                _ => None,
            };
            if let Some((kind, text)) = effect {
                let _ = fx2.send(UiEffect::Log(kind, text));
            }
        }
    });

    let base_cfg = agent.cfg.clone();
    let factory = crate::provider_factory(&cx.host, &cx.providers);
    let outcomes = run_swarm(&factory, &base_cfg, &swarm, candidates, &task, etx, cancel).await;
    let _ = forwarder.await;

    let mut out = String::from("race results:\n");
    let mut mergeable = 0;
    for o in &outcomes {
        out.push_str(&format!("\n● {} ({})\n", o.candidate.name, o.candidate.model));
        if let Some(e) = &o.error {
            out.push_str(&format!("  failed: {e}\n"));
            continue;
        }
        if o.patch_path.is_some() {
            mergeable += 1;
            out.push_str(&format!("  changes: {}\n", o.diff_stat.trim().replace('\n', "\n  ")));
        } else {
            out.push_str(&format!("  {}\n", o.diff_stat.trim()));
        }
        let summary: String = o.summary.chars().take(300).collect();
        if !summary.is_empty() {
            out.push_str(&format!("  says: {}\n", summary.replace('\n', " ")));
        }
    }
    let mut verdict_note = String::new();
    if let Some(judge_model) = judge {
        match rift_core::judge_swarm(&factory, &judge_model, base_cfg.num_ctx, &task, &outcomes).await {
            Ok(v) => {
                out.push_str(&format!("\njudge ({judge_model}):\n{}\n", v.text.trim()));
                if let Some(w) = &v.winner {
                    verdict_note = format!(" — judge recommends {w}");
                }
            }
            Err(e) => out.push_str(&format!("\njudge failed: {e:#}\n")),
        }
    }
    if mergeable > 0 {
        out.push_str("\napply a winner with /merge <name> [--cleanup]");
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!("race done — {mergeable} candidate(s) produced changes{verdict_note}"))
}

async fn cmd_merge(rest: &str, cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let mut name = None;
    let mut cleanup = false;
    for w in rest.split_whitespace() {
        match w {
            "--cleanup" => cleanup = true,
            other => name = Some(other.to_string()),
        }
    }
    let name = name.ok_or_else(|| anyhow!("usage: /merge <candidate-name> [--cleanup]"))?;
    let swarm = Swarm::discover(&cx.cwd).await?;
    swarm.apply_patch(&name).await?;
    let mut msg = format!("applied patch '{name}' to {}", swarm.root().display());
    if cleanup {
        let n = swarm.cleanup_all().await?;
        msg.push_str(&format!(", removed {n} worktree(s)"));
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
    Ok(msg)
}

/// /rewind [n] — the checkpoint restore: files (via the edit journal) and
/// conversation truncate together, then the transcript reseeds and the
/// session file is rewritten so the rewind survives a /restart.
fn cmd_rewind(arg: &str, agent: &mut Agent, cx: &mut CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let n: usize = match arg.trim() {
        "" => 1,
        s => s.parse().map_err(|_| anyhow!("usage: /rewind [n] — got '{s}'"))?,
    };
    let restored = agent.rewind(n)?;
    let cwd = cx.cwd.display().to_string();
    cx.store.save(&agent.cfg.model, &cwd, &agent.messages)?;
    let _ = fx.send(UiEffect::Seed(agent.messages.clone()));
    let mut msg = format!("⏪ rewound {n} turn(s)");
    if restored.is_empty() {
        msg.push_str(" — no write/edit changes to restore");
    } else {
        msg.push_str(&format!(" — restored {} file(s):", restored.len()));
        for p in &restored {
            msg.push_str(&format!("\n  {}", p.display()));
        }
    }
    msg.push_str("\n(bash-made changes are outside the journal — check /diff if the turn ran scripts)");
    let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
    Ok(format!("rewound {n} turn(s), {} file(s) restored", restored.len()))
}

fn cmd_approve(arg: &str, agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    match arg {
        "on" => agent.ctx().set_approval(true),
        "off" => agent.ctx().set_approval(false),
        "" => {}
        other => bail!("usage: /approve [on|off] — got '{other}'"),
    }
    let state = if agent.ctx().approval_enabled() { "ON — write/edit/bash ask first" } else { "off" };
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("approval mode: {state}")));
    Ok(format!("approval: {state}"))
}

/// /yolo — approval prompts off; /yolo off — back to asking (with the
/// allow-list tracking). Sugar over the same switch /approve flips, named
/// for what it means.
fn cmd_yolo(arg: &str, agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    match arg {
        "" | "on" => {
            if agent.ctx().approval_enabled() {
                agent.ctx().set_approval(false);
                let _ = fx.send(UiEffect::Out(
                    Kind::Warn,
                    "YOLO mode ON — write/edit/bash run without asking. The deny list still applies. \
                     /yolo off restores approval prompts."
                        .into(),
                ));
            } else {
                let _ = fx.send(UiEffect::Out(Kind::Info, "already in YOLO mode — /yolo off restores approval prompts".into()));
            }
            Ok("YOLO: on".into())
        }
        "off" => {
            agent.ctx().set_approval(true);
            let _ = fx.send(UiEffect::Out(
                Kind::Info,
                format!(
                    "YOLO mode off — write/edit/bash ask first again ({} allowed pattern(s) skip the prompt)",
                    agent.ctx().user_allow_patterns().len()
                ),
            ));
            Ok("YOLO: off".into())
        }
        other => bail!("usage: /yolo [off] — got '{other}'"),
    }
}

const CONFIG_TEMPLATE: &str = "{\n  \"host\": \"http://localhost:11434\",\n  \"model\": \"gemma4:26b\",\n  \"mcp\": {},\n  \"permissions\": {\"bash_deny\": [], \"bash_allow\": []}\n}\n";

fn cmd_config(arg: &str, agent: &Agent, cx: &mut CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    match arg {
        "" => {
            let mut out = String::new();
            match &cx.config_path {
                Some(p) => {
                    out.push_str(&format!("config: {}\n\n", p.display()));
                    let body = std::fs::read_to_string(p).unwrap_or_else(|e| format!("(unreadable: {e})"));
                    let capped: String = body.chars().take(1500).collect();
                    out.push_str(capped.trim_end());
                    if capped.len() < body.len() {
                        out.push_str("\n[truncated]");
                    }
                }
                None => out.push_str(
                    "no config file found — checked .rift.json (project) and ~/.config/rift/config.json (user)\ncreate one with /config edit",
                ),
            }
            out.push_str(&format!(
                "\n\nruntime: approval {}, {} user deny pattern(s)\nedit with /config edit (permissions hot-reload; MCP changes need a restart)",
                if agent.ctx().approval_enabled() { "ON" } else { "off" },
                agent.ctx().user_deny_patterns().len(),
            ));
            let _ = fx.send(UiEffect::Out(Kind::Info, out.trim().into()));
            Ok("config shown".into())
        }
        "edit" => {
            let path = cx.config_path.clone().unwrap_or_else(|| cx.cwd.join(".rift.json"));
            if !path.exists() {
                std::fs::write(&path, CONFIG_TEMPLATE).with_context(|| format!("creating {}", path.display()))?;
                let _ = fx.send(UiEffect::Out(Kind::Info, format!("created {}", path.display())));
            }
            let _ = fx.send(UiEffect::EditFile(path));
            Ok("opening config in $EDITOR…".into())
        }
        // Internal: dispatched by the UI after $EDITOR exits.
        "reload" => {
            let loaded = rift_core::Config::load(&cx.cwd)?;
            for w in &loaded.warnings {
                let _ = fx.send(UiEffect::Out(Kind::Warn, format!("! {w}")));
            }
            let config = loaded.config;
            agent.ctx().set_deny(&config.permissions.bash_deny);
            agent.ctx().set_allow(&config.permissions.bash_allow);
            agent.ctx().set_approval(config.permissions.approve_effective());
            // Hooks: only already-trusted project entries reload here (the
            // trust prompt is a startup interaction); new ones need /restart.
            let mut post_edit = config.hooks.post_edit.clone();
            post_edit.extend(
                config
                    .project_hooks
                    .post_edit
                    .iter()
                    .filter(|h| rift_core::config::hook_trusted(h))
                    .cloned(),
            );
            agent.ctx().set_post_edit_hooks(&post_edit);
            cx.config_path = loaded.paths.last().cloned();
            let msg = format!(
                "config reloaded — approval {}, {} allowed / {} user deny pattern(s) (host/model/MCP changes need a restart)",
                if config.permissions.approve_effective() { "ON" } else { "off" },
                config.permissions.bash_allow.len(),
                config.permissions.bash_deny.len(),
            );
            let _ = fx.send(UiEffect::Out(Kind::Info, msg.clone()));
            Ok(msg)
        }
        other => bail!("usage: /config [edit] — got '{other}'"),
    }
}

fn cmd_plan(arg: &str, agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    match arg {
        "clear" => {
            agent.ctx().clear_plan();
            let _ = fx.send(UiEffect::Plan(vec![]));
            let _ = fx.send(UiEffect::Out(Kind::Info, "plan cleared".into()));
            Ok("plan cleared".into())
        }
        "" => {
            let items = agent.ctx().plan_snapshot();
            if items.is_empty() {
                bail!("no plan yet — the agent sets one with its plan tool on multi-step tasks");
            }
            let done = items.iter().filter(|i| i.done).count();
            let mut out = format!("plan ({done}/{} done):\n", items.len());
            for item in &items {
                out.push_str(&format!("  {} {}\n", if item.done { "☑" } else { "☐" }, item.text));
            }
            let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
            Ok(format!("{done}/{} done", items.len()))
        }
        other => bail!("usage: /plan [clear] — got '{other}'"),
    }
}

async fn cmd_copy(arg: &str, agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let text = match arg {
        "" => agent
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
            .ok_or_else(|| anyhow!("nothing to copy yet — no assistant reply in this session"))?,
        "all" => {
            let mut out = String::new();
            for m in &agent.messages {
                match m.role {
                    Role::User if !m.content.starts_with("[system]") => {
                        out.push_str(&format!("USER: {}\n\n", m.content));
                    }
                    Role::Assistant if !m.content.is_empty() => {
                        out.push_str(&format!("ASSISTANT: {}\n\n", m.content));
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                bail!("nothing to copy yet");
            }
            out
        }
        other => bail!("usage: /copy [all|log] — got '{other}'"),
    };
    let chars = text.chars().count();
    let status = match crate::clipboard::copy_via_tool(&text).await {
        Some(tool) => format!("copied {chars} chars to the clipboard (via {tool})"),
        None => {
            // No clipboard tool — ask the terminal itself (OSC 52).
            let _ = fx.send(UiEffect::Osc52(text));
            format!("sent {chars} chars to the terminal clipboard (OSC 52)")
        }
    };
    let _ = fx.send(UiEffect::Out(Kind::Info, status.clone()));
    Ok(status)
}

async fn cmd_update(fx: &UnboundedSender<UiEffect>, cancel: &CancellationToken) -> Result<String> {
    let _ = fx.send(UiEffect::Out(Kind::Info, "checking for the latest release…".into()));
    let msg = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = crate::update::self_update(env!("CARGO_PKG_VERSION")) => r?,
    };
    let note = if msg.starts_with("updated") {
        "\nrun /restart to load the new version — your chat resumes automatically"
    } else {
        ""
    };
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("{msg}{note}")));
    Ok(msg)
}

fn cmd_undo(agent: &Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let restored = agent.ctx().undo_last_turn()?;
    if restored.is_empty() {
        bail!("nothing to undo (only write/edit tool changes are tracked)");
    }
    let mut out = String::from("restored to pre-turn state:\n");
    for p in &restored {
        out.push_str(&format!("  {}\n", p.display()));
    }
    out.push_str("(note: changes made via bash are not tracked)");
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!("undid {} file(s)", restored.len()))
}

async fn cmd_diff(cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let run = |args: &'static [&'static str]| {
        let cwd = cx.cwd.clone();
        async move {
            let out = tokio::process::Command::new("git").args(args).current_dir(&cwd).output().await?;
            anyhow::Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()))
        }
    };
    // HEAD form shows staged + unstaged; fall back for repos with no commits.
    let (ok, text) = run(&["diff", "HEAD"]).await?;
    let text = if ok { text } else { run(&["diff"]).await?.1 };
    if text.trim().is_empty() {
        let _ = fx.send(UiEffect::Out(Kind::Info, "working tree clean".into()));
        return Ok("no changes".into());
    }
    let lines = text.lines().count();
    let _ = fx.send(UiEffect::Diff(text));
    Ok(format!("diff: {lines} lines"))
}

async fn cmd_host(
    arg: &str,
    agent: &mut Agent,
    cx: &mut CmdCx,
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) -> Result<String> {
    if arg.is_empty() {
        let _ = fx.send(UiEffect::Out(
            Kind::Info,
            format!(
                "server: {}\nswitch with /host <url> — Ollama and OpenAI-compatible (vLLM, LM Studio, \
                 llama.cpp, …) servers are auto-detected",
                agent.client().base_url()
            ),
        ));
        return Ok("host shown".into());
    }
    // Autodetect the server type: probe the model list on both protocols,
    // trying first whichever the URL shape suggests (…/v1 = OpenAI-style).
    // Ad-hoc hosts get no API key — keyed endpoints belong in `providers`.
    let looks_openai = arg.trim_end_matches('/').ends_with("/v1") || arg.contains("/v1/");
    let candidates: Vec<(&str, Arc<dyn Provider>)> = if looks_openai {
        vec![
            ("openai-compatible", Arc::new(OpenAiClient::new(arg, None))),
            ("ollama", Arc::new(OllamaClient::new(arg))),
        ]
    } else {
        vec![
            ("ollama", Arc::new(OllamaClient::new(arg))),
            ("openai-compatible", Arc::new(OpenAiClient::new(arg, None))),
        ]
    };
    let mut errors: Vec<String> = vec![];
    for (kind, candidate) in candidates {
        let models = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("cancelled"),
            r = candidate.tags() => r,
        };
        let models = match models {
            Ok(m) if !m.is_empty() => m,
            Ok(_) => {
                errors.push(format!("{kind} at {}: reachable but lists no models", candidate.base_url()));
                continue;
            }
            Err(e) => {
                errors.push(format!("{kind} at {}: {e:#}", candidate.base_url()));
                continue;
            }
        };
        let url = candidate.base_url().to_string();
        let has_model = models.iter().any(|m| m.name == agent.cfg.model);
        agent.set_client(candidate);
        // Keep the default host in sync so a later bare `/model <name>`
        // rebuilds against this server with the right protocol —
        // build_provider routes …/v1 hosts through the OpenAI client.
        cx.host = url.clone();
        // And the UI's copy, so /restart relaunches against this server.
        let _ = fx.send(UiEffect::Host(url.clone()));
        let mut msg = format!("switched to {url} ({kind}, {} model(s))", models.len());
        if !has_model {
            msg.push_str(&format!(
                "\n! current model '{}' not found there — pick one with /model",
                agent.cfg.model
            ));
        }
        let _ = fx.send(UiEffect::Out(if has_model { Kind::Info } else { Kind::Warn }, msg));
        return Ok(format!("host: {url}"));
    }
    bail!("no model server answered at {arg}:\n  {}", errors.join("\n  "))
}

async fn cmd_think(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    /// Render the (mode, effort) pair the way /think reports it.
    fn describe(cfg: &rift_core::AgentConfig) -> String {
        let mode = match cfg.think {
            None => "auto (server default)",
            Some(true) => "on",
            Some(false) => "off",
        };
        match &cfg.effort {
            Some(e) => format!("{mode}, effort {e}"),
            None => format!("{mode}, effort auto"),
        }
    }
    let state = match arg {
        "" => {
            let _ = fx.send(UiEffect::Out(
                Kind::Info,
                format!(
                    "thinking: {}\nset with /think on|off|auto or an effort level: /think {}\n\
                     (levels imply thinking on; servers with fewer grades map between them — \
                     DeepSeek treats low/medium as high and xhigh as max)",
                    describe(&agent.cfg),
                    rift_core::EFFORT_LEVELS.join("|"),
                ),
            ));
            return Ok(format!("thinking: {}", describe(&agent.cfg)));
        }
        "on" => {
            let show = agent.client().show(&agent.cfg.model).await?;
            if !show.supports("thinking") {
                bail!("model '{}' does not have the 'thinking' capability", agent.cfg.model);
            }
            agent.cfg.think = Some(true);
            describe(&agent.cfg)
        }
        "off" => {
            agent.cfg.think = Some(false);
            agent.cfg.effort = None;
            describe(&agent.cfg)
        }
        "auto" => {
            let show = agent.client().show(&agent.cfg.model).await?;
            agent.cfg.think = if show.supports("thinking") { None } else { Some(false) };
            agent.cfg.effort = None;
            describe(&agent.cfg)
        }
        level if rift_core::EFFORT_LEVELS.contains(&level) => {
            let show = agent.client().show(&agent.cfg.model).await?;
            if !show.supports("thinking") {
                bail!("model '{}' does not have the 'thinking' capability", agent.cfg.model);
            }
            agent.cfg.think = Some(true);
            agent.cfg.effort = Some(level.to_string());
            describe(&agent.cfg)
        }
        other => bail!(
            "unknown value '{other}' — use on, off, auto, or an effort level ({})",
            rift_core::EFFORT_LEVELS.join(", ")
        ),
    };
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("thinking: {state}")));
    Ok(format!("thinking: {state}"))
}

fn cmd_export(agent: &Agent, cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = cx.cwd.join(format!("rift-export-{stamp}.md"));
    let mut out = format!("# Rift transcript — {}\n", agent.cfg.model);
    for m in &agent.messages {
        match m.role {
            Role::System => {}
            Role::User => {
                if !m.content.starts_with("[system]") {
                    out.push_str(&format!("\n## User\n\n{}\n", m.content));
                }
            }
            Role::Assistant => {
                if !m.content.is_empty() {
                    out.push_str(&format!("\n## Assistant\n\n{}\n", m.content));
                }
                for tc in &m.tool_calls {
                    out.push_str(&format!(
                        "\n> tool call: `{}` `{}`\n",
                        tc.function.name,
                        serde_json::to_string(&tc.function.arguments).unwrap_or_default()
                    ));
                }
            }
            Role::Tool => {
                let name = m.tool_name.as_deref().unwrap_or("?");
                let preview: String = m.content.chars().take(600).collect();
                out.push_str(&format!("\n> `{name}` returned:\n>\n> ```\n{}\n> ```\n", preview.trim_end()));
            }
        }
    }
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    let _ = fx.send(UiEffect::Out(Kind::Info, format!("exported to {}", path.display())));
    Ok(format!("exported {}", path.display()))
}
