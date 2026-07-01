//! OpenAI-compatible `/v1/chat/completions` provider.
//!
//! One implementation of `rift_provider::Provider` that speaks the OpenAI Chat
//! Completions protocol, so rift can drive vLLM, LM Studio, llama.cpp's server,
//! OpenRouter, LiteLLM, and Ollama's own `/v1` endpoint.
//!
//! Differences from Ollama's native API this crate reconciles:
//! - tool-call arguments are JSON *strings* here (parsed to objects for the
//!   neutral types, and re-stringified on the way out);
//! - tool results correlate by `tool_call_id`, not by name;
//! - streamed tool-call arguments arrive as fragments accumulated by index;
//! - there's no `/api/show`, so capabilities are best-known defaults.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use rift_provider::{
    ChatOutcome, ChatRequest, ChatStats, Message, ModelCapabilities, ModelEntry, Provider, Role,
    StreamDelta, ToolCall, ToolCallFunction,
};

#[derive(Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAiClient {
    /// `base_url` is the API root (e.g. `https://openrouter.ai/api/v1` or
    /// `http://localhost:11434/v1`); a trailing `/v1` is added if missing.
    pub fn new(base_url: impl AsRef<str>, api_key: Option<String>) -> Self {
        let mut base = base_url.as_ref().trim_end_matches('/').to_string();
        if !base.starts_with("http") {
            base = format!("http://{base}");
        }
        if !base.ends_with("/v1") {
            base = format!("{base}/v1");
        }
        Self { base_url: base, api_key, http: reqwest::Client::new() }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let b = self.http.request(method, format!("{}{path}", self.base_url));
        match &self.api_key {
            Some(k) => b.bearer_auth(k),
            None => b,
        }
    }
}

// ---- request wire types (neutral -> OpenAI) -------------------------------

#[derive(Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OaiTool>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i64>,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct OaiMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OaiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct OaiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OaiToolCallFn,
}

#[derive(Serialize)]
struct OaiToolCallFn {
    name: String,
    /// OpenAI takes arguments as a JSON *string*.
    arguments: String,
}

#[derive(Serialize)]
struct OaiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OaiToolFn,
}

#[derive(Serialize)]
struct OaiToolFn {
    name: String,
    description: String,
    parameters: Value,
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Map one neutral message to the OpenAI shape.
fn to_oai_message(m: &Message) -> OaiMessage {
    let tool_calls: Vec<OaiToolCall> = m
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| OaiToolCall {
            id: tc.id.clone().unwrap_or_else(|| format!("call_{i}")),
            kind: "function",
            function: OaiToolCallFn {
                name: tc.function.name.clone(),
                arguments: serde_json::to_string(&tc.function.arguments).unwrap_or_else(|_| "{}".into()),
            },
        })
        .collect();
    // Assistant messages that are pure tool calls send content: null.
    let content = if m.content.is_empty() && !tool_calls.is_empty() {
        None
    } else {
        Some(m.content.clone())
    };
    OaiMessage {
        role: role_str(m.role),
        content,
        tool_calls,
        // A tool result needs the id it answers; fall back to the name if the
        // upstream call carried no id (rare, non-compliant servers).
        tool_call_id: if m.role == Role::Tool {
            m.tool_call_id.clone().or_else(|| m.tool_name.clone())
        } else {
            None
        },
        name: if m.role == Role::Tool { m.tool_name.clone() } else { None },
    }
}

fn build_request(req: &ChatRequest) -> OaiRequest {
    OaiRequest {
        model: req.model.clone(),
        messages: req.messages.iter().map(to_oai_message).collect(),
        tools: req
            .tools
            .iter()
            .map(|t| OaiTool {
                kind: "function",
                function: OaiToolFn {
                    name: t.function.name.clone(),
                    description: t.function.description.clone(),
                    parameters: t.function.parameters.clone(),
                },
            })
            .collect(),
        stream: true,
        temperature: req.options.as_ref().and_then(|o| o.temperature),
        // Ollama's num_predict maps to OpenAI's max_tokens; num_ctx has no
        // equivalent (context is fixed per model) and `think` isn't sent.
        max_tokens: req.options.as_ref().and_then(|o| o.num_predict),
        stream_options: StreamOptions { include_usage: true },
    }
}

