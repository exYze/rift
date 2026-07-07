//! The agent loop: LLM call → tool calls → tool results → repeat until the
//! model answers in plain text.
//!
//! Hardening for local models (each addresses a documented failure mode of
//! Ollama-backed agents):
//! - explicit per-request `num_ctx` + truncation detection via prompt_eval_count
//! - fallback parser for tool calls emitted as plain JSON text in `content`
//! - alias resolution for hallucinated tool names (`read_file` → `read`)
//! - unknown tools / bad arguments are returned to the model as tool-result
//!   errors so it can self-correct, instead of crashing the turn
//! - doom-loop detection: an identical (name, args) call repeated 3× is
//!   refused with an error tool-result instead of being executed again

use std::collections::HashMap;
use std::time::Instant;

use std::sync::Arc;

use anyhow::Result;
use rift_provider::{
    extract_textual_tool_calls, ChatOptions, ChatRequest, Message, Provider, Role, StreamDelta,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::tools::{ToolCtx, ToolRegistry};
use crate::trace::{FailureCounters, ToolTraceRecord, TraceWriter, TurnTrace};

/// Recognized reasoning-effort levels, weakest to strongest. Providers pass
/// them through verbatim; servers with fewer grades map internally (DeepSeek:
/// low/medium→high, xhigh→max) or reject with a clear error.
pub const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub num_ctx: u64,
    pub temperature: Option<f64>,
    pub max_iterations: usize,
    /// None = server default. Only set true after confirming the model has the
    /// "thinking" capability (otherwise Ollama returns a 400).
    pub think: Option<bool>,
    /// Reasoning effort ("low"/"medium"/"high"/"max"/…) for models that
    /// grade their thinking; None = server default. Implies thinking on.
    pub effort: Option<String>,
    /// The prompt is known to be a work item (headless --prompt, swarm
    /// candidates): a turn that ends without any tool use always gets the
    /// apply-your-changes nudge, regardless of phrasing.
    pub always_task: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "gemma4:26b".into(),
            num_ctx: 32_768,
            // Local models' server-default temperature (often 0.7-1.0) makes
            // tool-calling flaky: the same task alternates between a proper
            // agentic run and a chat-only answer. Pin low for reliability.
            temperature: Some(0.2),
            max_iterations: 25,
            think: None,
            effort: None,
            always_task: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TurnStats {
    pub iterations: usize,
    /// The LAST call's prompt size — the context-window gauge.
    pub prompt_tokens: u64,
    /// Prompt tokens summed across every call this turn — what a metered
    /// provider actually bills for input (each iteration re-sends history).
    pub billed_prompt_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u128,
    pub tokens_per_sec: f64,
    /// How often the hardening layer had to intervene this turn — the
    /// per-model failure signal traces and /stats aggregate.
    pub failures: FailureCounters,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A new LLM call is starting (1-based iteration within the turn).
    Iteration(usize),
    Thinking(String),
    Content(String),
    ToolStart { name: String, args: String },
    ToolResult { name: String, ok: bool, preview: String },
    /// Informational note (compaction activity, budget state) — not a problem.
    Info(String),
    Warning(String),
    /// The model updated its task checklist (the `plan` tool).
    Plan(Vec<crate::tools::PlanItem>),
    /// A background task (bash run_in_background / background sub-agent)
    /// started. Flows through the session-wide notify channel, so it can
    /// arrive outside any turn.
    TaskStarted { id: u64, label: String },
    /// A background task finished on its own (kills are silent). `preview`
    /// is the output tail (shell) or final report (agent), capped ~1KB.
    TaskFinished { id: u64, label: String, ok: bool, preview: String },
    /// Turn finished (success or abort); always the final event of a turn.
    Done(TurnStats),
}

pub struct Agent {
    client: Arc<dyn Provider>,
    pub cfg: AgentConfig,
    registry: ToolRegistry,
    ctx: ToolCtx,
    pub messages: Vec<Message>,
    /// Running ratio of actual/estimated prompt tokens, fed back from
    /// `prompt_eval_count` so the chars/4 heuristic self-corrects per model.
    calibration: f64,
    /// Opt-in JSONL turn traces (`--trace`); None = no tracing.
    trace: Option<TraceWriter>,
    /// The previous turn was cancelled (Esc) mid-flight. The next turn's
    /// input gets a note saying so — otherwise the interrupted task is the
    /// last standing instruction in history and the model resumes it even
    /// when the user has moved on to something unrelated.
    interrupted: bool,
    /// Image attachments (data URLs) queued for the NEXT turn's user
    /// message — @-mentioned images and --attach files land here.
    pending_images: Vec<String>,
    /// Checkpoint per turn: (ctx turn number, message index of the turn's
    /// user message). /rewind pops these to restore files (via the edit
    /// journal) and truncate the conversation together.
    turn_marks: Vec<(u64, usize)>,
}

impl Agent {
    pub fn new(client: Arc<dyn Provider>, cfg: AgentConfig, registry: ToolRegistry, ctx: ToolCtx, system_prompt: String) -> Self {
        Self {
            client,
            cfg,
            registry,
            ctx,
            messages: vec![Message::system(system_prompt)],
            calibration: 1.0,
            trace: None,
            interrupted: false,
            pending_images: vec![],
            turn_marks: vec![],
        }
    }

    /// How many turns /rewind can currently reach back.
    pub fn rewindable_turns(&self) -> usize {
        self.turn_marks.len()
    }

    /// Rewind `n` turns: restore every file the write/edit tools touched in
    /// those turns (bash-made changes are outside the journal) and truncate
    /// the conversation to just before the first rewound turn. Returns the
    /// restored paths.
    pub fn rewind(&mut self, n: usize) -> Result<Vec<std::path::PathBuf>> {
        if n == 0 {
            anyhow::bail!("rewind how far? /rewind <n> with n >= 1");
        }
        if self.turn_marks.len() < n {
            anyhow::bail!(
                "only {} turn(s) are rewindable (checkpoints reach back at most 20 turns and don't survive compaction)",
                self.turn_marks.len()
            );
        }
        let (turn, msg_index) = self.turn_marks[self.turn_marks.len() - n];
        let restored = self.ctx.undo_to_turn(turn.saturating_sub(1))?;
        self.messages.truncate(msg_index.min(self.messages.len()));
        self.turn_marks.truncate(self.turn_marks.len() - n);
        self.interrupted = false;
        Ok(restored)
    }

    /// Queue image attachments (data URLs) for the next turn's user message.
    /// The turn consumes them whether or not the model has vision — a
    /// text-only model simply never sees them (Ollama ignores the field;
    /// OpenAI-style servers reject with a clear error the user can act on).
    pub fn attach_images(&mut self, images: Vec<String>) {
        self.pending_images.extend(images);
    }

    pub fn set_trace(&mut self, trace: Option<TraceWriter>) {
        self.trace = trace;
    }

    pub fn client(&self) -> &Arc<dyn Provider> {
        &self.client
    }

    /// Swap the backing provider (e.g. the `/host` command).
    pub fn set_client(&mut self, client: Arc<dyn Provider>) {
        self.client = client;
    }

    /// Register a tool mid-session (e.g. an MCP server trusted via `/mcp
    /// trust`); tool defs are rebuilt per request, so it's usable immediately.
    pub fn register_tool(&mut self, tool: Box<dyn crate::tools::Tool>) {
        self.registry.register(tool);
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    /// Current actual/estimated prompt-token ratio of the estimator.
    pub fn calibration(&self) -> f64 {
        self.calibration
    }

    /// Enforce the context budget before a request. Stage 1 prunes old tool
    /// outputs; stage 2 (LLM summarization) collapses old history entirely.
    async fn enforce_budget(
        &mut self,
        tools_overhead: u64,
        tx: &UnboundedSender<AgentEvent>,
        counters: &mut FailureCounters,
    ) {
        let budget = compact::usable_budget(self.cfg.num_ctx);
        let est = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
        if est <= budget {
            return;
        }
        let touched = compact::prune_old_turns(&mut self.messages);
        let est2 = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
        if touched > 0 {
            counters.compact_prunes += 1;
            let _ = tx.send(AgentEvent::Info(format!(
                "compacted: pruned {touched} old outputs (~{est} → ~{est2} tok, budget {budget})"
            )));
        }
        if est2 <= budget {
            return;
        }
        let _ = tx.send(AgentEvent::Info("compacting: summarizing earlier conversation…".into()));
        match compact::summarize_history(&*self.client, &self.cfg.model, self.cfg.num_ctx, &self.messages).await {
            Ok(rebuilt) => {
                self.messages = rebuilt;
                // Summarization rebuilds history — message indices in the
                // rewind checkpoints no longer point anywhere meaningful.
                self.turn_marks.clear();
                counters.compact_summaries += 1;
                let est3 = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
                let _ = tx.send(AgentEvent::Info(format!("compacted: history summarized (~{est3} tok)")));
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Warning(format!("summarization failed ({e}); continuing pruned")));
            }
        }
    }

    /// Proactive compaction between turns: run the same budget enforcement
    /// the next LLM call would run, but now — during idle time while the
    /// user reads the reply — instead of mid-turn while they wait on it.
    /// The trigger is identical, so firing here means the mid-turn check
    /// finds the history already inside budget.
    pub async fn idle_compact(&mut self, tx: &UnboundedSender<AgentEvent>) {
        let tools = self.registry.tool_defs();
        let overhead =
            compact::estimate_tokens(&serde_json::to_string(&tools).unwrap_or_default());
        // Idle compaction is bookkeeping between turns; its compaction
        // counters aren't attributed to any turn's trace.
        let mut counters = FailureCounters::default();
        self.enforce_budget(overhead, tx, &mut counters).await;
    }

    /// Append this turn to the JSONL trace, if tracing is enabled. Trace
    /// failures never fail the turn — warn once and carry on.
    fn write_trace(
        &self,
        user_input: &str,
        outcome: &str,
        stats: &TurnStats,
        tools: &[ToolTraceRecord],
        tx: &UnboundedSender<AgentEvent>,
    ) {
        let Some(writer) = &self.trace else { return };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rec = TurnTrace {
            ts,
            model: &self.cfg.model,
            provider: self.client.base_url(),
            num_ctx: self.cfg.num_ctx,
            prompt: preview(user_input, 240),
            outcome,
            iterations: stats.iterations,
            prompt_tokens: stats.prompt_tokens,
            billed_prompt_tokens: stats.billed_prompt_tokens,
            output_tokens: stats.output_tokens,
            duration_ms: stats.duration_ms,
            tools,
            failures: &stats.failures,
        };
        if let Err(e) = writer.record(&rec) {
            let _ = tx.send(AgentEvent::Warning(format!("trace write failed: {e:#}")));
        }
    }

    /// Run one user turn. `cancel` aborts cleanly mid-stream or mid-tool:
    /// the in-flight LLM call is dropped (history stays at the last consistent
    /// point) and any pending tool calls get "cancelled" results so the
    /// message sequence remains valid for the next request.
    pub async fn run_turn(
        &mut self,
        user_input: &str,
        tx: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<TurnStats> {
        // After an Esc-cancelled turn, tell the model the interruption was
        // deliberate — the abandoned task must not be resumed on its own.
        let user_input: String = if std::mem::take(&mut self.interrupted) {
            format!(
                "[note: the user pressed Esc to CANCEL your previous, incomplete turn. Treat that \
                 interrupted task as abandoned — do NOT resume it unless this message asks you to.]\n\n{user_input}"
            )
        } else {
            user_input.to_string()
        };
        let user_input = user_input.as_str();
        // Checkpoint BEFORE this turn's message lands: /rewind restores to
        // this index and the journal turn that begin_turn is about to open.
        let mark_index = self.messages.len();
        let mut user_msg = Message::user(user_input);
        user_msg.images = std::mem::take(&mut self.pending_images);
        self.messages.push(user_msg);
        self.ctx.begin_turn();
        self.turn_marks.push((self.ctx.current_turn(), mark_index));
        const REWIND_KEEP: usize = 20;
        if self.turn_marks.len() > REWIND_KEEP {
            let drop = self.turn_marks.len() - REWIND_KEEP;
            self.turn_marks.drain(..drop);
        }
        // Keep the sub-agent handle tracking the live provider/config, so
        // /model and /host switches carry over to delegated agents. Only
        // refreshes where a frontend installed one — child agents and swarm
        // candidates stay without (no nested delegation).
        self.ctx.refresh_subagent(self.client.clone(), self.cfg.clone());

        let tools = self.registry.tool_defs();
        let known = self.registry.names();
        let start = Instant::now();
        let mut stats = TurnStats::default();
        let mut tool_records: Vec<ToolTraceRecord> = vec![];
        let mut repeat_counts: HashMap<String, u32> = HashMap::new();
        // Chat-only failure mode: small models sometimes "answer" a coding
        // task by pasting the fix into the reply without touching any file.
        // A variant seen in the first bench matrix (gemma4:26b): the model
        // explores (repo_map/read) but never edits — so the nudge keys on
        // mutating tool use, not any tool use.
        let mut used_tools = false;
        let mut used_mutating = false;
        let mut nudged_apply = false;
        let mut retried_greedy = false;
        let mut temp_override: Option<f64> = None;
        // An explicit content request ("...in a code block", "do not use any
        // tools") disables the chat-only recovery machinery entirely — even
        // for headless work items, an answer in chat IS the deliverable.
        let chat_output = prompt_wants_chat_output(user_input);
        let actionable_prompt =
            !chat_output && (self.cfg.always_task || prompt_looks_actionable(user_input));

        let tools_overhead =
            compact::estimate_tokens(&serde_json::to_string(&tools).unwrap_or_default());

        for iteration in 1..=self.cfg.max_iterations {
            stats.iterations = iteration;
            let _ = tx.send(AgentEvent::Iteration(iteration));

            self.enforce_budget(tools_overhead, tx, &mut stats.failures).await;

            // Old turns' reasoning isn't needed for continuity — strip it
            // from the request (not from stored history) to save context.
            let last_user = self.messages.iter().rposition(|m| m.role == Role::User).unwrap_or(0);
            let request_messages: Vec<Message> = self
                .messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    if i < last_user && m.role == Role::Assistant && m.thinking.is_some() {
                        let mut m = m.clone();
                        m.thinking = None;
                        m
                    } else {
                        m.clone()
                    }
                })
                .collect();
            let unscaled_estimate =
                compact::estimate_prompt_tokens(&request_messages, tools_overhead, 1.0);

            let req = ChatRequest {
                model: self.cfg.model.clone(),
                messages: request_messages,
                tools: tools.clone(),
                stream: true,
                think: self.cfg.think,
                effort: self.cfg.effort.clone(),
                keep_alive: Some("10m".into()),
                options: Some(ChatOptions {
                    num_ctx: Some(self.cfg.num_ctx),
                    temperature: temp_override.or(self.cfg.temperature),
                    num_predict: None,
                }),
            };

            let tx2 = tx.clone();
            let mut on_delta = move |delta| {
                let _ = match delta {
                    StreamDelta::Thinking(t) => tx2.send(AgentEvent::Thinking(t)),
                    StreamDelta::Content(c) => tx2.send(AgentEvent::Content(c)),
                    StreamDelta::ToolCall(_) => Ok(()),
                };
            };
            let stream_fut = self.client.chat_stream(&req, &mut on_delta);
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.interrupted = true;
                    let _ = tx.send(AgentEvent::Warning("turn cancelled".into()));
                    stats.duration_ms = start.elapsed().as_millis();
                    self.write_trace(user_input, "cancelled", &stats, &tool_records, tx);
                    let _ = tx.send(AgentEvent::Done(stats.clone()));
                    return Ok(stats);
                }
                r = stream_fut => match r {
                    Ok(o) => o,
                    Err(e) => {
                        stats.duration_ms = start.elapsed().as_millis();
                        self.write_trace(user_input, "error", &stats, &tool_records, tx);
                        return Err(e);
                    }
                },
            };

            stats.prompt_tokens = outcome.stats.prompt_eval_count;
            stats.billed_prompt_tokens += outcome.stats.prompt_eval_count;
            stats.output_tokens += outcome.stats.eval_count;
            stats.tokens_per_sec = outcome.stats.tokens_per_sec();

            // Feed the server's real count back into the estimator (EMA,
            // clamped so one weird call can't poison it).
            if outcome.stats.prompt_eval_count > 0 && unscaled_estimate > 0 {
                let ratio = outcome.stats.prompt_eval_count as f64 / unscaled_estimate as f64;
                self.calibration = (0.7 * self.calibration + 0.3 * ratio).clamp(0.5, 3.0);
            }

            if outcome.truncation_suspected {
                stats.failures.truncations += 1;
                let _ = tx.send(AgentEvent::Warning(format!(
                    "context overflow: prompt filled num_ctx={} and was likely front-truncated; results may be degraded",
                    self.cfg.num_ctx
                )));
            }

            let mut msg = outcome.message;
            let cleaned = strip_template_tokens(&msg.content);
            if cleaned != msg.content.trim() {
                stats.failures.template_strips += 1;
                let _ = tx.send(AgentEvent::Warning("stripped leaked template tokens from output".into()));
                msg.content = cleaned;
            }

            // Recover tool calls the model emitted as plain JSON text.
            if msg.tool_calls.is_empty() {
                let recovered = extract_textual_tool_calls(&msg.content, &known);
                if !recovered.is_empty() {
                    stats.failures.textual_recoveries += 1;
                    let _ = tx.send(AgentEvent::Warning(
                        "model emitted a textual tool call; recovered it".into(),
                    ));
                    msg.tool_calls = recovered;
                    msg.content.clear();
                }
            }

            // Providers may omit tool-call ids (Ollama always does). Synthesize
            // them before the message is stored so the id in history and the
            // tool_call_id on the result below always agree — strict
            // OpenAI-compatible servers reject histories where they don't.
            let msg_index = self.messages.len();
            for (i, call) in msg.tool_calls.iter_mut().enumerate() {
                if call.id.is_none() {
                    call.id = Some(format!("call_{msg_index}_{i}"));
                }
            }

            let tool_calls = msg.tool_calls.clone();
            let content_has_code = msg.content.contains("```");
            self.messages.push(msg);

            if tool_calls.is_empty() {
                // The model finished without applying any change this turn
                // even though the prompt asked for one (or its reply contains
                // code) — either pure chat, or it explored with read-only
                // tools and then "fixed" the task in its reply. Nudge once to
                // apply for real.
                if !used_mutating && !nudged_apply && !chat_output && (content_has_code || actionable_prompt) {
                    nudged_apply = true;
                    stats.failures.apply_nudges += 1;
                    let _ = tx.send(AgentEvent::Warning(
                        "model finished without modifying any files; nudging it to apply the change".into(),
                    ));
                    self.messages.push(Message::user(
                        "[system] You have not changed any project files this turn. \
                         If the request was to fix/implement something, apply it now using the edit/write \
                         tools and verify the result. If the user only wanted content or an explanation, \
                         reply that your previous answer stands — do not rewrite or shorten it.",
                    ));
                    continue;
                }
                // Known work item, still no tool use after the nudge: scrap
                // the chat-only attempt entirely and retry greedily — at
                // temperature 0 the model reliably follows the tool-calling
                // instructions. Headless/swarm only (no human to confuse).
                if !used_tools && !retried_greedy && self.cfg.always_task && !chat_output {
                    retried_greedy = true;
                    nudged_apply = false;
                    stats.failures.greedy_retries += 1;
                    temp_override = Some(0.0);
                    // Scrap this turn's messages. A start-of-turn index would
                    // be stale here — mid-turn compaction rebuilds the whole
                    // list — so cut after the turn's user message instead
                    // (compaction keeps it; fall back to the last user-role
                    // message if it was reworded away).
                    let cut = self
                        .messages
                        .iter()
                        .rposition(|m| m.role == Role::User && m.content == user_input)
                        .or_else(|| self.messages.iter().rposition(|m| m.role == Role::User))
                        .map(|i| i + 1)
                        .unwrap_or(self.messages.len());
                    self.messages.truncate(cut);
                    let _ = tx.send(AgentEvent::Warning(
                        "model never used tools on a work item; retrying the turn at temperature 0".into(),
                    ));
                    continue;
                }
                stats.duration_ms = start.elapsed().as_millis();
                self.write_trace(user_input, "answered", &stats, &tool_records, tx);
                let _ = tx.send(AgentEvent::Done(stats.clone()));
                return Ok(stats);
            }
            used_tools = true;

            for call in tool_calls {
                let requested_name = call.function.name.clone();
                let canonical = self.registry.resolve_alias(&requested_name).to_string();
                if canonical != requested_name {
                    stats.failures.alias_hits += 1;
                }
                let args_json = serde_json::to_string(&call.function.arguments).unwrap_or_default();

                // Doom-loop guard.
                let signature = format!("{canonical}\u{0}{args_json}");
                let count = repeat_counts.entry(signature).or_insert(0);
                *count += 1;
                if *count >= 3 {
                    stats.failures.doom_loop_trips += 1;
                    tool_records.push(ToolTraceRecord {
                        name: canonical.clone(),
                        ok: false,
                        target: trace_target(&call.function.arguments),
                    });
                    let _ = tx.send(AgentEvent::Warning(format!(
                        "tool '{canonical}' called 3x with identical arguments; refusing the repeat"
                    )));
                    // Answer the call anyway: every id in the assistant's
                    // tool_calls must get a tool result or strict
                    // OpenAI-compatible servers reject the whole history.
                    let mut result_msg = Message::tool_result(
                        requested_name,
                        "ERROR: repeated identical tool call refused. You already ran this exact call \
                         and have its result. Take a different action or summarize what you have so far.",
                    );
                    result_msg.tool_call_id = call.id.clone();
                    self.messages.push(result_msg);
                    continue;
                }

                let _ = tx.send(AgentEvent::ToolStart { name: canonical.clone(), args: args_json });

                let (ok, result) = match self.registry.get(&canonical) {
                    // Once cancelled, every remaining call gets a "cancelled"
                    // result so the assistant message's tool_calls all stay
                    // answered and the history remains valid.
                    Some(_) if cancel.is_cancelled() => (false, "ERROR: cancelled by user".to_string()),
                    Some(tool) => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => (false, "ERROR: cancelled by user".to_string()),
                            r = tool.execute(&call.function.arguments, &self.ctx) => match r {
                                Ok(out) => (true, out),
                                Err(e) => {
                                    stats.failures.tool_errors += 1;
                                    (false, format!("ERROR: {e:#}"))
                                }
                            },
                        }
                    }
                    None => {
                        stats.failures.unknown_tools += 1;
                        (
                            false,
                            format!(
                                "ERROR: unknown tool '{requested_name}'. Available tools: {}",
                                known.join(", ")
                            ),
                        )
                    }
                };
                tool_records.push(ToolTraceRecord {
                    name: canonical.clone(),
                    ok,
                    target: trace_target(&call.function.arguments),
                });

                // bash counts as mutating: a command can apply changes and we
                // can't tell a read-only one apart; erring this way only
                // suppresses a nudge, never fires a spurious one. Same logic
                // for agent: delegated sub-agents apply changes themselves.
                if ok && matches!(canonical.as_str(), "write" | "edit" | "bash" | "agent") {
                    used_mutating = true;
                }

                let _ = tx.send(AgentEvent::ToolResult {
                    name: canonical.clone(),
                    ok,
                    preview: preview(&result, 200),
                });
                if canonical == "plan" && ok {
                    let _ = tx.send(AgentEvent::Plan(self.ctx.plan_snapshot()));
                }

                // Correlate by name (Ollama) and by id (OpenAI-compat): keep both
                // so the tool result round-trips whatever provider is in use.
                let mut result_msg = Message::tool_result(requested_name, result);
                result_msg.tool_call_id = call.id.clone();
                self.messages.push(result_msg);
            }

            if cancel.is_cancelled() {
                self.interrupted = true;
                let _ = tx.send(AgentEvent::Warning("turn cancelled".into()));
                stats.duration_ms = start.elapsed().as_millis();
                self.write_trace(user_input, "cancelled", &stats, &tool_records, tx);
                let _ = tx.send(AgentEvent::Done(stats.clone()));
                return Ok(stats);
            }
        }

        stats.duration_ms = start.elapsed().as_millis();
        let _ = tx.send(AgentEvent::Warning(format!(
            "stopped after {} iterations without a final answer",
            self.cfg.max_iterations
        )));
        self.write_trace(user_input, "max_iterations", &stats, &tool_records, tx);
        let _ = tx.send(AgentEvent::Done(stats.clone()));
        Ok(stats)
    }
}

