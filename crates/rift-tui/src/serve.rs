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
//! Events:    ready, capabilities (acks hello), history, iteration,
//!            thinking, content, tool_start, tool_result, info, warning,
//!            plan, subagent_started, subagent, subagent_finished,
//!            task_started, task_finished, ask (answer it by id),
//!            edit_review (decide it by id), edit_review_closed,
//!            done (always ends a turn), context ({used, limit} —
//!            context-window occupancy, sent at startup and after each
//!            turn's idle compaction).
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
use rift_core::{Agent, AgentEvent, AskRequest, EditReviewReply, EditReviewRequest, SessionStore, TurnStats};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The serve protocol version, carried in `ready` and `capabilities`.
/// Bumped ONLY on breaking changes (docs/SERVE.md has the rules).
pub const PROTOCOL_VERSION: u32 = 1;

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
    }
}

/// Work items for the agent task. Undo runs there (not on the select loop)
/// because the agent owns its `ToolCtx`, and routing through the same queue
/// keeps it serialized with turns.
enum ServeCmd {
    Prompt(String, CancellationToken),
    Undo,
}

pub async fn run_serve(
    mut agent: Agent,
    store: SessionStore,
    mut ask_rx: mpsc::UnboundedReceiver<AskRequest>,
    model: String,
    resumed: Vec<rift_ollama::Message>,
    skills: Vec<rift_core::Skill>,
) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<ServeCmd>();
    // Inline diff review: created here but only installed into the ctx when
    // the consumer's hello opts in — a consumer that doesn't know the
    // edit_review event must keep getting plain ask prompts, not hang.
    let (review_tx, mut review_rx) = mpsc::unbounded_channel::<EditReviewRequest>();
    let review_ctl = agent.ctx().clone();
    // Background-task events surface through the same channel, between turns.
    agent.ctx().bg().set_notify(ev_tx.clone());
    let cwd = std::env::current_dir()?.display().to_string();
    let session_path = store.path().display().to_string();

    // Turns run on their own task (same shape as the TUI) so stdin stays
    // responsive mid-turn for cancel and ask answers.
    let turn_ev = ev_tx.clone();
    let cwd_save = cwd.clone();
    // Captured before the agent moves: the ready event carries num_ctx and
    // an initial gauge so resumed sessions show their fill level up front.
    let (initial_used, num_ctx) = agent.context_usage();
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
        // Skills + plugin commands invocable via a "/skill:<name> [task]"
        // prompt — listed so consumers can offer completion (additive v1).
        "skills": skills.iter().map(|s| json!({"name": s.name, "description": s.description})).collect::<Vec<_>>(),
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
                            review_ctl.set_edit_review(Some(review_tx.clone()));
                        } else {
                            review_ctl.set_edit_review(None);
                        }
                        emit(json!({
                            "event": "capabilities",
                            "protocol_version": PROTOCOL_VERSION,
                            "edit_review": edit_review,
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
                    _ => emit(json!({"event": "warning", "text": "unknown cmd (expected prompt/answer/edit_decision/cancel/undo)"})),
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
}
