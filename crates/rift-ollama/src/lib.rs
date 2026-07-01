//! Native Ollama `/api/chat` client — one `rift_provider::Provider` implementation.
//!
//! Talks Ollama's native API (NOT the OpenAI-compat shim) so we get tool results
//! correlated by `tool_name`, `arguments` as parsed JSON objects, the separate
//! `thinking` field, and per-request `options.num_ctx` (Ollama silently
//! front-truncates prompts over num_ctx; see `ChatOutcome::truncation_suspected`).

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;

// Backend-neutral wire types + the Provider trait live in rift-provider; re-export
// them so `rift_ollama::Message` etc. keep resolving for existing callers.
pub use rift_provider::*;

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ollama api error: {0}")]
    Api(String),
    #[error("invalid json from server: {0}")]
    Json(#[from] serde_json::Error),
}

/// One NDJSON line of a /api/chat response (streaming or the single
/// non-streaming object). Ollama-specific wire shape.
#[derive(Debug, Clone, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    total_duration: Option<u64>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    eval_duration: Option<u64>,
}

/// Ollama's /api/show response, converted to the neutral `ModelCapabilities`.
#[derive(Debug, Clone, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model_info: Option<Value>,
}

impl ShowResponse {
    /// Architecture-prefixed key, e.g. "gemma4.context_length".
    fn context_length(&self) -> Option<u64> {
        let info = self.model_info.as_ref()?.as_object()?;
        info.iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| v.as_u64())
    }
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

#[async_trait]
impl Provider for OllamaClient {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn tags(&self) -> Result<Vec<ModelEntry>> {
        let resp = self.http.get(format!("{}/api/tags", self.base_url)).send().await?;
        let resp = Self::check(resp).await?;
        let tags: TagsResponse = resp.json().await?;
        Ok(tags.models)
    }

    async fn show(&self, model: &str) -> Result<ModelCapabilities> {
        let resp = self
            .http
            .post(format!("{}/api/show", self.base_url))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        let show: ShowResponse = resp.json().await?;
        Ok(ModelCapabilities { context_length: show.context_length(), capabilities: show.capabilities })
    }

    /// Streaming chat. `on_delta` fires for every thinking/content fragment and
    /// each (complete) tool call as it arrives; the accumulated message is
    /// returned at the end, ready to append to history verbatim.
    async fn chat_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatOutcome> {
        let resp = self.http.post(format!("{}/api/chat", self.base_url)).json(req).send().await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::api_error(body).into());
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
        // Non-streaming responses (and a final unterminated line) arrive without
        // a trailing newline — flush whatever is left.
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).to_string();
            handle_line(&line, &mut acc, &mut stats, &mut done_reason)?;
        }

        let num_ctx = req.options.as_ref().and_then(|o| o.num_ctx).unwrap_or(4096);
        // prompt_eval_count within ~2% of num_ctx ⇒ the prompt almost certainly
        // overflowed and was front-truncated by the server.
        let truncation_suspected =
            stats.prompt_eval_count > 0 && stats.prompt_eval_count >= num_ctx.saturating_sub(num_ctx / 50);

        Ok(ChatOutcome { message: acc, done_reason, stats, truncation_suspected })
    }
}
