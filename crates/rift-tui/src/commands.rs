//! Slash commands: lines starting with `/` are intercepted by the TUI and
//! handled here instead of being sent to the model. Commands run inside the
//! agent task (which owns the `Agent`); output flows back to the UI as
//! `UiEffect`s. Esc cancels long-running commands (`/compact`, `/swarm`)
//! through the same CancellationToken as normal turns.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rift_core::{builtin_bash_deny, compact, run_swarm, Agent, AgentEvent, Candidate, SessionStore, Swarm};
use rift_ollama::{Message, OllamaClient, Role};
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
    /// Replace the pinned plan checklist (e.g. /plan clear).
    Plan(Vec<rift_core::PlanItem>),
    /// Suspend the TUI, open the file in $EDITOR, then hot-reload config.
    EditFile(PathBuf),
    /// Ask the terminal to set the clipboard (OSC 52) — emitted by the UI
    /// loop, which owns stdout.
    Osc52(String),
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
}

/// (name, argument hint, one-line description) — the single source of truth
/// driving both `/help` and the input popup palette.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/approve", "[on|off]", "toggle approval mode for write/edit/bash"),
    ("/clear", "", "wipe the conversation (keeps the session file)"),
    ("/config", "[edit]", "show or edit .rift.json (hot-reloads permissions)"),
    ("/compact", "", "force history compaction now"),
    ("/copy", "[all|log]", "copy last reply / whole transcript / activity log"),
    ("/diff", "", "git diff of the working tree"),
    ("/export", "", "save the transcript as markdown"),
    ("/help", "", "list commands and keys"),
    ("/host", "[url]", "show or switch the Ollama server"),
    ("/init", "", "generate a RIFT.md project guide"),
    ("/mcp", "", "connected MCP servers"),
    ("/merge", "<name> [--cleanup]", "apply a swarm candidate's patch"),
    ("/model", "[name]", "list models on the server, or switch model"),
    ("/permissions", "", "active shell deny patterns"),
    ("/plan", "[clear]", "show or clear the agent's task checklist"),
    ("/sessions", "[n]", "list saved sessions, or resume the nth"),
    ("/save", "<name>", "name this session (keeps autosaving to it)"),
    ("/skills", "", "list available skills (run with /skill:<name>)"),
    ("/swarm", "<task> [--models a,b]", "WarpDrive race in isolated worktrees"),
    ("/worktrees", "", "list swarm worktrees + patches"),
    ("/think", "[on|off|auto]", "thinking mode (capability-checked)"),
    ("/tokens", "", "context budget, usage estimate, calibration"),
    ("/stats", "", "session totals: turns, tokens, tools, compactions"),
    ("/system", "[text]", "show or override the system prompt"),
    ("/temp", "<0.0-2.0>", "set sampling temperature"),
    ("/ctx", "<n>", "set context window (num_ctx)"),
    ("/retry", "", "re-run the last prompt"),
    ("/quit", "", "exit rift"),
    ("/tools", "", "tools the model can call (builtin + MCP)"),
    ("/undo", "", "revert last turn's write/edit changes"),
    ("/update", "", "update rift to the latest release"),
];

