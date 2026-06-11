//! Native Ollama `/api/chat` client.
//!
//! Talks Ollama's native API (NOT the OpenAI-compat shim) so we get:
//! - tool results correlated by `tool_name` (the native protocol has no `tool_call_id`;
//!   newer servers attach an optional `id` to calls, which we preserve round-trip)
//! - `arguments` as parsed JSON objects (never partial JSON strings)
//! - the separate `thinking` field on messages
//! - per-request `options.num_ctx` so tool schemas are never silently truncated
//!   (Ollama truncates the prompt FROM THE FRONT with no API error when the
//!   prompt exceeds num_ctx — the single biggest cause of "local tool calling
//!   mysteriously broken"; see `ChatOutcome::truncation_suspected`)

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ollama api error: {0}")]
    Api(String),
    #[error("invalid json from server: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Reasoning text for thinking-capable models. Streamed separately from content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// On role=tool messages: which tool this result answers (native API uses
    /// the name, not an id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into(), thinking: None, tool_calls: vec![], tool_name: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), thinking: None, tool_calls: vec![], tool_name: None }
    }
    pub fn tool_result(tool_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            thinking: None,
            tool_calls: vec![],
            tool_name: Some(tool_name.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Present on newer Ollama servers; absent on older ones. Preserved round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    pub name: String,
    /// Always a parsed JSON object in the native API (never a string).
    #[serde(default)]
    pub arguments: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

impl ToolDef {
    pub fn function(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            kind: "function".into(),
            function: ToolFunctionDef { name: name.into(), description: description.into(), parameters },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    /// None = let the server default (thinking models default to on).
    /// Sending `think:true` to a non-thinking model is a 400, so only set
    /// this after checking capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
}

/// One NDJSON line of a /api/chat response (streaming or the single
/// non-streaming object).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub load_duration: Option<u64>,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub prompt_eval_duration: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub eval_duration: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatStats {
    pub prompt_eval_count: u64,
    pub eval_count: u64,
    /// nanoseconds
    pub total_duration: u64,
    /// nanoseconds
    pub eval_duration: u64,
}

impl ChatStats {
    pub fn tokens_per_sec(&self) -> f64 {
        if self.eval_duration == 0 {
            return 0.0;
        }
        self.eval_count as f64 / (self.eval_duration as f64 / 1e9)
    }
}

/// Incremental events surfaced during a streaming chat call.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    Thinking(String),
    Content(String),
    /// Tool calls always arrive whole (arguments fully parsed), never as
    /// partial JSON fragments.
    ToolCall(ToolCall),
}

#[derive(Debug, Clone)]
pub struct ChatOutcome {
    /// The fully accumulated assistant message (thinking + content + tool calls).
    pub message: Message,
    pub done_reason: Option<String>,
    pub stats: ChatStats,
    /// True when the server reports it evaluated ~num_ctx prompt tokens, which
    /// strongly suggests the front of the prompt (system + tool schemas) was
    /// silently dropped. Callers should warn and compact.
    pub truncation_suspected: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShowResponse {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub model_info: Option<Value>,
}

impl ShowResponse {
    pub fn supports(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
    /// Architecture-prefixed key, e.g. "gemma4.context_length".
    pub fn context_length(&self) -> Option<u64> {
        let info = self.model_info.as_ref()?.as_object()?;
        info.iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| v.as_u64())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

#[derive(Clone)]
pub struct OllamaClient {
    base_url: String,
    http: reqwest::Client,
}

impl OllamaClient {
    pub fn new(base_url: impl AsRef<str>) -> Self {
        let mut base = base_url.as_ref().trim_end_matches('/').to_string();
        if !base.starts_with("http") {
            base = format!("http://{base}");
        }
        Self { base_url: base, http: reqwest::Client::new() }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn tags(&self) -> Result<Vec<ModelEntry>, OllamaError> {
        let resp = self.http.get(format!("{}/api/tags", self.base_url)).send().await?;
        let resp = Self::check(resp).await?;
        let tags: TagsResponse = resp.json().await?;
        Ok(tags.models)
    }

    pub async fn show(&self, model: &str) -> Result<ShowResponse, OllamaError> {
        let resp = self
            .http
            .post(format!("{}/api/show", self.base_url))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json().await?)
    }

    /// Streaming chat. `on_delta` fires for every thinking/content fragment and
    /// each (complete) tool call as it arrives; the accumulated message is
    /// returned at the end, ready to append to history verbatim.
    pub async fn chat_stream<F>(&self, req: &ChatRequest, mut on_delta: F) -> Result<ChatOutcome, OllamaError>
    where
        F: FnMut(StreamDelta),
    {
        let resp = self.http.post(format!("{}/api/chat", self.base_url)).json(req).send().await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::api_error(body));
        }

        let mut acc = Message { role: Role::Assistant, content: String::new(), thinking: None, tool_calls: vec![], tool_name: None };
        let mut stats = ChatStats::default();
        let mut done_reason = None;

        let mut handle_line = |line: &str,
                               acc: &mut Message,
                               stats: &mut ChatStats,
                               done_reason: &mut Option<String>|
         -> Result<(), OllamaError> {
            let line = line.trim();
            if line.is_empty() {
                return Ok(());
            }
            let value: Value = serde_json::from_str(line)?;
            if let Some(err) = value.get("error").and_then(|e| e.as_str()) {
                return Err(OllamaError::Api(err.to_string()));
            }
            let chunk: ChatChunk = serde_json::from_value(value)?;
            if let Some(msg) = &chunk.message {
                if let Some(t) = &msg.thinking {
                    if !t.is_empty() {
                        acc.thinking.get_or_insert_with(String::new).push_str(t);
                        on_delta(StreamDelta::Thinking(t.clone()));
                    }
                }
                if !msg.content.is_empty() {
                    acc.content.push_str(&msg.content);
                    on_delta(StreamDelta::Content(msg.content.clone()));
                }
                for tc in &msg.tool_calls {
                    acc.tool_calls.push(tc.clone());
                    on_delta(StreamDelta::ToolCall(tc.clone()));
                }
            }
            if chunk.done {
                *done_reason = chunk.done_reason.clone();
                *stats = ChatStats {
                    prompt_eval_count: chunk.prompt_eval_count.unwrap_or(0),
                    eval_count: chunk.eval_count.unwrap_or(0),
                    total_duration: chunk.total_duration.unwrap_or(0),
                    eval_duration: chunk.eval_duration.unwrap_or(0),
                };
            }
            Ok(())
        };

        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            // NDJSON: one complete JSON object per line.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                handle_line(&line, &mut acc, &mut stats, &mut done_reason)?;
            }
        }
        // Non-streaming responses (and a final unterminated line) arrive
        // without a trailing newline — flush whatever is left.
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).to_string();
            handle_line(&line, &mut acc, &mut stats, &mut done_reason)?;
        }

        let num_ctx = req.options.as_ref().and_then(|o| o.num_ctx).unwrap_or(4096);
        // prompt_eval_count within ~2% of num_ctx ⇒ the prompt almost certainly
        // overflowed and was front-truncated by the server.
        let truncation_suspected = stats.prompt_eval_count > 0 && stats.prompt_eval_count >= num_ctx.saturating_sub(num_ctx / 50);

        Ok(ChatOutcome { message: acc, done_reason, stats, truncation_suspected })
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, OllamaError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Self::api_error(body))
    }

    fn api_error(body: String) -> OllamaError {
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or(body);
        OllamaError::Api(msg)
    }
}

