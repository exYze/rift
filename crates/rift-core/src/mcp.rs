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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpClient {
    pub name: String,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: AtomicI64,
    _child: Child,
}

impl McpClient {
    /// Spawn the server process and run the initialize handshake.
    pub async fn spawn(name: &str, cfg: &McpServerConfig) -> Result<Arc<Self>> {
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
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            next_id: AtomicI64::new(1),
            _child: child,
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
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(serde_json::to_string(msg)?.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // Every early exit must remove the pending entry, or a wedged server
        // (accepts requests, never answers) grows the map for the whole
        // session — one dead oneshot per 60s-timeout call.
        let outcome = async {
            self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
            tokio::time::timeout(REQUEST_TIMEOUT, rx)
                .await
                .map_err(|_| anyhow!("MCP server '{}' timed out on {method}", self.name))?
                .map_err(|_| anyhow!("MCP server '{}' closed during {method}", self.name))
        }
        .await;
        let resp = match outcome {
            Ok(resp) => resp,
            Err(e) => {
                self.pending.lock().await.remove(&id);
                return Err(e);
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
