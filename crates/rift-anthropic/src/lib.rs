//! Native Anthropic Messages API client — one `rift_provider::Provider`
//! implementation for Claude models (claude-opus-4-8, claude-sonnet-5, …).
//!
//! Differences from the OpenAI-compat shape this crate reconciles:
//! - the system prompt is a top-level `system` field, not a message;
//! - tool results are `tool_result` content blocks inside `user` messages,
//!   correlated by `tool_use_id`;
//! - streamed tool arguments arrive as `input_json_delta` fragments per
//!   content block, not accumulated by array index;
//! - thinking blocks carry signatures the API validates on replay, so the
//!   assistant's raw content blocks round-trip via `Message::provider_data`;
//! - `max_tokens` is required, and there is no `num_ctx` (context is fixed
//!   per model; discovered via `GET /v1/models/{id}`).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Map, Value};

use rift_provider::{
    api_error_message, for_each_line, http_client, normalize_base_url, send_with_retry,
    ChatOutcome, ChatRequest, ChatStats, LineFlow, Message, ModelCapabilities, ModelEntry,
    Provider, Role, StreamDelta, ToolCall, ToolCallFunction,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `max_tokens` is required by the API; we stream, so a generous cap is safe
/// (only generated tokens bill). Kept under Haiku's 64K output ceiling.
const DEFAULT_MAX_TOKENS: i64 = 32_000;

#[derive(Clone)]
pub struct AnthropicClient {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl AnthropicClient {
    /// `base_url` is the API root (default `https://api.anthropic.com`); a
    /// trailing `/v1` is added if missing so config values in either form work.
    pub fn new(base_url: impl AsRef<str>, api_key: Option<String>) -> Self {
        let mut base = normalize_base_url(base_url.as_ref());
        if !base.ends_with("/v1") {
            base = format!("{base}/v1");
        }
        Self { base_url: base, api_key, http: http_client() }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let b = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .header("anthropic-version", ANTHROPIC_VERSION);
        match &self.api_key {
            Some(k) => b.header("x-api-key", k),
            None => b,
        }
    }
}

// ---- request wire types (neutral -> Anthropic) -----------------------------

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: i64,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

/// Map the neutral history to Anthropic messages. The system prompt lifts out
/// to the top level; role=tool messages become `tool_result` blocks — with
/// consecutive results merged into ONE user message, which the API requires
/// for parallel tool calls.
fn build_request(req: &ChatRequest) -> AnthropicRequest {
    let system: Option<String> = {
        let joined: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect();
        (!joined.is_empty()).then(|| joined.join("\n\n"))
    };

    // Raw content blocks (thinking signatures, exact tool_use blocks) matter
    // only for the current turn's tool loop; older assistant turns are built
    // from neutral fields so cross-provider histories and pruned/compacted
    // clones stay valid.
    let last_user = req.messages.iter().rposition(|m| m.role == Role::User).unwrap_or(0);

    let mut messages: Vec<Value> = Vec::new();
    for (i, m) in req.messages.iter().enumerate() {
        match m.role {
            Role::System => {}
            Role::User => messages.push(json!({"role": "user", "content": m.content})),
            Role::Assistant => {
                let content = match (&m.provider_data, i > last_user) {
                    (Some(raw), true) if raw.get("content").is_some() => {
                        raw.get("content").cloned().unwrap_or(Value::Null)
                    }
                    _ => Value::Array(assistant_blocks(m)),
                };
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                });
                // Parallel tool results must share one user message.
                let merged = messages
                    .last_mut()
                    .filter(|v| {
                        v["role"] == "user"
                            && v["content"].as_array().is_some_and(|a| {
                                a.iter().all(|b| b["type"] == "tool_result")
                            })
                    })
                    .and_then(|v| v["content"].as_array_mut())
                    .map(|a| a.push(block.clone()))
                    .is_some();
                if !merged {
                    messages.push(json!({"role": "user", "content": [block]}));
                }
            }
        }
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.function.name,
                "description": t.function.description,
                "input_schema": t.function.parameters,
            })
        })
        .collect();

    // Thinking: only ever send `adaptive` — `disabled` 400s on Fable-tier
    // models and `budget_tokens` 400s on Opus 4.7+, while omitting the field
    // is accepted everywhere. `display: summarized` so rift can show it.
    let thinking = (req.think == Some(true))
        .then(|| json!({"type": "adaptive", "display": "summarized"}));

    // Adaptive-thinking models reject sampling params; only pass temperature
    // through when thinking is off.
    let temperature = match thinking {
        Some(_) => None,
        None => req.options.as_ref().and_then(|o| o.temperature),
    };

    AnthropicRequest {
        model: req.model.clone(),
        max_tokens: req
            .options
            .as_ref()
            .and_then(|o| o.num_predict)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        stream: true,
        system,
        messages,
        tools,
        thinking,
        temperature,
    }
}

