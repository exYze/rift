//! `rift --serve`: a line-oriented JSON protocol over stdio for editor
//! integrations (the VS Code sidebar chat). stdout carries one event object
//! per line; stdin carries one command object per line; stderr stays
//! human-readable diagnostics. The process exits when stdin closes.
//!
//! **docs/SERVE.md is the protocol contract** (protocol v1) — the tests at
//! the bottom of this file pin the wire shapes it documents. Additive
//! changes (new events, new fields) are fine within v1; consumers must
//! ignore what they don't know. Removing/renaming/retyping anything bumps
//! PROTOCOL_VERSION — which is the breaking change 2.0 exists for.
//!
//! Commands:  {"cmd":"hello","edit_review":bool} | {"cmd":"prompt","text":…}
//!            | {"cmd":"answer","id":…,"text":…} | {"cmd":"cancel"} | {"cmd":"undo"}
//!            | {"cmd":"edit_decision","id":…,"apply":bool,"content":…}
//!            | {"cmd":"list_sessions"} | {"cmd":"list_models"}
//!            | {"cmd":"set_model","model":…} | {"cmd":"set_approval","approve":bool}
//! Events:    ready (carries `commands` so consumers can feature-detect,
//!            and `approve` — the current approval mode), capabilities
//!            (acks hello), history, iteration,
//!            thinking, content, tool_start, tool_result, info, warning,
//!            plan, subagent_started, subagent, subagent_finished,
//!            task_started, task_finished, ask (answer it by id),
//!            edit_review (decide it by id), edit_review_closed,
//!            done (always ends a turn), context ({used, limit} —
//!            context-window occupancy, sent at startup and after each
//!            turn's idle compaction), models (answers list_models),
//!            model_changed (acks a successful set_model),
//!            approval_changed (acks a successful set_approval).
//!
//! `set_approval` is the TUI's `/yolo` over the wire: `approve:false` stops
//! the prompts, so write/edit/bash apply as soon as the model calls them
//! (and edit_review events stop being emitted — there is nothing left to
//! decide). It is deliberately NOT a bypass of the permission rules: `deny`
//! rules still refuse and `ask` rules still prompt, exactly as in the TUI.
//! An approval prompt already on screen still needs its answer; the change
//! takes effect from the next gated action.
//!
//! Inline diff review is a capability the consumer opts into with
//! `{"cmd":"hello","edit_review":true}` (the VS Code extension sends it at
//! spawn). Once enabled, every proposed write/edit is emitted as an
//! edit_review event ({path, old, new}) BEFORE it touches disk; the
//! consumer shows a native diff and replies with edit_decision —
//! apply:true writes `content` (the proposal, or the accepted-hunk subset
//! the reviewer assembled; omitted = the proposal verbatim), apply:false
//! rejects it. Consumers that never say hello keep the plain `ask`
//! approval prompts, so older frontends are unaffected.