/// Fallback for the well-documented failure mode where a model emits its tool
/// call as plain JSON text in `content` instead of a structured `tool_calls`
/// entry (Ollama's template-based parser misses it for some models/templates).
/// Recognizes `{"name": ..., "arguments"|"parameters": {...}}`, an array of
/// those, and code-fenced variants — but only for names in `known_tools` so we
/// never misfire on ordinary JSON the model is just talking about.
pub fn extract_textual_tool_calls(content: &str, known_tools: &[String]) -> Vec<ToolCall> {
    let mut text = content.trim();
    if let Some(stripped) = strip_code_fence(text) {
        text = stripped;
    }
    if !(text.starts_with('{') || text.starts_with('[')) {
        return vec![];
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    let candidates: Vec<&Value> = match &value {
        Value::Array(items) => items.iter().collect(),
        v @ Value::Object(_) => vec![v],
        _ => return vec![],
    };
    let mut calls = Vec::new();
    for c in candidates {
        let Some(obj) = c.as_object() else { return vec![] };
        let Some(name) = obj.get("name").and_then(|n| n.as_str()) else { return vec![] };
        if !known_tools.iter().any(|t| t == name) {
            return vec![];
        }
        let args = obj
            .get("arguments")
            .or_else(|| obj.get("parameters"))
            .and_then(|a| a.as_object())
            .cloned()
            .unwrap_or_default();
        calls.push(ToolCall {
            id: None,
            function: ToolCallFunction { index: Some(calls.len() as i64), name: name.to_string(), arguments: args },
        });
    }
    calls
}

fn strip_code_fence(text: &str) -> Option<&str> {
    let text = text.trim();
    let rest = text.strip_prefix("```")?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    let rest = rest.strip_suffix("```")?;
    Some(rest.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<String> {
        vec!["read".into(), "bash".into()]
    }

    #[test]
    fn textual_tool_call_object() {
        let calls = extract_textual_tool_calls(r#"{"name": "read", "arguments": {"path": "a.rs"}}"#, &known());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
    }

    #[test]
    fn textual_tool_call_fenced_array_with_parameters_key() {
        let text = "```json\n[{\"name\": \"bash\", \"parameters\": {\"command\": \"ls\"}}]\n```";
        let calls = extract_textual_tool_calls(text, &known());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "bash");
    }

    #[test]
    fn ignores_unknown_names_and_plain_json() {
        assert!(extract_textual_tool_calls(r#"{"name": "nope", "arguments": {}}"#, &known()).is_empty());
        assert!(extract_textual_tool_calls(r#"{"key": "value"}"#, &known()).is_empty());
        assert!(extract_textual_tool_calls("just prose", &known()).is_empty());
    }

    #[test]
    fn tool_result_serializes_with_tool_name() {
        let msg = Message::tool_result("read", "file contents");
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_name"], "read");
        assert!(v.get("tool_calls").is_none());
    }
}