/// Assistant content blocks from neutral fields (no raw payload available):
/// text + tool_use. Thinking is intentionally dropped — without its original
/// signature the API rejects the block, and older turns don't need it.
fn assistant_blocks(m: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    if !m.content.is_empty() {
        blocks.push(json!({"type": "text", "text": m.content}));
    }
    for (i, tc) in m.tool_calls.iter().enumerate() {
        blocks.push(json!({
            "type": "tool_use",
            "id": tc.id.clone().unwrap_or_else(|| format!("toolu_missing_{i}")),
            "name": tc.function.name,
            "input": tc.function.arguments,
        }));
    }
    if blocks.is_empty() {
        // The API rejects empty content; this only happens for degenerate
        // histories (e.g. a cancelled turn stored an empty assistant message).
        blocks.push(json!({"type": "text", "text": "…"}));
    }
    blocks
}

// ---- streaming accumulation (Anthropic -> neutral) -------------------------

/// One in-flight content block, keyed by the stream's block index.
#[derive(Default)]
struct BlockAcc {
    kind: String,
    text: String,
    /// tool_use only: id, name, and the accumulated input_json_delta string.
    tool_id: String,
    tool_name: String,
    tool_args: String,
    /// thinking only: the signature to round-trip.
    signature: String,
}

#[derive(Default)]
struct StreamAcc {
    blocks: Vec<BlockAcc>,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
}

fn block_at(acc: &mut StreamAcc, index: usize) -> Result<&mut BlockAcc> {
    // Indices are assigned sequentially by the server; anything wildly out of
    // range means a broken stream — refuse rather than allocate unbounded.
    if index > 256 {
        bail!("server sent content block index {index} (max 256)");
    }
    if index >= acc.blocks.len() {
        acc.blocks.resize_with(index + 1, BlockAcc::default);
    }
    Ok(&mut acc.blocks[index])
}

/// Apply one SSE `data:` payload. Returns tool calls completed by this event
/// (on `content_block_stop`) so the caller can emit them as deltas.
fn apply_event(
    value: &Value,
    acc: &mut StreamAcc,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<Vec<ToolCall>> {
    let mut completed = Vec::new();
    match value.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "message_start" => {
            if let Some(n) = value.pointer("/message/usage/input_tokens").and_then(|v| v.as_u64()) {
                acc.input_tokens = n;
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let block = value.get("content_block").cloned().unwrap_or_default();
            let slot = block_at(acc, index)?;
            slot.kind = block.get("type").and_then(|t| t.as_str()).unwrap_or_default().to_string();
            if slot.kind == "tool_use" {
                slot.tool_id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default().into();
                slot.tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or_default().into();
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = value.get("delta").cloned().unwrap_or_default();
            let slot = block_at(acc, index)?;
            match delta.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "text_delta" => {
                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                        slot.text.push_str(t);
                        on_delta(StreamDelta::Content(t.to_string()));
                    }
                }
                "thinking_delta" => {
                    if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                        slot.text.push_str(t);
                        on_delta(StreamDelta::Thinking(t.to_string()));
                    }
                }
                "input_json_delta" => {
                    if let Some(j) = delta.get("partial_json").and_then(|v| v.as_str()) {
                        slot.tool_args.push_str(j);
                    }
                }
                "signature_delta" => {
                    if let Some(s) = delta.get("signature").and_then(|v| v.as_str()) {
                        slot.signature.push_str(s);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let slot = block_at(acc, index)?;
            if slot.kind == "tool_use" {
                let arguments = parse_tool_input(&slot.tool_args).map_err(|e| {
                    anyhow!(
                        "model emitted invalid JSON input for tool '{}': {e}; raw: {}",
                        slot.tool_name,
                        preview(&slot.tool_args, 300)
                    )
                })?;
                completed.push(ToolCall {
                    id: Some(slot.tool_id.clone()),
                    function: ToolCallFunction {
                        index: Some(index as i64),
                        name: slot.tool_name.clone(),
                        arguments,
                    },
                });
            }
        }
        "message_delta" => {
            if let Some(sr) = value.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                acc.stop_reason = Some(sr.to_string());
            }
            if let Some(n) = value.pointer("/usage/output_tokens").and_then(|v| v.as_u64()) {
                acc.output_tokens = n;
            }
        }
        // Mid-stream failures (overloads, upstream errors) arrive as an
        // `error` event; surface them instead of a silently truncated message.
        "error" => {
            let msg = value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown stream error");
            bail!("anthropic api error (mid-stream): {msg}");
        }
        // message_stop, ping, and anything newer are structural no-ops.
        _ => {}
    }
    Ok(completed)
}

/// Streamed tool input: empty means "no arguments" (zero-arg tools send no
/// input_json_delta at all).
fn parse_tool_input(raw: &str) -> Result<Map<String, Value>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(raw)? {
        Value::Object(m) => Ok(m),
        _ => bail!("tool input is not a JSON object"),
    }
}

