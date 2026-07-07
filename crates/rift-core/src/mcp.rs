//! Minimal Model Context Protocol client (stdio transport).
//!
//! Speaks JSON-RPC 2.0 over a child process's stdin/stdout, newline-delimited:
//! `initialize` → `notifications/initialized` → `tools/list`, then
//! `tools/call` per invocation. Server-initiated requests (sampling etc.) are
//! answered with method-not-found; notifications are ignored — that's all a
//! tool-using client needs, and it keeps the dependency count at zero.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::tools::{Tool, ToolCtx};

#[cfg(test)]
mod http_tests {
    use super::*;
    use rift_provider::test_support::{MockResponse, MockServer};

    // The streamable-HTTP transport: initialize captures the session id,
    // the initialized notification is fire-and-forget, and tools/list
    // parses both plain-JSON and SSE-framed responses.
    #[tokio::test]
    async fn http_transport_handshakes_and_lists_tools() {
        let server = MockServer::start(vec![
            MockResponse::json(
                200,
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\"}}}",
            ),
            MockResponse::json(202, ""),
            MockResponse {
                status: 200,
                content_type: "text/event-stream",
                chunks: vec![
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"gets a url\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n".into(),
                ],
            },
        ])
        .await;
        let cfg = McpServerConfig {
            command: String::new(),
            args: vec![],
            env: Default::default(),
            url: Some(server.base_url.clone()),
            headers: HashMap::from([("x-test".to_string(), "1".to_string())]),
        };
        let client = McpClient::spawn("mock", &cfg).await.expect("handshake");
        let tools = client.list_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "fetch");
        let reqs = server.requests().await;
        assert_eq!(reqs.len(), 3);
        assert!(reqs[0].contains("\"method\":\"initialize\""));
        assert!(reqs[0].to_lowercase().contains("x-test: 1"), "custom header missing");
        assert!(reqs[1].contains("notifications/initialized"));
        assert!(reqs[2].contains("tools/list"));
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    /// Stdio transport: the command to spawn. Empty when `url` is set.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable-HTTP transport: the server endpoint (remote/hosted MCP).
    /// Takes precedence over `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra HTTP headers for `url` servers (e.g. an Authorization bearer).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    /// What this entry connects to, for display and trust prompts.
    pub fn target(&self) -> String {
        match &self.url {
            Some(u) => u.clone(),
            None => format!("{} {}", self.command, self.args.join(" ")).trim().to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpClient {
    pub name: String,
    next_id: AtomicI64,
    transport: Transport,
}

#[allow(clippy::large_enum_variant)] // one per server, boxed behind an Arc
enum Transport {
    Stdio {
        stdin: Mutex<ChildStdin>,
        pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
        _child: Child,
    },
    /// Streamable HTTP: one POST per JSON-RPC message; the server answers
    /// with plain JSON or a one-shot SSE stream, and hands out a session id
    /// on initialize that later requests echo back.
    Http {
        url: String,
        headers: HashMap<String, String>,
        session: tokio::sync::RwLock<Option<String>>,
        http: reqwest::Client,
    },
}

impl McpClient {
    /// Connect a server (stdio spawn or HTTP endpoint) and run the
    /// initialize handshake.
    pub async fn spawn(name: &str, cfg: &McpServerConfig) -> Result<Arc<Self>> {
        if let Some(url) = &cfg.url {
            let client = Arc::new(Self {
                name: name.to_string(),
                next_id: AtomicI64::new(1),
                transport: Transport::Http {
                    url: url.clone(),
                    headers: cfg.headers.clone(),
                    session: tokio::sync::RwLock::new(None),
                    http: rift_provider::http_client(),
                },
            });
            client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "rift", "version": env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await
                .with_context(|| format!("MCP initialize failed for '{name}' at {url}"))?;
            client.notify("notifications/initialized", json!({})).await?;
            return Ok(client);
        }
        if cfg.command.trim().is_empty() {
            bail!("MCP server '{name}' has neither a command nor a url");
        }
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning MCP server '{name}' ({})", cfg.command))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::default();
        let client = Arc::new(Self {
            name: name.to_string(),
            next_id: AtomicI64::new(1),
            transport: Transport::Stdio {
                stdin: Mutex::new(stdin),
                pending: pending.clone(),
                _child: child,
            },
        });

        // Reader: route responses to waiting requests; politely refuse
        // server-initiated requests.
        let reader_client = Arc::downgrade(&client);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let id = v.get("id").cloned();
                let is_request = v.get("method").is_some();
                match (id, is_request) {
                    (Some(id), false) => {
                        if let Some(id) = id.as_i64() {
                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let _ = tx.send(v);
                            }
                        }
                    }
                    (Some(id), true) => {
                        if let Some(client) = reader_client.upgrade() {
                            let _ = client
                                .send_raw(&json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {"code": -32601, "message": "method not supported by this client"}
                                }))
                                .await;
                        }
                    }
                    _ => {} // notification — ignore
                }
            }
        });

        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "rift", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .with_context(|| format!("MCP initialize failed for '{name}'"))?;
        let _ = init; // protocolVersion negotiation: accept whatever the server returns
        client.notify("notifications/initialized", json!({})).await?;
        Ok(client)
    }

    async fn send_raw(&self, msg: &Value) -> Result<()> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut stdin = stdin.lock().await;
                stdin.write_all(serde_json::to_string(msg)?.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
                Ok(())
            }
            Transport::Http { .. } => {
                // Fire-and-forget over HTTP (client error replies): drop it —
                // the server never sees our JSON-RPC errors anyway.
                Ok(())
            }
        }
    }

    /// POST one JSON-RPC message to an HTTP server, returning the response
    /// body (None for accepted-without-body notifications). Captures the
    /// Mcp-Session-Id header the first time the server sends one.
    async fn http_post(&self, msg: &Value) -> Result<Option<Value>> {
        let Transport::Http { url, headers, session, http } = &self.transport else {
            bail!("not an HTTP transport");
        };
        let mut req = http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION);
        if let Some(sid) = session.read().await.clone() {
            req = req.header("mcp-session-id", sid);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, req.json(msg).send())
            .await
            .map_err(|_| anyhow!("MCP server '{}' timed out", self.name))?
            .with_context(|| format!("MCP server '{}' unreachable", self.name))?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.write().await = Some(sid.to_string());
        }
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = tokio::time::timeout(REQUEST_TIMEOUT, resp.text())
            .await
            .map_err(|_| anyhow!("MCP server '{}' timed out reading the response", self.name))??;
        if !status.is_success() {
            bail!("MCP server '{}' returned {status}: {}", self.name, body.chars().take(300).collect::<String>());
        }
        if body.trim().is_empty() {
            return Ok(None); // 202 Accepted (notifications)
        }
        if content_type.contains("text/event-stream") {
            // One-shot SSE: the response to our request is the last data
            // event carrying a result or error.
            let mut answer = None;
            for line in body.lines() {
                let Some(data) = line.strip_prefix("data:") else { continue };
                let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { continue };
                if v.get("result").is_some() || v.get("error").is_some() {
                    answer = Some(v);
                }
            }
            return Ok(answer);
        }
        Ok(Some(serde_json::from_str(&body).with_context(|| {
            format!("MCP server '{}' sent invalid JSON", self.name)
        })?))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio { .. } => self.send_raw(&msg).await,
            Transport::Http { .. } => self.http_post(&msg).await.map(|_| ()),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = match &self.transport {
            Transport::Http { .. } => self
                .http_post(&msg)
                .await?
                .ok_or_else(|| anyhow!("MCP server '{}' sent no response to {method}", self.name))?,
            Transport::Stdio { pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().await.insert(id, tx);
                // Every early exit must remove the pending entry, or a wedged
                // server (accepts requests, never answers) grows the map for
                // the whole session — one dead oneshot per 60s-timeout call.
                let outcome = async {
                    self.send_raw(&msg).await?;
                    tokio::time::timeout(REQUEST_TIMEOUT, rx)
                        .await
                        .map_err(|_| anyhow!("MCP server '{}' timed out on {method}", self.name))?
                        .map_err(|_| anyhow!("MCP server '{}' closed during {method}", self.name))
                }
                .await;
                match outcome {
                    Ok(resp) => resp,
                    Err(e) => {
                        pending.lock().await.remove(&id);
                        return Err(e);
                    }
                }
            }
        };
        if let Some(err) = resp.get("error") {
            bail!(
                "MCP '{}' {method} error: {}",
                self.name,
                err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
            );
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| anyhow!("MCP '{}' returned no tools array", self.name))?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                Some(McpToolInfo {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
            })
            .collect())
    }

    pub async fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        let (text, is_error) = parse_tool_content(&result);
        if is_error {
            bail!("{text}");
        }
        Ok(text)
    }
}