/// The file/pattern argument of a tool call, for traces — what retrieval
/// heuristics (repo-map ranking, hydration) are tuned against.
fn trace_target(args: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    for key in ["path", "pattern"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return Some(v.chars().take(200).collect());
        }
    }
    None
}

/// Did the user explicitly ask for the answer as chat content (a document,
/// a code block, no tools)? Suppresses the apply-nudge: "Write a markdown
/// cheatsheet, do not use any tools" must not get nudged into rewriting —
/// the nudged retry tends to drop the requested content entirely.
fn prompt_wants_chat_output(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    const OPT_OUT: &[&str] = &[
        "do not use any tools",
        "don't use any tools",
        "do not use tools",
        "don't use tools",
        "without using tools",
        "no tools",
        "in a code block",
        "in the chat",
        "in your reply",
    ];
    OPT_OUT.iter().any(|s| p.contains(s))
}

/// Does the user's prompt ask for changes (vs. a question/explanation)?
/// Drives the no-tools nudge — imprecise on purpose; a false positive just
/// costs the model one short clarification round.
fn prompt_looks_actionable(prompt: &str) -> bool {
    if prompt_wants_chat_output(prompt) {
        return false;
    }
    let p = prompt.to_lowercase();
    const VERBS: &[&str] = &[
        "fix", "implement", "add ", "refactor", "rename", "create", "write ", "update",
        "change", "make ", "remove", "delete", "extend", "convert", "build ",
    ];
    VERBS.iter().any(|v| p.contains(v))
}

