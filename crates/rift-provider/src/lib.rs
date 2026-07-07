//! Provider abstraction: the trait rift's agent loop, compactor, and swarm talk
//! to, plus the backend-neutral wire types they exchange. `OllamaClient` (in
//! rift-ollama) is one implementation; an OpenAI-compatible one is next.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// On role=tool messages: which tool this result answers (Ollama's native
    /// correlation, by name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// On role=tool messages: the tool-call id this result answers. OpenAI-compat
    /// providers require it; Ollama ignores it. Threaded from `ToolCall::id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider-specific raw payload for lossless round-trips. rift-anthropic
    /// stores the assistant's raw content blocks here (thinking blocks carry
    /// signatures the API validates on replay — the neutral fields can't
    /// represent them). Other providers ignore it and build requests from the
    /// neutral fields, so cross-provider switches keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_data: Option<Value>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            thinking: None,
            tool_calls: vec![],
            tool_name: None,
            tool_call_id: None,
            provider_data: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            thinking: None,
            tool_calls: vec![],
            tool_name: None,
            tool_call_id: None,
            provider_data: None,
        }
    }
    pub fn tool_result(tool_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            thinking: None,
            tool_calls: vec![],
            tool_name: Some(tool_name.into()),
            tool_call_id: None,
            provider_data: None,
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
    /// Always a parsed JSON object (never a string).
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
    /// None = let the server default (thinking models default to on). Sending
    /// `think:true` to a non-thinking model is a 400, so only set after a
    /// capability check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    /// Reasoning effort level ("low"/"medium"/"high"/"max"/…), for models
    /// that grade their thinking. Never serialized directly — each provider
    /// translates it to its own wire form (Ollama: string `think`; OpenAI:
    /// `reasoning_effort`; Anthropic-format: `output_config.effort`).
    #[serde(skip_serializing)]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
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
    /// Tool calls always arrive whole (arguments fully parsed).
    ToolCall(ToolCall),
}

#[derive(Debug, Clone)]
pub struct ChatOutcome {
    /// The fully accumulated assistant message (thinking + content + tool calls).
    pub message: Message,
    pub done_reason: Option<String>,
    pub stats: ChatStats,
    /// True when the server reports it evaluated ~num_ctx prompt tokens, which
    /// strongly suggests the front of the prompt was silently truncated.
    pub truncation_suspected: bool,
}

/// One model as reported by a provider's model list.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Backend-neutral capability info for a single model.
#[derive(Debug, Clone, Default)]
pub struct ModelCapabilities {
    pub capabilities: Vec<String>,
    pub context_length: Option<u64>,
}

impl ModelCapabilities {
    pub fn supports(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
    pub fn context_length(&self) -> Option<u64> {
        self.context_length
    }
}

/// A model backend. `OllamaClient` implements this; other providers (e.g. an
/// OpenAI-compatible one) will too. Object-safe so the agent can hold an
/// `Arc<dyn Provider>` and the swarm can share one across candidates.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Endpoint this provider talks to (for display in the UI).
    fn base_url(&self) -> &str;

    /// List available models.
    async fn tags(&self) -> Result<Vec<ModelEntry>>;

    /// Capabilities (tools/thinking) + context length for one model. Providers
    /// that can't probe should return their best-known defaults.
    async fn show(&self, model: &str) -> Result<ModelCapabilities>;

    /// Streaming chat. `on_delta` fires for every thinking/content fragment and
    /// each complete tool call; the accumulated message is returned at the end.
    async fn chat_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatOutcome>;
}

// ---- shared HTTP/streaming plumbing for provider implementations ----------

/// HTTP client with sane timeouts for streaming LLM traffic: bounded connect
/// and per-read idle timeouts, but no whole-request timeout (that would kill
/// long generations mid-stream). `reqwest::Client::new()` has NO timeouts at
/// all — a hung server would stall a turn forever.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Canonicalize a user-supplied endpoint: trim the trailing slash and default
/// the scheme to `http://` (LAN/localhost servers are the common case).
pub fn normalize_base_url(url: &str) -> String {
    let base = url.trim_end_matches('/');
    if base.starts_with("http") {
        base.to_string()
    } else {
        format!("http://{base}")
    }
}

/// Human-readable message from an API error body. Handles `{"error": "..."}`
/// (Ollama) and `{"error": {"message": "..."}}` (OpenAI), falling back to the
/// raw body — truncated so an HTML 502 page from a reverse proxy stays legible.
pub fn api_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            e.as_str()
                .map(str::to_string)
                .or_else(|| e.get("message").and_then(|m| m.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(600).collect())
}

/// Send a request, retrying transient transport failures (connection
/// refused/reset, timeouts) with exponential backoff. Only the initial send
/// is ever retried — never a stream mid-read — so this is safe for streaming
/// endpoints: nothing has been consumed when a send fails. Non-2xx responses
/// are returned untouched for the caller to interpret.
pub async fn send_with_retry(builder: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    const ATTEMPTS: u32 = 3;
    let mut delay = std::time::Duration::from_millis(400);
    for _ in 1..ATTEMPTS {
        match builder.try_clone() {
            // Non-replayable body: fall through to the single real attempt.
            None => break,
            Some(b) => match b.send().await {
                Err(e) if e.is_connect() || e.is_timeout() => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            },
        }
    }
    builder.send().await
}