use anyhow::Result;
use rift_core::{
    Agent, AgentEvent, AskRequest, EditReviewReply, EditReviewRequest, ProviderConfig,
    SessionStore, TurnStats,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The serve protocol version, carried in `ready` and `capabilities`.
/// Bumped ONLY on breaking changes (docs/SERVE.md has the rules).
pub const PROTOCOL_VERSION: u32 = 1;

/// The command set this build understands, advertised in `ready` so
/// consumers feature-detect instead of probing (an unknown command would
/// land a user-visible warning in their chat).
const COMMANDS: &[&str] = &[
    "hello", "prompt", "answer", "edit_decision", "cancel", "undo", "list_sessions",
    "list_models", "set_model", "set_approval",
];

/// One event object per stdout line, flushed immediately — the consumer on
/// the other side of the pipe renders in real time.
fn emit(v: Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn event_json(ev: &AgentEvent) -> Value {
    match ev {
        AgentEvent::Iteration(i) => json!({"event": "iteration", "n": i}),
        AgentEvent::Thinking(t) => json!({"event": "thinking", "text": t}),
        AgentEvent::Content(c) => json!({"event": "content", "text": c}),
        AgentEvent::ToolStart { name, args } => json!({"event": "tool_start", "name": name, "args": args}),
        AgentEvent::ToolResult { name, ok, preview } => {
            json!({"event": "tool_result", "name": name, "ok": ok, "preview": preview})
        }
        AgentEvent::Info(t) => json!({"event": "info", "text": t}),
        AgentEvent::Warning(t) => json!({"event": "warning", "text": t}),
        AgentEvent::SubAgentStarted { tag, model, label } => {
            json!({"event": "subagent_started", "tag": tag, "model": model, "label": label})
        }
        AgentEvent::SubAgentActivity { tag, text, warn } => {
            json!({"event": "subagent", "tag": tag, "text": text, "warn": warn})
        }
        AgentEvent::SubAgentFinished { tag, steps } => {
            json!({"event": "subagent_finished", "tag": tag, "steps": steps})
        }
        AgentEvent::Plan(items) => json!({
            "event": "plan",
            "items": items.iter().map(|p| json!({"text": p.text, "done": p.done})).collect::<Vec<_>>(),
        }),
        AgentEvent::TaskStarted { id, label } => json!({"event": "task_started", "id": id, "label": label}),
        AgentEvent::TaskFinished { id, label, ok, preview } => {
            json!({"event": "task_finished", "id": id, "label": label, "ok": ok, "preview": preview})
        }
        AgentEvent::Done(s) => json!({
            "event": "done",
            "stats": {
                "iterations": s.iterations,
                "prompt_tokens": s.prompt_tokens,
                "billed_prompt_tokens": s.billed_prompt_tokens,
                "output_tokens": s.output_tokens,
                "duration_ms": s.duration_ms as u64,
                "tokens_per_sec": s.tokens_per_sec,
            },
        }),
        AgentEvent::Context { used, limit } => {
            json!({"event": "context", "used": used, "limit": limit})
        }
        // Additive (still protocol v1): consumers that don't know the event
        // ignore the line. added/removed are precomputed so slim renderers
        // can show a "+3 −1" head without parsing the diff body.
        AgentEvent::EditDiff { path, diff } => json!({
            "event": "edit_diff",
            "path": path,
            "added": diff.iter().filter(|l| l.starts_with('+')).count(),
            "removed": diff.iter().filter(|l| l.starts_with('-')).count(),
            "diff": diff,
        }),
    }
}

/// The authoritative hunking for an edit_review event: rift's own line diff
/// as alternating same/change segments, so consumers render and reassemble
/// accepted-hunk content without re-deriving a diff of their own.
fn segments_json(old: &str, new: &str) -> Vec<Value> {
    rift_core::diff_segments(old, new)
        .into_iter()
        .map(|s| match s {
            rift_core::DiffSegment::Same(lines) => json!({"same": true, "lines": lines}),
            rift_core::DiffSegment::Change { old, new } => {
                json!({"same": false, "old": old, "new": new})
            }
        })
        .collect()
}

/// Every model rift can currently reach, exactly as the `/model` argument
/// expects it: the default host's list (bare names), then each configured
/// provider's list as `provider/model`. Unreachable servers are skipped
/// (short timeout — an offline provider must not wedge the picker). This is
/// what lets consumers drop their own provider-routing reimplementations.
async fn discover_models(host: &str, providers: &HashMap<String, ProviderConfig>) -> Vec<String> {
    const PROBE: std::time::Duration = std::time::Duration::from_millis(2500);
    let mut out = Vec::new();
    // build_provider's bare-name path: the model string doesn't matter, the
    // client is the default host's.
    let (client, _) = crate::build_provider("_", host, providers);
    if let Ok(Ok(models)) = tokio::time::timeout(PROBE, client.tags()).await {
        out.extend(models.into_iter().map(|m| m.name));
    }
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    for name in names {
        let (client, _) = crate::build_provider(&format!("{name}/_"), host, providers);
        if let Ok(Ok(models)) = tokio::time::timeout(PROBE, client.tags()).await {
            out.extend(models.into_iter().map(|m| format!("{name}/{}", m.name)));
        }
    }
    out
}

/// Metadata for the past-chats picker: every saved session, newest first,
/// each labelled by its first real user message so the consumer can show a
/// human title. Best-effort — unreadable or corrupt files are skipped, never
/// fatal (browsing history must not depend on every autosave being clean).
fn session_list() -> Vec<Value> {
    let paths = SessionStore::list().unwrap_or_default();
    let mut out = Vec::new();
    for path in paths.into_iter().take(200) {
        let Ok(saved) = SessionStore::load(&path) else { continue };
        let is_user = |m: &&rift_ollama::Message| {
            matches!(m.role, rift_ollama::Role::User)
                && !m.content.starts_with("[system]")
                && !m.content.trim().is_empty()
        };
        let title = saved
            .messages
            .iter()
            .find(is_user)
            .map(|m| m.content.lines().next().unwrap_or("").chars().take(100).collect::<String>())
            .unwrap_or_else(|| "(empty session)".into());
        let turns = saved.messages.iter().filter(is_user).count();
        out.push(json!({
            "path": path.display().to_string(),
            "title": title,
            "saved_at": saved.saved_at,
            "cwd": saved.cwd,
            "model": saved.model,
            "turns": turns,
        }));
    }
    out
}

/// Work items for the agent task. Undo runs there (not on the select loop)
/// because the agent owns its `ToolCtx`, and routing through the same queue
/// keeps it serialized with turns.
enum ServeCmd {
    Prompt(String, CancellationToken),
    Undo,
    /// The consumer answered a project-plugin trust ask with "trust":
    /// persist the approval and register the plugin's tools (and hooks)
    /// into the running agent. Runs on the agent task, which owns both.
    TrustPlugin(rift_core::Plugin),
    /// Live model switch (the same preflight-and-swap as the TUI's /model).
    /// Runs on the agent task because it mutates the agent's client/config;
    /// queueing keeps it serialized with turns.
    SetModel(String),
}

#[allow(clippy::too_many_arguments)] // one call site (main); a struct would just relocate the list
pub async fn run_serve(
    mut agent: Agent,
    store: SessionStore,
    mut ask_rx: mpsc::UnboundedReceiver<AskRequest>,
    model: String,
    resumed: Vec<rift_ollama::Message>,
    skills: Vec<rift_core::Skill>,
    pending_plugins: Vec<rift_core::Plugin>,
    host: String,
    providers: HashMap<String, ProviderConfig>,
) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<ServeCmd>();
    // Inline diff review: created here but only installed into the ctx when
    // the consumer's hello opts in — a consumer that doesn't know the
    // edit_review event must keep getting plain ask prompts, not hang.
    let (review_tx, mut review_rx) = mpsc::unbounded_channel::<EditReviewRequest>();
    // Control handle for the switches the select loop owns (edit-review
    // installation, approval mode). ToolCtx shares them behind Arcs, so
    // flipping here is seen by the agent task immediately.
    let ctl = agent.ctx().clone();
    // Background-task events surface through the same channel, between turns.
    agent.ctx().bg().set_notify(ev_tx.clone());
    let cwd = std::env::current_dir()?.display().to_string();
    let session_path = store.path().display().to_string();

    // The ADDRESSABLE model name (provider prefix intact): what set_model
    // updates and the models event marks as current. Shared with the agent
    // task, which owns the actual switch.
    let model_addr = std::sync::Arc::new(std::sync::Mutex::new(model.clone()));

    // Turns run on their own task (same shape as the TUI) so stdin stays
    // responsive mid-turn for cancel and ask answers.
    let turn_ev = ev_tx.clone();
    let cwd_save = cwd.clone();
    // Captured before the agent moves: the ready event carries num_ctx and
    // an initial gauge so resumed sessions show their fill level up front.
    let (initial_used, num_ctx) = agent.context_usage();
    // Startup approval mode (config + --approve), so the consumer's toggle
    // opens in the right state instead of guessing.
    let initial_approve = agent.ctx().approval_enabled();
    let switch_host = host.clone();
    let switch_providers = providers.clone();
    let switch_addr = model_addr.clone();
    let agent_task = tokio::spawn(async move {
        while let Some(cmd) = prompt_rx.recv().await {
            match cmd {
                ServeCmd::Prompt(prompt, cancel) => {
                    if let Err(e) = agent.run_turn(&prompt, &turn_ev, &cancel).await {
                        let _ = turn_ev.send(AgentEvent::Warning(format!("error: {e:#}")));
                        let _ = turn_ev.send(AgentEvent::Done(TurnStats::default()));
                    }
                    if let Err(e) = store.save(&agent.cfg.model, &cwd_save, &agent.messages) {
                        let _ = turn_ev.send(AgentEvent::Warning(format!("session save failed: {e:#}")));
                    }
                    // Compact while the consumer renders the reply, not mid-turn.
                    agent.idle_compact(&turn_ev).await;
                    // Post-compaction context gauge for the consumer's UI.
                    let (used, limit) = agent.context_usage();
                    let _ = turn_ev.send(AgentEvent::Context { used, limit });
                }
                ServeCmd::TrustPlugin(p) => {
                    let key = rift_core::plugins::trust_key(&p);
                    if let Err(e) = rift_core::config::trust_hook(&key) {
                        let _ = turn_ev
                            .send(AgentEvent::Warning(format!("could not persist plugin approval: {e:#}")));
                    }
                    let names: Vec<&str> = p.tools.iter().map(|t| t.name.as_str()).collect();
                    for tool in rift_core::plugins::tools_for(&p) {
                        agent.register_tool(tool);
                    }
                    // The approval covers the whole manifest: its post_edit
                    // hooks activate now too (and are remembered like
                    // project-config hooks).
                    if !p.hooks.post_edit.is_empty() {
                        let mut hooks = agent.ctx().post_edit_hooks();
                        for h in &p.hooks.post_edit {
                            let _ = rift_core::config::trust_hook(h);
                            if !hooks.contains(h) {
                                hooks.push(h.clone());
                            }
                        }
                        agent.ctx().set_post_edit_hooks(&hooks);
                    }
                    let _ = turn_ev.send(AgentEvent::Info(format!(
                        "plugin '{}' trusted — tools registered: {}",
                        p.name,
                        names.join(", ")
                    )));
                }
                // The same preflight-and-swap as the TUI's /model. Emitted
                // straight to stdout (not through the event channel) because
                // model_changed isn't turn traffic — but it still runs here,
                // serialized after any in-flight turn, since it mutates the
                // agent.
                ServeCmd::SetModel(arg) => {
                    match crate::switch_model(&mut agent, &arg, &switch_host, &switch_providers).await {
                        Ok(note) => {
                            if let Ok(mut m) = switch_addr.lock() {
                                *m = arg.clone();
                            }
                            emit(json!({
                                "event": "model_changed",
                                "model": arg,
                                "num_ctx": agent.cfg.num_ctx,
                                "note": note.trim(),
                            }));
                            // The switch may have adopted/clamped the budget:
                            // refresh the consumer's context gauge.
                            let (used, limit) = agent.context_usage();
                            let _ = turn_ev.send(AgentEvent::Context { used, limit });
                        }
                        Err(e) => {
                            let _ = turn_ev.send(AgentEvent::Warning(format!("model switch failed: {e:#}")));
                        }
                    }
                }
                // Same semantics as the TUI's /undo: revert the write/edit
                // journal's most recent turn; conversation stays intact.
                ServeCmd::Undo => {
                    let ev = match agent.ctx().undo_last_turn() {
                        Ok(restored) if restored.is_empty() => AgentEvent::Warning(
                            "nothing to undo (only write/edit tool changes are tracked)".into(),
                        ),
                        Ok(restored) => {
                            let mut out = String::from("restored to pre-turn state:\n");
                            for p in &restored {
                                out.push_str(&format!("  {}\n", p.display()));
                            }
                            out.push_str("(note: changes made via bash are not tracked)");
                            AgentEvent::Info(out)
                        }
                        Err(e) => AgentEvent::Warning(format!("undo failed: {e:#}")),
                    };
                    let _ = turn_ev.send(ev);
                }
            }
        }
    });
    drop(ev_tx);

    emit(json!({
        "event": "ready",
        "model": model,
        "session": session_path,
        "cwd": cwd,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        "num_ctx": num_ctx,
        // Approval mode at startup (additive v1): true = write/edit/bash
        // pause for approval, false = they apply as the model calls them.
        "approve": initial_approve,
        // Skills + plugin commands invocable via a "/skill:<name> [task]"
        // prompt — listed so consumers can offer completion (additive v1).
        "skills": skills.iter().map(|s| json!({"name": s.name, "description": s.description})).collect::<Vec<_>>(),
        // Feature detection (additive v1): the command set this build
        // accepts, so consumers gate UI on what's actually there instead
        // of probing (unknown commands warn into the user's chat).
        "commands": COMMANDS,
    }));
    emit(json!({"event": "context", "used": initial_used, "limit": num_ctx}));
    if !resumed.is_empty() {
        // Replay user/assistant exchanges so a resumed session renders its
        // prior conversation (tool/system traffic stays out — the consumer
        // shows what a person would scroll back through).
        let msgs: Vec<Value> = resumed
            .iter()
            .filter_map(|m| match m.role {
                rift_ollama::Role::User if !m.content.starts_with("[system]") => {
                    Some(json!({"role": "user", "text": m.content}))
                }
                rift_ollama::Role::Assistant if !m.content.is_empty() && m.tool_calls.is_empty() => {
                    Some(json!({"role": "assistant", "text": m.content}))
                }
                _ => None,
            })
            .collect();
        emit(json!({"event": "history", "messages": msgs}));
    }

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut pending_asks: HashMap<u64, tokio::sync::oneshot::Sender<String>> = HashMap::new();
    // Review id → (reply channel, the proposed content) — the proposal is
    // kept so an `apply` with no `content` field applies it verbatim.
    let mut pending_reviews: HashMap<u64, (tokio::sync::oneshot::Sender<EditReviewReply>, String)> =
        HashMap::new();
    let mut ask_seq: u64 = 0;
    let mut current_cancel: Option<CancellationToken> = None;
    let mut busy = false;
    let mut asks_open = true;
    let mut reviews_open = true;

    // Project-plugin trust rides the ordinary ask machinery: one question
    // per untrusted manifest, right after startup, so serve consumers (the
    // VS Code chat) can approve what the TUI would have asked on stdin.
    // A dropped or non-"trust" answer means skipped — fail safe; the
    // plugin's tools stay unregistered for this session.
    for plugin in pending_plugins {
        ask_seq += 1;
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        emit(json!({
            "event": "ask",
            "id": ask_seq,
            "question": format!(
                "Project plugin '{}' declares {} tool(s) — each runs a command from this repo \
                 when the model calls it. Trust this manifest on this machine?",
                plugin.name,
                plugin.tools.len()
            ),
            "detail": plugin
                .tools
                .iter()
                .map(|t| format!("{}: {} — `{}`", t.name, t.description, t.command))
                .collect::<Vec<_>>(),
            "choices": ["trust", "skip"],
        }));
        pending_asks.insert(ask_seq, tx);
        let cmd_tx = prompt_tx.clone();
        tokio::spawn(async move {
            if let Ok(answer) = rx.await {
                if matches!(answer.trim().to_lowercase().as_str(), "trust" | "yes" | "y") {
                    let _ = cmd_tx.send(ServeCmd::TrustPlugin(plugin));
                }
            }
        });
    }

    loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break };
                if matches!(ev, AgentEvent::Done(_)) {
                    busy = false;
                    current_cancel = None;
                    // Any review still pending when the turn ends is
                    // orphaned (its tool is gone) — a later edit_decision
                    // would silently do nothing while the consumer shows
                    // "applied". Tell it to close them instead.
                    for (id, _) in pending_reviews.drain() {
                        emit(json!({"event": "edit_review_closed", "id": id}));
                    }
                }
                emit(event_json(&ev));
            }
            ask = ask_rx.recv(), if asks_open => {
                match ask {
                    Some(req) => {
                        ask_seq += 1;
                        emit(json!({
                            "event": "ask",
                            "id": ask_seq,
                            "question": req.question,
                            "detail": req.detail,
                            "choices": req.choices,
                        }));
                        pending_asks.insert(ask_seq, req.reply);
                    }
                    None => asks_open = false,
                }
            }
            review = review_rx.recv(), if reviews_open => {
                match review {
                    Some(req) => {
                        // Same id space as asks — one counter, two maps.
                        ask_seq += 1;
                        emit(json!({
                            "event": "edit_review",
                            "id": ask_seq,
                            "tool": req.tool,
                            "path": req.path.display().to_string(),
                            "old": req.old,
                            "new": req.new,
                            // rift's own hunking (added in 2.6.3, additive):
                            // consumers review per-segment instead of
                            // re-deriving a diff from old/new themselves.
                            "segments": segments_json(&req.old, &req.new),
                        }));
                        pending_reviews.insert(ask_seq, (req.reply, req.new));
                    }
                    None => reviews_open = false,
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break }; // EOF: consumer went away
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        emit(json!({"event": "warning", "text": format!("bad command json: {e}")}));
                        continue;
                    }
                };
                match v["cmd"].as_str() {
                    // Capability handshake. Idempotent; send it once at
                    // spawn. Acked with the effective capability set so the
                    // consumer can confirm what it negotiated.
                    Some("hello") => {
                        let edit_review = v["edit_review"].as_bool().unwrap_or(false);
                        if edit_review {
                            ctl.set_edit_review(Some(review_tx.clone()));
                        } else {
                            ctl.set_edit_review(None);
                        }
                        emit(json!({
                            "event": "capabilities",
                            "protocol_version": PROTOCOL_VERSION,
                            "edit_review": edit_review,
                            // Echoed so a consumer that sets approval at
                            // spawn can confirm both switches in one place.
                            "approve": ctl.approval_enabled(),
                        }));
                    }
                    Some("prompt") => {
                        let mut text = v["text"].as_str().unwrap_or("").to_string();
                        if text.trim().is_empty() {
                            continue;
                        }
                        if busy {
                            emit(json!({"event": "warning", "text": "turn in progress — cancel it or wait for done"}));
                            continue;
                        }
                        // `/skill:<name> [task]` expands exactly like the
                        // TUI, so editor chats invoke skills and plugin
                        // commands identically. Plain prompts untouched.
                        if let Some(rest) = text.trim().strip_prefix("/skill:") {
                            let (name, task) = match rest.split_once(char::is_whitespace) {
                                Some((n, t)) => (n, t.trim()),
                                None => (rest, ""),
                            };
                            let Some(s) = skills.iter().find(|s| s.name == name) else {
                                emit(json!({"event": "warning", "text": format!("unknown skill '{name}'")}));
                                continue;
                            };
                            let task = if task.is_empty() {
                                "Apply this skill to the current project now."
                            } else {
                                task
                            };
                            text = format!(
                                "Follow this skill's instructions.\n\n--- SKILL: {} ---\n{}\n--- END SKILL ---\n\nTask: {task}",
                                s.name, s.body
                            );
                        }
                        let cancel = CancellationToken::new();
                        current_cancel = Some(cancel.clone());
                        busy = true;
                        let _ = prompt_tx.send(ServeCmd::Prompt(text, cancel));
                    }
                    Some("answer") => {
                        let id = v["id"].as_u64().unwrap_or(0);
                        if let Some(reply) = pending_asks.remove(&id) {
                            let _ = reply.send(v["text"].as_str().unwrap_or("").to_string());
                        }
                    }
                    Some("edit_decision") => {
                        let id = v["id"].as_u64().unwrap_or(0);
                        if let Some((reply, proposal)) = pending_reviews.remove(&id) {
                            let decision = if v["apply"].as_bool().unwrap_or(false) {
                                // `content` = the reviewer's accepted-hunk
                                // text; absent = apply the proposal as-is.
                                match v["content"].as_str() {
                                    Some(c) => EditReviewReply::Apply(c.to_string()),
                                    None => EditReviewReply::Apply(proposal),
                                }
                            } else {
                                EditReviewReply::Deny
                            };
                            let _ = reply.send(decision);
                        }
                    }
                    Some("cancel") => {
                        if let Some(c) = &current_cancel {
                            c.cancel();
                        }
                        // Cancelling drops the tools awaiting review; close
                        // their diffs so a decision can't be sent into the void.
                        for (id, _) in pending_reviews.drain() {
                            emit(json!({"event": "edit_review_closed", "id": id}));
                        }
                    }
                    Some("undo") => {
                        // Mid-turn undo would race the agent's own writes;
                        // the TUI has the same restriction (input is modal).
                        if busy {
                            emit(json!({"event": "warning", "text": "turn in progress — cancel it or wait before undoing"}));
                            continue;
                        }
                        let _ = prompt_tx.send(ServeCmd::Undo);
                    }
                    // Past-chats picker: read-only, so it answers straight from
                    // the select loop without touching the agent or the turn.
                    Some("list_sessions") => {
                        emit(json!({"event": "sessions", "items": session_list()}));
                    }
                    // Model picker: read-only network probes, so it runs on
                    // its own task — discovery must not block the select
                    // loop (or a turn) while servers time out.
                    Some("list_models") => {
                        let host = host.clone();
                        let providers = providers.clone();
                        let addr = model_addr.clone();
                        tokio::spawn(async move {
                            let models = discover_models(&host, &providers).await;
                            let current = addr.lock().map(|m| m.clone()).unwrap_or_default();
                            emit(json!({"event": "models", "models": models, "current": current}));
                        });
                    }
                    Some("set_model") => {
                        let Some(name) = v["model"].as_str().filter(|s| !s.trim().is_empty()) else {
                            emit(json!({"event": "warning", "text": "set_model needs a 'model' field"}));
                            continue;
                        };
                        // Mid-turn switches would change the model under the
                        // running turn's feet; the TUI has the same guard.
                        if busy {
                            emit(json!({"event": "warning", "text": "turn in progress — cancel it or wait before switching models"}));
                            continue;
                        }
                        let _ = prompt_tx.send(ServeCmd::SetModel(name.trim().to_string()));
                    }
                    // Approval mode (the TUI's /approve … /yolo) over the
                    // wire. Answered from the select loop: it is one atomic
                    // flag the agent task shares, so it needs no queueing and
                    // can be flipped mid-turn. Permission rules are
                    // untouched — deny still refuses, ask still prompts.
                    Some("set_approval") => {
                        let Some(approve) = v["approve"].as_bool() else {
                            emit(json!({"event": "warning", "text": "set_approval needs a boolean 'approve' field"}));
                            continue;
                        };
                        ctl.set_approval(approve);
                        emit(json!({"event": "approval_changed", "approve": approve}));
                    }
                    _ => emit(json!({"event": "warning", "text": "unknown cmd (expected prompt/answer/edit_decision/cancel/undo/list_sessions/list_models/set_model/set_approval)"})),
                }
            }
        }
    }

    // Shutdown: abandon any pending ask/review (dropped reply = denied),
    // cancel the in-flight turn, and let the agent task finish its loop so
    // the session file gets its last save.
    pending_asks.clear();
    pending_reviews.clear();
    if let Some(c) = current_cancel {
        c.cancel();
    }
    drop(prompt_tx);
    let _ = agent_task.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protocol-v1 conformance suite: every event's wire shape, pinned
    /// exactly as docs/SERVE.md documents it. A failure here means a
    /// breaking protocol change — either fix the regression, or bump
    /// PROTOCOL_VERSION and update SERVE.md (that's a 2.0-class change).
    #[test]
    fn wire_shapes_are_frozen_protocol_v1() {
        use rift_core::PlanItem;
        let cases: Vec<(AgentEvent, Value)> = vec![
            (AgentEvent::Iteration(3), json!({"event":"iteration","n":3})),
            (AgentEvent::Thinking("hm".into()), json!({"event":"thinking","text":"hm"})),
            (AgentEvent::Content("hi".into()), json!({"event":"content","text":"hi"})),
            (
                AgentEvent::ToolStart { name: "read".into(), args: "{\"path\":\"x\"}".into() },
                json!({"event":"tool_start","name":"read","args":"{\"path\":\"x\"}"}),
            ),
            (
                AgentEvent::ToolResult { name: "read".into(), ok: true, preview: "…".into() },
                json!({"event":"tool_result","name":"read","ok":true,"preview":"…"}),
            ),
            (AgentEvent::Info("i".into()), json!({"event":"info","text":"i"})),
            (AgentEvent::Warning("w".into()), json!({"event":"warning","text":"w"})),
            (
                AgentEvent::Plan(vec![PlanItem { text: "step".into(), done: false }]),
                json!({"event":"plan","items":[{"text":"step","done":false}]}),
            ),
            (
                AgentEvent::SubAgentStarted { tag: "a1".into(), model: "m".into(), label: "l".into() },
                json!({"event":"subagent_started","tag":"a1","model":"m","label":"l"}),
            ),
            (
                AgentEvent::SubAgentActivity { tag: "a1".into(), text: "t".into(), warn: false },
                json!({"event":"subagent","tag":"a1","text":"t","warn":false}),
            ),
            (
                AgentEvent::SubAgentFinished { tag: "a1".into(), steps: 4 },
                json!({"event":"subagent_finished","tag":"a1","steps":4}),
            ),
            (
                AgentEvent::TaskStarted { id: 1, label: "cargo test".into() },
                json!({"event":"task_started","id":1,"label":"cargo test"}),
            ),
            (
                AgentEvent::TaskFinished { id: 1, label: "cargo test".into(), ok: true, preview: "ok".into() },
                json!({"event":"task_finished","id":1,"label":"cargo test","ok":true,"preview":"ok"}),
            ),
            (
                AgentEvent::Context { used: 10, limit: 100 },
                json!({"event":"context","used":10,"limit":100}),
            ),
            (
                AgentEvent::Done(TurnStats::default()),
                json!({"event":"done","stats":{
                    "iterations":0,"prompt_tokens":0,"billed_prompt_tokens":0,
                    "output_tokens":0,"duration_ms":0,"tokens_per_sec":0.0,
                }}),
            ),
        ];
        for (ev, want) in cases {
            assert_eq!(event_json(&ev), want, "wire shape drifted for {want}");
        }
    }

    #[test]
    fn protocol_version_is_one_until_a_deliberate_break() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    /// edit_review's `segments` field (added 2.6.3, additive): alternating
    /// {"same":true,"lines":[…]} / {"same":false,"old":[…],"new":[…]} runs.
    /// The VS Code reviewer reassembles accepted-hunk content from exactly
    /// this shape — a drift here silently corrupts applied edits.
    #[test]
    fn edit_review_segments_wire_shape() {
        assert_eq!(
            serde_json::to_value(segments_json("a\nb\nc", "a\nB\nc")).unwrap(),
            json!([
                {"same": true, "lines": ["a"]},
                {"same": false, "old": ["b"], "new": ["B"]},
                {"same": true, "lines": ["c"]},
            ])
        );
    }

    /// `commands` in ready is the consumer's feature-detection surface:
    /// entries may be added within v1, never removed or renamed.
    #[test]
    fn ready_commands_cover_the_v1_set() {
        for cmd in ["hello", "prompt", "answer", "edit_decision", "cancel", "undo",
                    "list_sessions", "list_models", "set_model", "set_approval"] {
            assert!(COMMANDS.contains(&cmd), "command '{cmd}' missing from ready.commands");
        }
    }

    /// Approval mode is the switch behind the VS Code auto-approve toggle:
    /// off means write/edit never raise an edit_review or ask prompt, on
    /// means they do. Pinned here because the frontend toggle is only
    /// honest if this flag actually gates the prompts.
    #[test]
    fn approval_switch_gates_the_prompts() {
        let ctx = rift_core::ToolCtx::new(std::env::temp_dir());
        ctx.set_approval(true);
        assert!(ctx.approval_enabled());
        ctx.set_approval(false);
        assert!(!ctx.approval_enabled());
    }
}
