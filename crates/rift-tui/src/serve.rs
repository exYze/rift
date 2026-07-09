//! `rift --serve`: a line-oriented JSON protocol over stdio for editor
//! integrations (the VS Code sidebar chat). stdout carries one event object
//! per line; stdin carries one command object per line; stderr stays
//! human-readable diagnostics. The process exits when stdin closes.
//!
//! Commands:  {"cmd":"hello","edit_review":bool} | {"cmd":"prompt","text":…}
//!            | {"cmd":"answer","id":…,"text":…} | {"cmd":"cancel"} | {"cmd":"undo"}
//!            | {"cmd":"edit_decision","id":…,"apply":bool,"content":…}
//! Events:    ready, history, iteration, thinking, content, tool_start,
//!            tool_result, info, warning, plan, subagent_started, subagent,
//!            subagent_finished, task_started, task_finished,
//!            ask (answer it by id), edit_review (decide it by id),
//!            done (always ends a turn).
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
    }));
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
                    // Capability handshake. Idempotent; send it once at spawn.
                    Some("hello") => {
                        if v["edit_review"].as_bool().unwrap_or(false) {
                            review_ctl.set_edit_review(Some(review_tx.clone()));
                        } else {
                            review_ctl.set_edit_review(None);
                        }
                    }
                    Some("prompt") => {
                        let text = v["text"].as_str().unwrap_or("").to_string();
                        if text.trim().is_empty() {
                            continue;
                        }
                        if busy {
                            emit(json!({"event": "warning", "text": "turn in progress — cancel it or wait for done"}));
                            continue;
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
