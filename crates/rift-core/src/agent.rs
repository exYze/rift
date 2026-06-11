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
//! - doom-loop detection: an identical (name, args) call repeated 3× aborts

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use rift_ollama::{
    extract_textual_tool_calls, ChatOptions, ChatRequest, Message, OllamaClient, Role, StreamDelta,
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "gemma4:26b".into(),
            num_ctx: 32_768,
            temperature: None,
            max_iterations: 25,
            think: None,
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
    /// Turn finished (success or abort); always the final event of a turn.
    Done(TurnStats),
}

pub struct Agent {
    client: OllamaClient,
    pub cfg: AgentConfig,
    registry: ToolRegistry,
    ctx: ToolCtx,
    pub messages: Vec<Message>,
    /// Running ratio of actual/estimated prompt tokens, fed back from
    /// `prompt_eval_count` so the chars/4 heuristic self-corrects per model.
    calibration: f64,
}

impl Agent {
    pub fn new(client: OllamaClient, cfg: AgentConfig, registry: ToolRegistry, ctx: ToolCtx, system_prompt: String) -> Self {
        Self { client, cfg, registry, ctx, messages: vec![Message::system(system_prompt)], calibration: 1.0 }
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
        match compact::summarize_history(&self.client, &self.cfg.model, self.cfg.num_ctx, &self.messages).await {
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

        let tools = self.registry.tool_defs();
        let known = self.registry.names();
        let start = Instant::now();
        let mut stats = TurnStats::default();
        let mut repeat_counts: HashMap<String, u32> = HashMap::new();

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
                    temperature: self.cfg.temperature,
                    num_predict: None,
                }),
            };

            let tx2 = tx.clone();
            let stream_fut = self.client.chat_stream(&req, move |delta| {
                let _ = match delta {
                    StreamDelta::Thinking(t) => tx2.send(AgentEvent::Thinking(t)),
                    StreamDelta::Content(c) => tx2.send(AgentEvent::Content(c)),
                    StreamDelta::ToolCall(_) => Ok(()),
                };
            });
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

            let tool_calls = msg.tool_calls.clone();
            self.messages.push(msg);

            if tool_calls.is_empty() {
                stats.duration_ms = start.elapsed().as_millis();
                let _ = tx.send(AgentEvent::Done(stats.clone()));
                return Ok(stats);
            }

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
                        "aborting: tool '{canonical}' called 3x with identical arguments"
                    )));
                    self.messages.push(Message::user(
                        "[system] You repeated the same tool call three times. Stop and summarize what you have so far.",
                    ));
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
                    name: canonical,
                    ok,
                    preview: preview(&result, 200),
                });

                // Correlate by the name the model used, per the native protocol.
                self.messages.push(Message::tool_result(requested_name, result));
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