/// Weak models occasionally leak chat-template special tokens (e.g.
/// `<|tool_response>`, `<|eot_id|>`) into their text. Strip them.
fn strip_template_tokens(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"<\|[^<>]{0,48}>").unwrap());
    re.replace_all(s, "").trim().to_string()
}

fn preview(s: &str, max: usize) -> String {
    let s = s.trim();
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if end < s.len() {
        format!("{}…", &s[..end].replace('\n', " "))
    } else {
        s.replace('\n', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProvider;
    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn base_url(&self) -> &str {
            "stub"
        }
        async fn tags(&self) -> Result<Vec<rift_provider::ModelEntry>> {
            Ok(vec![])
        }
        async fn show(&self, _m: &str) -> Result<rift_provider::ModelCapabilities> {
            Ok(rift_provider::ModelCapabilities::default())
        }
        async fn chat_stream(
            &self,
            _req: &ChatRequest,
            _on_delta: &mut (dyn FnMut(StreamDelta) + Send),
        ) -> Result<rift_provider::ChatOutcome> {
            anyhow::bail!("stub")
        }
    }

    #[test]
    fn rewind_truncates_messages_and_marks() {
        let mut agent = Agent::new(
            Arc::new(StubProvider),
            AgentConfig::default(),
            ToolRegistry::standard(),
            ToolCtx::new(std::env::temp_dir()),
            "system".into(),
        );
        // Simulate three completed turns the way run_turn records them.
        for i in 1..=3 {
            let mark_index = agent.messages.len();
            agent.messages.push(Message::user(format!("turn {i}")));
            agent.ctx.begin_turn();
            agent.turn_marks.push((agent.ctx.current_turn(), mark_index));
            agent.messages.push(Message {
                role: Role::Assistant,
                content: format!("reply {i}"),
                thinking: None,
                tool_calls: vec![],
                tool_name: None,
                tool_call_id: None,
                provider_data: None,
                images: vec![],
            });
        }
        assert_eq!(agent.rewindable_turns(), 3);
        assert_eq!(agent.messages.len(), 7); // system + 3×(user+assistant)

        // Rewind 2 turns: back to just after turn 1's exchange.
        agent.rewind(2).unwrap();
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages.last().unwrap().content, "reply 1");
        assert_eq!(agent.rewindable_turns(), 1);
        // Beyond the marks → a clear error, nothing changes.
        assert!(agent.rewind(5).is_err());
        assert_eq!(agent.messages.len(), 3);
    }

    #[test]
    fn content_requests_suppress_the_apply_nudge() {
        // Explicit chat-output requests must not count as actionable even
        // when they contain actionable verbs ("write").
        assert!(prompt_looks_actionable("Fix the bug in stats.py"));
        assert!(prompt_looks_actionable("write a helper and add tests"));
        assert!(!prompt_looks_actionable(
            "Write a markdown cheatsheet. Do not use any tools."
        ));
        assert!(!prompt_looks_actionable("Show the config in a code block"));
        assert!(prompt_wants_chat_output("reply in the chat, no tools"));
        assert!(!prompt_wants_chat_output("fix the tests"));
    }

    #[test]
    fn strips_leaked_template_tokens() {
        assert_eq!(strip_template_tokens("<|tool_response>"), "");
        assert_eq!(strip_template_tokens("done <|eot_id|> now"), "done  now");
        assert_eq!(strip_template_tokens("x < y and a |> b"), "x < y and a |> b");
        assert_eq!(strip_template_tokens("plain answer"), "plain answer");
    }
}
