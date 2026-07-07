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

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rift_provider::{
    api_error_message, for_each_line, http_client, normalize_base_url, send_with_retry,
    ChatOutcome, ChatRequest, ChatStats, LineFlow, Message, ModelCapabilities, ModelEntry,
    Provider, Role, StreamDelta, ToolCall, ToolCallFunction,
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
        let mut base = normalize_base_url(base_url.as_ref());
        if !base.ends_with("/v1") {
            base = format!("{base}/v1");
        }
        Self { base_url: base, api_key, http: http_client() }
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
    /// `None` after a retry against servers that reject the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// Reasoning effort ("low"/"medium"/"high"/"max"…): OpenAI o-series
    /// syntax, also spoken by DeepSeek (which maps low/medium→high,
    /// xhigh→max). Cleared and retried when a server rejects it.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// DeepSeek's thinking toggle ({"type": "enabled"/"disabled"}). Only
    /// sent when the user explicitly set a thinking mode; cleared and
    /// retried when a server rejects it.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    /// vLLM routes the same controls through the chat template instead
    /// (per the DeepSeek-V4 vLLM recipe: {"thinking": bool,
    /// "reasoning_effort": "high"/"max"}). Sent alongside the top-level
    /// fields — each server reads its own form and ignores or (via the 400
    /// retry) sheds the other.
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct OaiMessage {
    role: &'static str,
    /// A plain string normally; an array of content parts (text +
    /// image_url) for user messages carrying vision attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OaiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Reasoning models (DeepSeek) require their chain-of-thought passed
    /// back during a tool-call loop; the agent already keeps thinking only
    /// on the current turn's messages, so presence here is exactly the
    /// "with tool calls" case. Absent for models that never produce it.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
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
            // Ids are normally guaranteed: the agent synthesizes one for any
            // call the provider left id-less, and session load backfills old
            // histories. The fallback is a last resort and may not match the
            // paired tool result.
            id: tc.id.clone().unwrap_or_else(|| format!("call_{i}")),
            kind: "function",
            function: OaiToolCallFn {
                name: tc.function.name.clone(),
                arguments: serde_json::to_string(&tc.function.arguments).unwrap_or_else(|_| "{}".into()),
            },
        })
        .collect();
    // Assistant messages that are pure tool calls send content: null; user
    // messages with vision attachments become text + image_url parts.
    let content = if m.role == Role::User && !m.images.is_empty() {
        let mut parts = vec![json!({"type": "text", "text": m.content})];
        for url in &m.images {
            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
        }
        Some(Value::Array(parts))
    } else if m.content.is_empty() && !tool_calls.is_empty() {
        None
    } else {
        Some(Value::String(m.content.clone()))
    };
    OaiMessage {
        role: role_str(m.role),
        content,
        tool_calls,
        // A tool result needs the id it answers; ids are synthesized by the
        // agent when missing, so the name fallback is a last resort only.
        tool_call_id: if m.role == Role::Tool {
            m.tool_call_id.clone().or_else(|| m.tool_name.clone())
        } else {
            None
        },
        name: if m.role == Role::Tool { m.tool_name.clone() } else { None },
        reasoning_content: if m.role == Role::Assistant {
            m.thinking.clone().filter(|t| !t.is_empty())
        } else {
            None
        },
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
        // DeepSeek rejects sampling params in thinking mode — drop
        // temperature whenever reasoning is explicitly requested.
        temperature: if req.effort.is_some() || req.think == Some(true) {
            None
        } else {
            req.options.as_ref().and_then(|o| o.temperature)
        },
        // Ollama's num_predict maps to OpenAI's max_tokens; num_ctx has no
        // equivalent (context is fixed per model).
        max_tokens: req.options.as_ref().and_then(|o| o.num_predict),
        stream_options: Some(StreamOptions { include_usage: true }),
        reasoning_effort: req.effort.clone(),
        // Only an EXPLICIT user choice travels: unknown body fields are a
        // 400 on some servers, and None means "server default" anyway.
        thinking: req.think.map(|on| json!({"type": if on { "enabled" } else { "disabled" }})),
        chat_template_kwargs: {
            let mut kw = serde_json::Map::new();
            if let Some(e) = &req.effort {
                // A set effort implies thinking on.
                kw.insert("thinking".into(), json!(true));
                kw.insert("reasoning_effort".into(), json!(e));
            } else if let Some(on) = req.think {
                kw.insert("thinking".into(), json!(on));
            }
            (!kw.is_empty()).then_some(Value::Object(kw))
        },
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

fn preview(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Parse an accumulated tool-argument string. Empty means "no arguments"
/// (models send nothing for zero-arg tools); some servers double-encode the
/// object as a JSON string, so unwrap one level of that.
fn parse_call_arguments(raw: &str) -> Result<serde_json::Map<String, Value>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(raw)? {
        Value::Object(m) => Ok(m),
        Value::String(inner) => match serde_json::from_str::<Value>(&inner)? {
            Value::Object(m) => Ok(m),
            _ => bail!("arguments are not a JSON object"),
        },
        _ => bail!("arguments are not a JSON object"),
    }
}

/// More parallel tool calls than any real model emits; `tool_calls[].index`
/// comes off the wire, so an implausible value means a broken (or hostile)
/// server — reject it instead of `resize_with`-allocating gigabytes.
const MAX_TOOL_CALLS: usize = 128;

/// Apply one streamed chunk to the running accumulators, emitting content/
/// thinking deltas as they arrive.
fn apply_chunk(
    chunk: OaiChunk,
    acc: &mut StreamAcc,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<()> {
    if let Some(u) = chunk.usage {
        if u.prompt_tokens > 0 {
            acc.prompt_tokens = u.prompt_tokens;
        }
        if u.completion_tokens > 0 {
            acc.completion_tokens = u.completion_tokens;
        }
    }
    let Some(choice) = chunk.choices.into_iter().next() else { return Ok(()) };
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
        if tc.index >= MAX_TOOL_CALLS {
            bail!("server sent tool_calls index {} (max {MAX_TOOL_CALLS})", tc.index);
        }
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
    Ok(())
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
        let resp = send_with_retry(self.req(reqwest::Method::GET, "/models")).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{status}: {}", api_error_message(&text)));
        }
        let models: Models = resp.json().await?;
        Ok(models.data.into_iter().map(|m| ModelEntry { name: m.id, capabilities: vec![] }).collect())
    }

    async fn show(&self, model: &str) -> Result<ModelCapabilities> {
        // No /show in the OpenAI protocol. Assume tool + thinking support
        // (undetectable here; explicitly-set reasoning params degrade
        // gracefully via the 400 retry), and recover the context length from
        // the /models listing where servers expose it — vLLM
        // (`max_model_len`), OpenRouter (`context_length`), LM Studio
        // (`max_context_length`), llama.cpp (`meta.n_ctx_train`). Absent or
        // unreachable just means unknown.
        let context_length = async {
            let resp = send_with_retry(self.req(reqwest::Method::GET, "/models")).await.ok()?;
            let body: Value = resp.json().await.ok()?;
            let entry = body
                .get("data")?
                .as_array()?
                .iter()
                .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model))?;
            for key in ["max_model_len", "context_length", "max_context_length", "context_window"] {
                if let Some(n) = entry.get(key).and_then(|v| v.as_u64()) {
                    return Some(n);
                }
            }
            entry.get("meta")?.get("n_ctx_train")?.as_u64()
        }
        .await
        .filter(|n| *n > 0);
        Ok(ModelCapabilities { capabilities: vec!["tools".into(), "thinking".into()], context_length })
    }

    async fn chat_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatOutcome> {
        let mut body = build_request(req);
        let mut resp = send_with_retry(self.req(reqwest::Method::POST, "/chat/completions").json(&body)).await?;
        // Parameter-compat fallback: servers differ on which optional params
        // they accept (older ones 400 on stream_options; non-reasoning ones
        // on reasoning_effort/thinking). Drop exactly the params the error
        // names and retry once, so a mixed fleet still works.
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let text = resp.text().await.unwrap_or_default();
            let mut retry = false;
            if body.stream_options.is_some() && text.contains("stream_options") {
                body.stream_options = None;
                retry = true;
            }
            if (body.reasoning_effort.is_some() && text.contains("reasoning_effort"))
                || (body.thinking.is_some() && text.contains("thinking"))
                || (body.chat_template_kwargs.is_some() && text.contains("chat_template_kwargs"))
            {
                body.reasoning_effort = None;
                body.thinking = None;
                body.chat_template_kwargs = None;
                retry = true;
            }
            if !retry {
                return Err(anyhow!("openai api error (400 Bad Request): {}", api_error_message(&text)));
            }
            resp = send_with_retry(self.req(reqwest::Method::POST, "/chat/completions").json(&body)).await?;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("openai api error ({status}): {}", api_error_message(&text)));
        }

        let mut acc = StreamAcc::default();
        // The OpenAI protocol carries no timing, so measure it: prefill ends
        // when the first data chunk arrives, decoding runs from there to the
        // end of the stream. Feeds the tok/s display instead of a flat 0.0.
        let started = std::time::Instant::now();
        let mut first_chunk: Option<std::time::Instant> = None;

        // Server-Sent Events: `data: {json}` lines, terminated by `data: [DONE]`.
        for_each_line(resp.bytes_stream(), |line| {
            let Some(payload) = line.strip_prefix("data:") else { return Ok(LineFlow::Continue) };
            let payload = payload.trim();
            if payload == "[DONE]" {
                return Ok(LineFlow::Break);
            }
            if payload.is_empty() {
                return Ok(LineFlow::Continue);
            }
            first_chunk.get_or_insert_with(std::time::Instant::now);
            let value: Value = serde_json::from_str(payload)
                .with_context(|| format!("malformed stream event: {}", preview(payload, 200)))?;
            // Mid-stream failures (rate limits, upstream errors on OpenRouter/
            // LiteLLM/vLLM) arrive as an `error` event; surface them instead of
            // returning a silently truncated message.
            if let Some(err) = value.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .or_else(|| err.as_str().map(str::to_string))
                    .unwrap_or_else(|| err.to_string());
                bail!("openai api error (mid-stream): {msg}");
            }
            let parsed: OaiChunk = serde_json::from_value(value).context("unexpected stream event shape")?;
            apply_chunk(parsed, &mut acc, on_delta)?;
            Ok(LineFlow::Continue)
        })
        .await?;

        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (i, c) in acc.calls.into_iter().enumerate() {
            if c.name.is_empty() {
                continue;
            }
            let arguments = parse_call_arguments(&c.args).map_err(|e| {
                let truncated = if acc.done_reason.as_deref() == Some("length") {
                    " (output truncated by the token limit)"
                } else {
                    ""
                };
                anyhow!(
                    "model emitted invalid JSON arguments for tool '{}'{truncated}: {e}; raw: {}",
                    c.name,
                    preview(&c.args, 300)
                )
            })?;
            let call = ToolCall {
                id: (!c.id.is_empty()).then_some(c.id),
                function: ToolCallFunction { index: Some(i as i64), name: c.name, arguments },
            };
            on_delta(StreamDelta::ToolCall(call.clone()));
            tool_calls.push(call);
        }

        let message = Message {
            role: Role::Assistant,
            content: acc.content,
            thinking: (!acc.thinking.is_empty()).then_some(acc.thinking),
            tool_calls,
            tool_name: None,
            tool_call_id: None,
            provider_data: None,
            images: vec![],
        };
        let stats = ChatStats {
            prompt_eval_count: acc.prompt_tokens,
            eval_count: acc.completion_tokens,
            total_duration: started.elapsed().as_nanos() as u64,
            eval_duration: first_chunk.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0),
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
        let mut assistant = Message {
            role: Role::Assistant,
            content: String::new(),
            thinking: None,
            tool_calls: vec![],
            tool_name: None,
            tool_call_id: None,
            provider_data: None,
            images: vec![],
        };
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
        assert_eq!(oai.content, Some(Value::String("contents".into())));
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
            apply_chunk(chunk, &mut acc, &mut sink).unwrap();
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
                apply_chunk(chunk, &mut acc, &mut sink).unwrap();
            }
        }
        assert_eq!(acc.content, "Hello");
        assert_eq!(seen, "Hello");
    }
}