// ---- streaming response wire types (OpenAI -> neutral) --------------------

#[derive(Deserialize)]
struct OaiChunk {
    #[serde(default)]
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
struct OaiChoice {
    #[serde(default)]
    delta: OaiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OaiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiDeltaToolCall>>,
    /// Some OpenAI-compatible servers stream reasoning under one of these.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct OaiDeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OaiDeltaFn>,
}

#[derive(Deserialize)]
struct OaiDeltaFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

/// Everything a streamed response accumulates across chunks, so `apply_chunk`
/// takes one `&mut` instead of six loose out-params.
#[derive(Default)]
struct StreamAcc {
    content: String,
    thinking: String,
    calls: Vec<ToolCallAcc>,
    prompt_tokens: u64,
    completion_tokens: u64,
    done_reason: Option<String>,
}

/// Apply one streamed chunk to the running accumulators, emitting content/
/// thinking deltas as they arrive.
fn apply_chunk(
    chunk: OaiChunk,
    acc: &mut StreamAcc,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) {
    if let Some(u) = chunk.usage {
        if u.prompt_tokens > 0 {
            acc.prompt_tokens = u.prompt_tokens;
        }
        if u.completion_tokens > 0 {
            acc.completion_tokens = u.completion_tokens;
        }
    }
    let Some(choice) = chunk.choices.into_iter().next() else { return };
    if let Some(fr) = choice.finish_reason {
        acc.done_reason = Some(fr);
    }
    let delta = choice.delta;
    if let Some(t) = delta.reasoning.or(delta.reasoning_content) {
        if !t.is_empty() {
            acc.thinking.push_str(&t);
            on_delta(StreamDelta::Thinking(t));
        }
    }
    if let Some(c) = delta.content {
        if !c.is_empty() {
            acc.content.push_str(&c);
            on_delta(StreamDelta::Content(c));
        }
    }
    for tc in delta.tool_calls.into_iter().flatten() {
        if tc.index >= acc.calls.len() {
            acc.calls.resize_with(tc.index + 1, ToolCallAcc::default);
        }
        let slot = &mut acc.calls[tc.index];
        if let Some(id) = tc.id {
            slot.id = id;
        }
        if let Some(f) = tc.function {
            if let Some(n) = f.name {
                slot.name.push_str(&n);
            }
            if let Some(a) = f.arguments {
                slot.args.push_str(&a);
            }
        }
    }
}

#[async_trait]
impl Provider for OpenAiClient {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn tags(&self) -> Result<Vec<ModelEntry>> {
        #[derive(Deserialize)]
        struct Models {
            #[serde(default)]
            data: Vec<ModelObj>,
        }
        #[derive(Deserialize)]
        struct ModelObj {
            id: String,
        }
        let resp = self.req(reqwest::Method::GET, "/models").send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("{}: {}", resp.status(), resp.text().await.unwrap_or_default()));
        }
        let models: Models = resp.json().await?;
        Ok(models.data.into_iter().map(|m| ModelEntry { name: m.id, capabilities: vec![] }).collect())
    }

    async fn show(&self, _model: &str) -> Result<ModelCapabilities> {
        // No /show in the OpenAI protocol. Assume tool support (nearly universal
        // for chat models) and leave context length unknown (no front-truncation
        // to detect). `/model` can still switch freely.
        Ok(ModelCapabilities { capabilities: vec!["tools".into()], context_length: None })
    }

    async fn chat_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatOutcome> {
        let body = build_request(req);
        let resp = self.req(reqwest::Method::POST, "/chat/completions").json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or(text);
            return Err(anyhow!("openai api error ({status}): {msg}"));
        }

        let mut acc = StreamAcc::default();

        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        'outer: while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            // Server-Sent Events: `data: {json}` lines, terminated by `data: [DONE]`.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(payload) = line.trim().strip_prefix("data:") else { continue };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    break 'outer;
                }
                if payload.is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<OaiChunk>(payload) {
                    apply_chunk(parsed, &mut acc, on_delta);
                }
            }
        }

        let tool_calls: Vec<ToolCall> = acc
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| {
                let arguments = serde_json::from_str::<serde_json::Map<String, Value>>(&c.args).unwrap_or_default();
                let call = ToolCall {
                    id: (!c.id.is_empty()).then_some(c.id),
                    function: ToolCallFunction { index: Some(i as i64), name: c.name, arguments },
                };
                on_delta(StreamDelta::ToolCall(call.clone()));
                call
            })
            .collect();

        let message = Message {
            role: Role::Assistant,
            content: acc.content,
            thinking: (!acc.thinking.is_empty()).then_some(acc.thinking),
            tool_calls,
            tool_name: None,
            tool_call_id: None,
        };
        let stats = ChatStats {
            prompt_eval_count: acc.prompt_tokens,
            eval_count: acc.completion_tokens,
            total_duration: 0,
            eval_duration: 0,
        };
        Ok(ChatOutcome { message, done_reason: acc.done_reason, stats, truncation_suspected: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_result_maps_to_tool_call_id_and_string_args() {
        // An assistant tool call: neutral arguments (object) -> OpenAI string.
        let mut assistant = Message { role: Role::Assistant, content: String::new(), thinking: None, tool_calls: vec![], tool_name: None, tool_call_id: None };
        assistant.tool_calls.push(ToolCall {
            id: Some("call_9".into()),
            function: ToolCallFunction {
                index: Some(0),
                name: "read".into(),
                arguments: serde_json::from_value(json!({"path": "a.rs"})).unwrap(),
            },
        });
        let oai = to_oai_message(&assistant);
        assert_eq!(oai.role, "assistant");
        assert!(oai.content.is_none()); // pure tool call -> null content
        assert_eq!(oai.tool_calls[0].id, "call_9");
        assert_eq!(oai.tool_calls[0].function.arguments, r#"{"path":"a.rs"}"#);

        // The tool result correlates by tool_call_id.
        let mut result = Message::tool_result("read", "contents");
        result.tool_call_id = Some("call_9".into());
        let oai = to_oai_message(&result);
        assert_eq!(oai.role, "tool");
        assert_eq!(oai.tool_call_id.as_deref(), Some("call_9"));
        assert_eq!(oai.content.as_deref(), Some("contents"));
    }

    #[test]
    fn streamed_tool_call_arguments_accumulate_by_index() {
        let mut acc = StreamAcc::default();
        let mut sink = |_: StreamDelta| {};

        // id + name in the first fragment, arguments split across fragments.
        for frag in [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"comm"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}),
            json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}}),
        ] {
            let chunk: OaiChunk = serde_json::from_value(frag).unwrap();
            apply_chunk(chunk, &mut acc, &mut sink);
        }
        assert_eq!(acc.calls.len(), 1);
        assert_eq!(acc.calls[0].id, "call_1");
        assert_eq!(acc.calls[0].name, "bash");
        assert_eq!(acc.calls[0].args, r#"{"command":"ls"}"#);
        assert_eq!((acc.prompt_tokens, acc.completion_tokens), (42, 7));
        // The accumulated arg string parses to a proper object.
        let args: serde_json::Map<String, Value> = serde_json::from_str(&acc.calls[0].args).unwrap();
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn content_deltas_stream_through() {
        let mut acc = StreamAcc::default();
        let mut seen = String::new();
        {
            let mut sink = |d: StreamDelta| {
                if let StreamDelta::Content(s) = d {
                    seen.push_str(&s);
                }
            };
            for piece in ["Hel", "lo"] {
                let chunk: OaiChunk =
                    serde_json::from_value(json!({"choices":[{"delta":{"content":piece}}]})).unwrap();
                apply_chunk(chunk, &mut acc, &mut sink);
            }
        }
        assert_eq!(acc.content, "Hello");
        assert_eq!(seen, "Hello");
    }
}