/// Extract (text, is_error) from a tools/call result. Lenient on shape:
/// the spec says `{"content":[blocks], "isError"?}`, but hand-written and
/// model-generated servers commonly return the bare content array — accept
/// it rather than silently reading an empty string (the same zero-errors
/// posture as textual tool-call recovery).
fn parse_tool_content(result: &Value) -> (String, bool) {
    let blocks = result
        .get("content")
        .and_then(|c| c.as_array())
        .or_else(|| result.as_array());
    let text = blocks
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => b.get("text").and_then(|t| t.as_str()).map(str::to_string),
                    Some(other) => Some(format!("[unsupported content block: {other}]")),
                    None => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let is_error = result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
    (text, is_error)
}


/// An MCP server tool exposed through the regular tool registry, namespaced
/// `<server>_<tool>` (the convention models already know from Claude Code).
pub struct McpTool {
    client: Arc<McpClient>,
    qualified_name: String,
    remote_name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, info: McpToolInfo) -> Self {
        let qualified_name = format!("{}_{}", client.name, info.name);
        let description = format!("[{} MCP] {}", client.name, info.description);
        Self { client, qualified_name, remote_name: info.name, description, schema: info.input_schema }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    async fn execute(&self, args: &Map<String, Value>, _ctx: &ToolCtx) -> Result<String> {
        self.client.call_tool(&self.remote_name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_content_accepts_spec_and_bare_array_shapes() {
        // Spec shape.
        let spec = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(parse_tool_content(&spec), ("hello".into(), false));

        // Error flag.
        let err = json!({"content": [{"type": "text", "text": "boom"}], "isError": true});
        assert_eq!(parse_tool_content(&err), ("boom".into(), true));

        // The common near-miss: a bare content array as the whole result.
        let bare = json!([{"type": "text", "text": "lenient"}]);
        assert_eq!(parse_tool_content(&bare), ("lenient".into(), false));

        // Garbage degrades to empty, not a panic.
        assert_eq!(parse_tool_content(&json!(42)), (String::new(), false));
    }
}
