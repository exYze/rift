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

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub num_ctx: u64,
    pub temperature: Option<f64>,
    pub max_iterations: usize,
    /// None = server default. Only set true after confirming the model has the
    /// "thinking" capability (otherwise Ollama returns a 400).
    pub think: Option<bool>,
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
            always_task: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TurnStats {
    pub iterations: usize,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u128,
    pub tokens_per_sec: f64,
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
}

impl Agent {
    pub fn new(client: Arc<dyn Provider>, cfg: AgentConfig, registry: ToolRegistry, ctx: ToolCtx, system_prompt: String) -> Self {
        Self { client, cfg, registry, ctx, messages: vec![Message::system(system_prompt)], calibration: 1.0 }
    }

    pub fn client(&self) -> &Arc<dyn Provider> {
        &self.client
    }

    /// Swap the backing provider (e.g. the `/host` command).
    pub fn set_client(&mut self, client: Arc<dyn Provider>) {
        self.client = client;
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
    async fn enforce_budget(&mut self, tools_overhead: u64, tx: &UnboundedSender<AgentEvent>) {
        let budget = compact::usable_budget(self.cfg.num_ctx);
        let est = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
        if est <= budget {
            return;
        }
        let touched = compact::prune_old_turns(&mut self.messages);
        let est2 = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
        if touched > 0 {
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
                let est3 = compact::estimate_prompt_tokens(&self.messages, tools_overhead, self.calibration);
                let _ = tx.send(AgentEvent::Info(format!("compacted: history summarized (~{est3} tok)")));
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Warning(format!("summarization failed ({e}); continuing pruned")));
            }
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
        self.messages.push(Message::user(user_input));
        self.ctx.begin_turn();

        let tools = self.registry.tool_defs();
        let known = self.registry.names();
        let start = Instant::now();
        let mut stats = TurnStats::default();
        let mut repeat_counts: HashMap<String, u32> = HashMap::new();
        // Chat-only failure mode: small models sometimes "answer" a coding
        // task by pasting the fix into the reply without touching any file.
        let mut used_tools = false;
        let mut nudged_apply = false;
        let mut retried_greedy = false;
        let mut temp_override: Option<f64> = None;
        let actionable_prompt = self.cfg.always_task || prompt_looks_actionable(user_input);

        let tools_overhead =
            compact::estimate_tokens(&serde_json::to_string(&tools).unwrap_or_default());

        for iteration in 1..=self.cfg.max_iterations {
            stats.iterations = iteration;
            let _ = tx.send(AgentEvent::Iteration(iteration));

            self.enforce_budget(tools_overhead, tx).await;

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
                    let _ = tx.send(AgentEvent::Warning("turn cancelled".into()));
                    stats.duration_ms = start.elapsed().as_millis();
                    let _ = tx.send(AgentEvent::Done(stats.clone()));
                    return Ok(stats);
                }
                r = stream_fut => r?,
            };

            stats.prompt_tokens = outcome.stats.prompt_eval_count;
            stats.output_tokens += outcome.stats.eval_count;
            stats.tokens_per_sec = outcome.stats.tokens_per_sec();

            // Feed the server's real count back into the estimator (EMA,
            // clamped so one weird call can't poison it).
            if outcome.stats.prompt_eval_count > 0 && unscaled_estimate > 0 {
                let ratio = outcome.stats.prompt_eval_count as f64 / unscaled_estimate as f64;
                self.calibration = (0.7 * self.calibration + 0.3 * ratio).clamp(0.5, 3.0);
            }

            if outcome.truncation_suspected {
                let _ = tx.send(AgentEvent::Warning(format!(
                    "context overflow: prompt filled num_ctx={} and was likely front-truncated; results may be degraded",
                    self.cfg.num_ctx
                )));
            }

            let mut msg = outcome.message;
            let cleaned = strip_template_tokens(&msg.content);
            if cleaned != msg.content.trim() {
                let _ = tx.send(AgentEvent::Warning("stripped leaked template tokens from output".into()));
                msg.content = cleaned;
            }

            // Recover tool calls the model emitted as plain JSON text.
            if msg.tool_calls.is_empty() {
                let recovered = extract_textual_tool_calls(&msg.content, &known);
                if !recovered.is_empty() {
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
                // The model finished without touching a tool this turn even
                // though the prompt asked for changes (or its reply contains
                // code) — it likely "fixed" the task in chat instead of in
                // the files. Nudge once to apply for real.
                if !used_tools && !nudged_apply && (content_has_code || actionable_prompt) {
                    nudged_apply = true;
                    let _ = tx.send(AgentEvent::Warning(
                        "model replied with code but didn't modify any files; nudging it to apply the change".into(),
                    ));
                    self.messages.push(Message::user(
                        "[system] You wrote code in your reply but did not change any project files. \
                         If the request was to fix/implement something, apply it now using the read/edit/write \
                         tools and verify the result. If the user only wanted an explanation, restate your \
                         answer briefly without code.",
                    ));
                    continue;
                }
                // Known work item, still no tool use after the nudge: scrap
                // the chat-only attempt entirely and retry greedily — at
                // temperature 0 the model reliably follows the tool-calling
                // instructions. Headless/swarm only (no human to confuse).
                if !used_tools && !retried_greedy && self.cfg.always_task {
                    retried_greedy = true;
                    nudged_apply = false;
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
                let _ = tx.send(AgentEvent::Done(stats.clone()));
                return Ok(stats);
            }
            used_tools = true;

            for call in tool_calls {
                let requested_name = call.function.name.clone();
                let canonical = self.registry.resolve_alias(&requested_name).to_string();
                let args_json = serde_json::to_string(&call.function.arguments).unwrap_or_default();

                // Doom-loop guard.
                let signature = format!("{canonical}\u{0}{args_json}");
                let count = repeat_counts.entry(signature).or_insert(0);
                *count += 1;
                if *count >= 3 {
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
                                Err(e) => (false, format!("ERROR: {e:#}")),
                            },
                        }
                    }
                    None => (
                        false,
                        format!(
                            "ERROR: unknown tool '{requested_name}'. Available tools: {}",
                            known.join(", ")
                        ),
                    ),
                };

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
                let _ = tx.send(AgentEvent::Warning("turn cancelled".into()));
                stats.duration_ms = start.elapsed().as_millis();
                let _ = tx.send(AgentEvent::Done(stats.clone()));
                return Ok(stats);
            }
        }

        stats.duration_ms = start.elapsed().as_millis();
        let _ = tx.send(AgentEvent::Warning(format!(
            "stopped after {} iterations without a final answer",
            self.cfg.max_iterations
        )));
        let _ = tx.send(AgentEvent::Done(stats.clone()));
        Ok(stats)
    }
}

/// Does the user's prompt ask for changes (vs. a question/explanation)?
/// Drives the no-tools nudge — imprecise on purpose; a false positive just
/// costs the model one short clarification round.
fn prompt_looks_actionable(prompt: &str) -> bool {
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

    #[test]
    fn strips_leaked_template_tokens() {
        assert_eq!(strip_template_tokens("<|tool_response>"), "");
        assert_eq!(strip_template_tokens("done <|eot_id|> now"), "done  now");
        assert_eq!(strip_template_tokens("x < y and a |> b"), "x < y and a |> b");
        assert_eq!(strip_template_tokens("plain answer"), "plain answer");
    }
}