/// Control flow for [`for_each_line`] callbacks.
pub enum LineFlow {
    Continue,
    /// Stop consuming the stream (e.g. after an SSE `[DONE]` sentinel).
    Break,
}

/// Feed every newline-terminated line of a byte stream to `f`, then the
/// unterminated tail if any. The tail flush matters: non-streaming bodies and
/// servers that close without a trailing newline (or without SSE `[DONE]`)
/// put their final — often stats-carrying — line there.
pub async fn for_each_line<S, B, E, F>(mut stream: S, mut f: F) -> Result<()>
where
    S: futures_util::Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<anyhow::Error>,
    F: FnMut(&str) -> Result<LineFlow>,
{
    use futures_util::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(chunk.map_err(Into::into)?.as_ref());
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
            buf.drain(..=pos);
            if matches!(f(line.trim())?, LineFlow::Break) {
                return Ok(());
            }
        }
    }
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).into_owned();
        if !line.trim().is_empty() {
            f(line.trim())?;
        }
    }
    Ok(())
}

/// Fallback for the failure mode where a model emits its tool call as plain JSON
/// text in `content` instead of a structured `tool_calls` entry. Recognizes
/// `{"name": ..., "arguments"|"parameters": {...}}`, an array of those, and
/// code-fenced variants — but only for names in `known_tools` so it never
/// misfires on ordinary JSON the model is just talking about.
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

/// Mock HTTP server for the per-provider hardening test suites (rift-ollama
/// and rift-openai integration tests). Serves canned responses one
/// connection at a time, records raw requests, and frames bodies by
/// connection-close so streaming chunk boundaries land exactly where a test
/// puts them. Not part of the public API.
#[doc(hidden)]
pub mod test_support {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    pub struct MockResponse {
        pub status: u16,
        pub content_type: &'static str,
        /// Written with a flush (and a small delay) between chunks, so each
        /// chunk arrives as its own network read on the client side.
        pub chunks: Vec<String>,
    }

    impl MockResponse {
        pub fn json(status: u16, body: &str) -> Self {
            Self { status, content_type: "application/json", chunks: vec![body.to_string()] }
        }
        /// 200 stream (SSE or NDJSON — the client decides how to parse).
        pub fn stream(chunks: &[&str]) -> Self {
            Self {
                status: 200,
                content_type: "text/event-stream",
                chunks: chunks.iter().map(|c| c.to_string()).collect(),
            }
        }
    }

    pub struct MockServer {
        pub base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        /// Serve `responses` in order, one per connection.
        pub async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
            let recorded = requests.clone();
            tokio::spawn(async move {
                for resp in responses {
                    let Ok((mut sock, _)) = listener.accept().await else { return };
                    let raw = read_request(&mut sock).await;
                    recorded.lock().await.push(raw);
                    let head = format!(
                        "HTTP/1.1 {} MOCK\r\ncontent-type: {}\r\nconnection: close\r\n\r\n",
                        resp.status, resp.content_type
                    );
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        continue;
                    }
                    for chunk in &resp.chunks {
                        if sock.write_all(chunk.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = sock.flush().await;
                        // Let the client observe this chunk as a separate read.
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    let _ = sock.shutdown().await;
                }
            });
            Self { base_url, requests }
        }

        /// Raw request texts (start-line + headers + body), in arrival order.
        pub async fn requests(&self) -> Vec<String> {
            self.requests.lock().await.clone()
        }
    }

    async fn read_request(sock: &mut TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let need = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + need {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
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

    fn byte_stream(
        chunks: &[&str],
    ) -> impl futures_util::Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> + Unpin {
        futures_util::stream::iter(
            chunks.iter().map(|c| Ok(c.as_bytes().to_vec())).collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn for_each_line_reassembles_split_lines_and_flushes_tail() {
        // Lines split across network reads; the last line has no trailing
        // newline (a server that closes without one) and must still arrive.
        let stream = byte_stream(&["first ", "line\nsec", "ond\ntail no newline"]);
        let mut seen = vec![];
        for_each_line(stream, |line| {
            seen.push(line.to_string());
            Ok(LineFlow::Continue)
        })
        .await
        .unwrap();
        assert_eq!(seen, ["first line", "second", "tail no newline"]);
    }

    #[tokio::test]
    async fn for_each_line_break_stops_early() {
        let stream = byte_stream(&["a\nSTOP\nnever\n"]);
        let mut seen = vec![];
        for_each_line(stream, |line| {
            seen.push(line.to_string());
            Ok(if line == "STOP" { LineFlow::Break } else { LineFlow::Continue })
        })
        .await
        .unwrap();
        assert_eq!(seen, ["a", "STOP"]);
    }
}