fn preview(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[async_trait]
impl Provider for AnthropicClient {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn tags(&self) -> Result<Vec<ModelEntry>> {
        let resp = send_with_retry(self.req(reqwest::Method::GET, "/models?limit=100")).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{status}: {}", api_error_message(&text)));
        }
        let body: Value = resp.json().await?;
        Ok(body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                    .map(|id| ModelEntry { name: id.to_string(), capabilities: vec![] })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn show(&self, model: &str) -> Result<ModelCapabilities> {
        let resp = send_with_retry(self.req(reqwest::Method::GET, &format!("/models/{model}"))).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{status}: {}", api_error_message(&text)));
        }
        let body: Value = resp.json().await?;
        // Every current Claude model supports tool use; thinking support is
        // reported per model. Context window comes from max_input_tokens.
        let mut capabilities = vec!["tools".to_string()];
        if body
            .pointer("/capabilities/thinking/supported")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
        {
            capabilities.push("thinking".to_string());
        }
        Ok(ModelCapabilities {
            capabilities,
            context_length: body.get("max_input_tokens").and_then(|v| v.as_u64()),
        })
    }

    async fn chat_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatOutcome> {
        if self.api_key.is_none() {
            bail!(
                "no Anthropic API key — set ANTHROPIC_API_KEY, or api_key/api_key_env on the provider in config"
            );
        }
        let body = build_request(req);
        let resp = send_with_retry(self.req(reqwest::Method::POST, "/messages").json(&body)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("anthropic api error ({status}): {}", api_error_message(&text)));
        }

        let mut acc = StreamAcc::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        // Server-Sent Events: `event: <type>` lines (ignored — every `data:`
        // payload repeats the type) and `data: {json}` lines; the stream ends
        // with message_stop + connection close, no [DONE] sentinel.
        for_each_line(resp.bytes_stream(), |line| {
            let Some(payload) = line.strip_prefix("data:") else { return Ok(LineFlow::Continue) };
            let payload = payload.trim();
            if payload.is_empty() {
                return Ok(LineFlow::Continue);
            }
            let value: Value = serde_json::from_str(payload)
                .with_context(|| format!("malformed stream event: {}", preview(payload, 200)))?;
            for call in apply_event(&value, &mut acc, on_delta)? {
                on_delta(StreamDelta::ToolCall(call.clone()));
                tool_calls.push(call);
            }
            Ok(LineFlow::Continue)
        })
        .await?;

        // Neutral fields for the UI/history + the raw blocks for lossless
        // replay (thinking signatures, exact tool_use ids).
        let mut content = String::new();
        let mut thinking = String::new();
        let mut raw_blocks: Vec<Value> = Vec::new();
        for b in &acc.blocks {
            match b.kind.as_str() {
                "text" => {
                    content.push_str(&b.text);
                    raw_blocks.push(json!({"type": "text", "text": b.text}));
                }
                "thinking" => {
                    thinking.push_str(&b.text);
                    raw_blocks.push(json!({
                        "type": "thinking",
                        "thinking": b.text,
                        "signature": b.signature,
                    }));
                }
                "tool_use" => {
                    let input: Value = parse_tool_input(&b.tool_args).map(Value::Object).unwrap_or(json!({}));
                    raw_blocks.push(json!({
                        "type": "tool_use",
                        "id": b.tool_id,
                        "name": b.tool_name,
                        "input": input,
                    }));
                }
                _ => {}
            }
        }

        let message = Message {
            role: Role::Assistant,
            content,
            thinking: (!thinking.is_empty()).then_some(thinking),
            tool_calls,
            tool_name: None,
            tool_call_id: None,
            provider_data: (!raw_blocks.is_empty()).then(|| json!({"content": raw_blocks})),
        };
        let stats = ChatStats {
            prompt_eval_count: acc.input_tokens,
            eval_count: acc.output_tokens,
            total_duration: 0,
            eval_duration: 0,
        };
        // Context is fixed per model here — the server errors instead of
        // silently truncating, so front-truncation detection never applies.
        Ok(ChatOutcome {
            message,
            done_reason: acc.stop_reason,
            stats,
            truncation_suspected: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_provider::{ChatOptions, ToolDef};

    fn neutral_request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "claude-opus-4-8".into(),
            messages,
            tools: vec![ToolDef::function(
                "read",
                "Read a file",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            )],
            stream: true,
            think: None,
            keep_alive: None,
            options: Some(ChatOptions { num_ctx: Some(32768), temperature: Some(0.2), num_predict: None }),
        }
    }

    #[test]
    fn system_lifts_out_and_tools_map_to_input_schema() {
        let req = neutral_request(vec![Message::system("be brief"), Message::user("hi")]);
        let built = build_request(&req);
        assert_eq!(built.system.as_deref(), Some("be brief"));
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.messages[0]["role"], "user");
        assert_eq!(built.tools[0]["name"], "read");
        assert!(built.tools[0]["input_schema"].is_object());
        assert_eq!(built.max_tokens, DEFAULT_MAX_TOKENS);
        // No thinking requested -> temperature passes through, no thinking field.
        assert!(built.thinking.is_none());
        assert_eq!(built.temperature, Some(0.2));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let mut assistant = Message::user("");
        assistant.role = Role::Assistant;
        assistant.tool_calls = vec![
            ToolCall {
                id: Some("toolu_1".into()),
                function: ToolCallFunction { index: Some(0), name: "read".into(), arguments: Map::new() },
            },
            ToolCall {
                id: Some("toolu_2".into()),
                function: ToolCallFunction { index: Some(1), name: "read".into(), arguments: Map::new() },
            },
        ];
        let mut r1 = Message::tool_result("read", "one");
        r1.tool_call_id = Some("toolu_1".into());
        let mut r2 = Message::tool_result("read", "two");
        r2.tool_call_id = Some("toolu_2".into());

        let req = neutral_request(vec![Message::user("go"), assistant, r1, r2]);
        let built = build_request(&req);
        // user, assistant, ONE merged user message with two tool_result blocks.
        assert_eq!(built.messages.len(), 3);
        let results = built.messages[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
        // The assistant echo carries tool_use blocks with the same ids.
        let assistant_blocks = built.messages[1]["content"].as_array().unwrap();
        assert!(assistant_blocks.iter().any(|b| b["type"] == "tool_use" && b["id"] == "toolu_1"));
    }

    #[test]
    fn thinking_maps_to_adaptive_and_drops_temperature() {
        let mut req = neutral_request(vec![Message::user("hi")]);
        req.think = Some(true);
        let built = build_request(&req);
        assert_eq!(built.thinking.as_ref().unwrap()["type"], "adaptive");
        assert!(built.temperature.is_none(), "sampling params 400 alongside adaptive thinking");
        // think=Some(false) must OMIT the field ({"type":"disabled"} 400s on
        // Fable-tier models), not send disabled.
        req.think = Some(false);
        assert!(build_request(&req).thinking.is_none());
    }

    #[test]
    fn current_turn_assistant_uses_raw_provider_data() {
        let mut assistant = Message::user("");
        assistant.role = Role::Assistant;
        assistant.content = "text fallback".into();
        assistant.provider_data = Some(json!({
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig123"},
                {"type": "text", "text": "raw text"}
            ]
        }));
        // After the last user message -> raw blocks (signature preserved).
        let req = neutral_request(vec![Message::user("go"), assistant.clone()]);
        let built = build_request(&req);
        let blocks = built.messages[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["signature"], "sig123");

        // Before the last user message (older turn) -> neutral rebuild, no
        // thinking block (its signature no longer matters and costs tokens).
        let req = neutral_request(vec![Message::user("go"), assistant, Message::user("next")]);
        let built = build_request(&req);
        let blocks = built.messages[1]["content"].as_array().unwrap();
        assert!(blocks.iter().all(|b| b["type"] != "thinking"));
        assert_eq!(blocks[0]["text"], "text fallback");
    }

    #[test]
    fn stream_events_accumulate_text_tools_and_usage() {
        let mut acc = StreamAcc::default();
        let mut deltas: Vec<StreamDelta> = Vec::new();
        let mut sink = |d: StreamDelta| deltas.push(d);
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 42}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "read"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "th\": \"a.rs\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ];
        let mut calls = Vec::new();
        for ev in &events {
            calls.extend(apply_event(ev, &mut acc, &mut sink).unwrap());
        }
        assert_eq!(acc.input_tokens, 42);
        assert_eq!(acc.output_tokens, 7);
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(acc.blocks[0].text, "Hello");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("toolu_9"));
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
    }

    #[test]
    fn error_event_fails_the_stream() {
        let mut acc = StreamAcc::default();
        let mut sink = |_: StreamDelta| {};
        let err = apply_event(
            &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            &mut acc,
            &mut sink,
        )
        .expect_err("error event must fail");
        assert!(format!("{err:#}").contains("Overloaded"));
    }
}