fn help_text() -> String {
    let mut out = String::from("commands:\n");
    for (name, args, desc) in COMMANDS {
        let left = if args.is_empty() { (*name).to_string() } else { format!("{name} {args}") };
        out.push_str(&format!("  {left:<30}{desc}\n"));
    }
    out.push_str(
        "\nkeys: Enter send · Ctrl+J newline · Tab focus · Ctrl+L log · Ctrl+T toggle mouse capture \
         (off = select/copy text natively) · Esc cancel · Ctrl+C quit",
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
        "/config" => cmd_config(rest, agent, cx, fx),
        "/model" => cmd_model(rest, agent, fx).await,
        "/clear" => cmd_clear(agent, cx, fx),
        "/compact" => cmd_compact(agent, fx, cancel).await,
        "/copy" => cmd_copy(rest, agent, fx).await,
        "/tokens" => cmd_tokens(agent, fx),
        "/sessions" => cmd_sessions(rest, agent, cx, fx),
        "/tools" => cmd_tools(agent, fx),
        "/mcp" => cmd_mcp(cx, fx),
        "/permissions" => cmd_permissions(agent, cx, fx),
        "/plan" => cmd_plan(rest, agent, fx),
        "/swarm" => cmd_swarm(rest, agent, cx, fx, cancel).await,
        "/merge" => cmd_merge(rest, cx, fx).await,
        "/undo" => cmd_undo(agent, fx),
        "/update" => cmd_update(fx, cancel).await,
        "/diff" => cmd_diff(cx, fx).await,
        "/host" => cmd_host(rest, agent, fx, cancel).await,
        "/think" => cmd_think(rest, agent, fx).await,
        "/export" => cmd_export(agent, cx, fx),
        "/system" => cmd_system(rest, agent, fx),
        "/temp" => cmd_temp(rest, agent, fx),
        "/ctx" => cmd_ctx(rest, agent, fx),
        "/save" => cmd_save(rest, agent, cx, fx),
        "/worktrees" => cmd_worktrees(cx, fx).await,
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

async fn cmd_model(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    if arg.is_empty() {
        let models = agent.client().tags().await.context("listing models")?;
        if models.is_empty() {
            bail!("no models installed on {}", agent.client().base_url());
        }
        let count = models.len();
        let items = models
            .into_iter()
            .map(|m| {
                let detail = if m.name == agent.cfg.model {
                    "current".to_string()
                } else {
                    m.capabilities.join(", ")
                };
                PickerItem { value: m.name.clone(), label: m.name, detail }
            })
            .collect();
        let _ = fx.send(UiEffect::Picker {
            title: format!("select model — {}", agent.client().base_url()),
            items,
            template: "/model {}".into(),
        });
        return Ok(format!("{count} model(s) — ↑↓ select, Enter switch, Esc cancel"));
    }

    // Same preflight as startup: capability check + num_ctx clamp.
    let show = agent
        .client()
        .show(arg)
        .await
        .map_err(|e| anyhow!("model '{arg}' not usable: {e}"))?;
    if !show.supports("tools") {
        bail!("model '{arg}' does not have the 'tools' capability");
    }
    agent.cfg.think = if show.supports("thinking") { None } else { Some(false) };
    let mut note = String::new();
    if let Some(max) = show.context_length() {
        if agent.cfg.num_ctx > max {
            note = format!(" (num_ctx clamped {} → {max})", agent.cfg.num_ctx);
            agent.cfg.num_ctx = agent.cfg.num_ctx.min(max);
        }
    }
    agent.cfg.model = arg.to_string();
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
        r = compact::summarize_history(agent.client(), &agent.cfg.model, agent.cfg.num_ctx, &agent.messages) => r?,
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

fn cmd_mcp(cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let mut out = String::new();
    match &cx.config_path {
        Some(p) => out.push_str(&format!("config: {}\n", p.display())),
        None => out.push_str("config: none found (.rift.json or ~/.config/rift/config.json)\n"),
    }
    if cx.mcp.is_empty() {
        out.push_str("no MCP servers connected");
    } else {
        out.push_str("MCP servers:\n");
        for (name, count) in &cx.mcp {
            out.push_str(&format!("  {name}: {count} tool(s), exposed as {name}_<tool>\n"));
        }
        out.push_str("(config changes need a restart)");
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!("{} server(s)", cx.mcp.len()))
}

fn cmd_permissions(agent: &Agent, cx: &CmdCx, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let user = agent.ctx().user_deny_patterns();
    let mut out = format!(
        "approval mode: {} (write/edit/bash {}; --approve or permissions.approve in config)\n\n",
        if agent.ctx().approval_enabled() { "ON" } else { "off" },
        if agent.ctx().approval_enabled() { "ask before running" } else { "run without asking" },
    );
    out.push_str("shell commands matching these patterns are refused:\n\nbuilt-in (always on):\n");
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
    Ok(format!("{} builtin + {} user pattern(s)", builtin_bash_deny().len(), user.len()))
}

async fn cmd_swarm(
    rest: &str,
    agent: &Agent,
    cx: &CmdCx,
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) -> Result<String> {
    // Parse: task words, optional `--models a,b`, optional `--explore`.
    let mut task_words: Vec<&str> = vec![];
    let mut models = agent.cfg.model.clone();
    let mut explore = false;
    let mut words = rest.split_whitespace().peekable();
    while let Some(w) = words.next() {
        match w {
            "--models" => {
                models = words.next().ok_or_else(|| anyhow!("--models needs a value"))?.to_string();
            }
            "--explore" => explore = true,
            _ => task_words.push(w),
        }
    }
    let task = task_words.join(" ");
    if task.is_empty() {
        bail!("usage: /swarm <task> [--models a,b] [--explore]");
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
    let outcomes = run_swarm(agent.client(), &base_cfg, &swarm, candidates, &task, etx, cancel).await;
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
    if mergeable > 0 {
        out.push_str("\napply a winner with /merge <name> [--cleanup]");
    }
    let _ = fx.send(UiEffect::Out(Kind::Info, out.trim_end().into()));
    Ok(format!("race done — {mergeable} candidate(s) produced changes"))
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

const CONFIG_TEMPLATE: &str = "{\n  \"host\": \"http://localhost:11434\",\n  \"model\": \"gemma4:26b\",\n  \"mcp\": {},\n  \"permissions\": {\"bash_deny\": [], \"approve\": false}\n}\n";

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
            let (config, path) = rift_core::Config::load(&cx.cwd)?;
            agent.ctx().set_deny(&config.permissions.bash_deny);
            agent.ctx().set_approval(config.permissions.approve);
            cx.config_path = path;
            let msg = format!(
                "config reloaded — approval {}, {} user deny pattern(s) (host/model/MCP changes need a restart)",
                if config.permissions.approve { "ON" } else { "off" },
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
    let note = if msg.starts_with("updated") { "\nrestart rift to use the new version" } else { "" };
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
    fx: &UnboundedSender<UiEffect>,
    cancel: &CancellationToken,
) -> Result<String> {
    if arg.is_empty() {
        let _ = fx.send(UiEffect::Out(
            Kind::Info,
            format!("Ollama server: {}\nswitch with /host <url>", agent.client().base_url()),
        ));
        return Ok("host shown".into());
    }
    let candidate = OllamaClient::new(arg);
    let models = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = candidate.tags() => r.map_err(|e| anyhow!("cannot reach {}: {e}", candidate.base_url()))?,
    };
    let url = candidate.base_url().to_string();
    let has_model = models.iter().any(|m| m.name == agent.cfg.model);
    agent.set_client(candidate);
    let mut msg = format!("switched to {url} ({} model(s))", models.len());
    if !has_model {
        msg.push_str(&format!("\n! current model '{}' not found there — pick one with /model", agent.cfg.model));
    }
    let _ = fx.send(UiEffect::Out(if has_model { Kind::Info } else { Kind::Warn }, msg));
    Ok(format!("host: {url}"))
}

async fn cmd_think(arg: &str, agent: &mut Agent, fx: &UnboundedSender<UiEffect>) -> Result<String> {
    let state = match arg {
        "" => {
            let now = match agent.cfg.think {
                None => "auto (server default)",
                Some(true) => "on",
                Some(false) => "off",
            };
            let _ = fx.send(UiEffect::Out(Kind::Info, format!("thinking: {now}\nset with /think on|off|auto")));
            return Ok(format!("thinking: {now}"));
        }
        "on" => {
            let show = agent.client().show(&agent.cfg.model).await?;
            if !show.supports("thinking") {
                bail!("model '{}' does not have the 'thinking' capability", agent.cfg.model);
            }
            agent.cfg.think = Some(true);
            "on"
        }
        "off" => {
            agent.cfg.think = Some(false);
            "off"
        }
        "auto" => {
            let show = agent.client().show(&agent.cfg.model).await?;
            agent.cfg.think = if show.supports("thinking") { None } else { Some(false) };
            "auto (server default)"
        }
        other => bail!("unknown value '{other}' — use on, off, or auto"),
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
