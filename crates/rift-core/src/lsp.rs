//! Minimal Language Server Protocol client (stdio transport), for post-edit
//! diagnostics only.
//!
//! Speaks Content-Length-framed JSON-RPC 2.0 to a child process: `initialize`
//! → `initialized`, then `textDocument/didOpen`/`didChange` per edit, waiting
//! briefly for `textDocument/publishDiagnostics`. Servers spawn lazily on the
//! first edit of a matching file and live for the process lifetime — one per
//! language per workspace root. Everything degrades to None: diagnostics are
//! a bonus appended to a successful edit, never a reason to fail one. Like
//! the MCP client, this keeps the dependency count at zero.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// How long an edit waits for publishDiagnostics before the tool result
/// ships without them. Long enough for syntax-level diagnostics from a warm
/// server; a cold rust-analyzer will simply miss the first edit or two.
const DIAG_WAIT: Duration = Duration::from_millis(1500);
const DIAG_POLL: Duration = Duration::from_millis(50);
/// initialize handshake bound — a wedged server must not stall an edit.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Diagnostic line cap per edit; errors sort first so the cap sheds
/// warnings before errors.
const MAX_LINES: usize = 10;

/// The `lsp` config field: `false` disables diagnostics entirely; a map
/// overrides/adds servers. Keys are the built-in language names (`rust`,
/// `python`, `typescript`, `go`, `c`), or a file extension for languages
/// the registry doesn't know. Absent = enabled with the built-ins.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum LspSetting {
    Enabled(bool),
    Servers(HashMap<String, LspServerOverride>),
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct LspServerOverride {
    /// argv replacing the built-in command; empty = keep the built-in.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub disabled: bool,
}

struct Builtin {
    lang: &'static str,
    exts: &'static [&'static str],
    /// Candidate argvs in preference order; the first whose binary is on
    /// PATH wins (pyright, else pylsp).
    commands: &'static [&'static [&'static str]],
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        lang: "rust",
        exts: &["rs"],
        commands: &[&["rust-analyzer"]],
    },
    Builtin {
        lang: "python",
        exts: &["py"],
        commands: &[&["pyright-langserver", "--stdio"], &["pylsp"]],
    },
    Builtin {
        lang: "typescript",
        exts: &["ts", "tsx", "js", "jsx"],
        commands: &[&["typescript-language-server", "--stdio"]],
    },
    Builtin {
        lang: "go",
        exts: &["go"],
        commands: &[&["gopls"]],
    },
    Builtin {
        lang: "c",
        exts: &["c", "cc", "cpp", "h", "hpp"],
        commands: &[&["clangd"]],
    },
];

fn builtin_for_lang(lang: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.lang == lang)
}

/// The registry language for a file extension: built-ins first, then
/// config-added entries (those are keyed by extension).
fn lang_for_ext(ext: &str, overrides: &HashMap<String, LspServerOverride>) -> Option<String> {
    if let Some(b) = BUILTINS.iter().find(|b| b.exts.contains(&ext)) {
        return Some(b.lang.to_string());
    }
    overrides.contains_key(ext).then(|| ext.to_string())
}

/// LSP languageId for didOpen.
fn language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "c" => "c",
        "cc" | "cpp" | "h" | "hpp" => "cpp",
        other => other,
    }
}

/// `which`-style PATH probe. On Windows also tries the .exe/.cmd/.bat forms
/// npm and pip install (std::process::Command handles .cmd/.bat spawning).
pub(crate) fn find_binary(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

// ---- base-protocol framing --------------------------------------------------

/// Encode one JSON-RPC message with LSP Content-Length framing.
fn encode_frame(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend(body);
    out
}

/// Read one framed message. Ok(None) = clean EOF. Unknown headers are
/// skipped; Content-Length matches case-insensitively (servers vary).
async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("frame without Content-Length"))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(
        serde_json::from_slice(&buf).context("invalid JSON in LSP frame")?,
    ))
}

/// file:// URI for a local path, minimally percent-encoded. Windows drive
/// paths become `file:///C:/…`; the `\\?\` verbatim prefix is stripped.
fn path_to_uri(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    let s = s.trim_start_matches("//?/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/');
    }
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Key form for matching a server's published URI against ours — servers
/// disagree on drive-letter case and whether `:` is percent-encoded.
fn norm_uri(uri: &str) -> String {
    uri.to_lowercase().replace("%3a", ":")
}

/// Render raw LSP diagnostics as capped `path:line:col severity: message`
/// lines (1-based; LSP positions are 0-based). Errors and warnings only;
/// None = nothing worth showing.
fn format_diagnostics(path: &str, diags: &[Value]) -> Option<String> {
    let mut lines: Vec<(i64, String)> = vec![];
    for d in diags {
        // Missing severity means "up to the client" — treat as error.
        let sev = d.get("severity").and_then(|s| s.as_i64()).unwrap_or(1);
        if sev > 2 {
            continue;
        }
        let word = if sev == 1 { "error" } else { "warning" };
        let start = d.get("range").and_then(|r| r.get("start"));
        let get = |k| {
            start
                .and_then(|s| s.get(k))
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
        };
        let msg = d
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        if msg.is_empty() {
            continue;
        }
        lines.push((
            sev,
            format!(
                "{path}:{}:{} {word}: {msg}",
                get("line") + 1,
                get("character") + 1
            ),
        ));
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort_by_key(|(sev, _)| *sev); // stable: errors survive the cap first
    let extra = lines.len().saturating_sub(MAX_LINES);
    let mut out: Vec<String> = lines.into_iter().take(MAX_LINES).map(|(_, l)| l).collect();
    if extra > 0 {
        out.push(format!("(+{extra} more)"));
    }
    Some(out.join("\n"))
}

// ---- server -----------------------------------------------------------------

struct LspServer {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Latest published diagnostics keyed by normalized URI. An entry's
    /// PRESENCE means the server reported since we cleared it — an empty
    /// vec is "all clear".
    diags: Arc<std::sync::Mutex<HashMap<String, Vec<Value>>>>,
    /// uri → document version (present = didOpen already sent).
    open_docs: Mutex<HashMap<String, i64>>,
    alive: Arc<AtomicBool>,
    _child: Child,
}

impl LspServer {
    /// Spawn and handshake. Errors are for the manager to record — callers
    /// of diagnostics never see them.
    async fn spawn(command: &[String], root: &Path) -> Result<Arc<Self>> {
        let program = command
            .first()
            .ok_or_else(|| anyhow!("empty LSP command"))?;
        let mut child = Command::new(program)
            .args(&command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning LSP server '{program}'"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let server = Arc::new(Self {
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Arc::default(),
            diags: Arc::default(),
            open_docs: Mutex::new(HashMap::new()),
            alive: Arc::new(AtomicBool::new(true)),
            _child: child,
        });

        // Reader: route responses to waiters, record publishDiagnostics,
        // answer server-initiated requests just enough to not wedge it.
        let pending = server.pending.clone();
        let diags = server.diags.clone();
        let alive = server.alive.clone();
        let weak = Arc::downgrade(&server);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_frame(&mut reader).await {
                let id = msg.get("id").cloned();
                let method = msg.get("method").and_then(|m| m.as_str());
                match (id, method) {
                    (None, Some("textDocument/publishDiagnostics")) => {
                        let uri = msg
                            .pointer("/params/uri")
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        let list = msg
                            .pointer("/params/diagnostics")
                            .and_then(|d| d.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if let Ok(mut d) = diags.lock() {
                            d.insert(norm_uri(uri), list);
                        }
                    }
                    (Some(id), Some(method)) => {
                        // Benign replies keep servers happy; anything that
                        // truly needed support degrades on their side.
                        let result = match method {
                            "workspace/configuration" => {
                                let n = msg
                                    .pointer("/params/items")
                                    .and_then(|i| i.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                Value::Array(vec![Value::Null; n])
                            }
                            _ => Value::Null,
                        };
                        if let Some(server) = weak.upgrade() {
                            let _ = server
                                .send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
                                .await;
                        }
                    }
                    (Some(id), None) => {
                        if let Some(id) = id.as_i64() {
                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                    }
                    _ => {} // other notifications — ignore
                }
            }
            alive.store(false, Ordering::SeqCst);
            // Drop pending waiters so a dead-on-arrival server (e.g. a
            // rustup shim without the component) fails the handshake
            // immediately instead of riding out the timeout.
            pending.lock().await.clear();
        });

        let root_uri = path_to_uri(root);
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        server
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{"uri": root_uri, "name": name}],
                    "capabilities": {
                        "textDocument": {
                            "synchronization": {"didSave": false},
                            "publishDiagnostics": {"relatedInformation": false}
                        }
                    },
                    "clientInfo": {"name": "rift", "version": env!("CARGO_PKG_VERSION")}
                }),
                INIT_TIMEOUT,
            )
            .await
            .with_context(|| format!("LSP initialize failed for '{program}'"))?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&encode_frame(msg)).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let outcome = async {
            self.send(&msg).await?;
            tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| anyhow!("LSP server timed out on {method}"))?
                .map_err(|_| anyhow!("LSP server closed during {method}"))
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
                "LSP {method} error: {}",
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
            );
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Sync the document (didOpen first time, full-text didChange after),
    /// then wait briefly for the server's diagnostics for that URI.
    async fn diagnostics(&self, path: &Path, text: &str, root: &Path) -> Option<String> {
        if !self.alive() {
            return None;
        }
        let uri = path_to_uri(path);
        let key = norm_uri(&uri);
        if let Ok(mut d) = self.diags.lock() {
            d.remove(&key); // presence below = published for THIS version
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let sent = {
            let mut open = self.open_docs.lock().await;
            match open.get_mut(&uri) {
                Some(version) => {
                    *version += 1;
                    self.notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {"uri": uri, "version": *version},
                            "contentChanges": [{"text": text}]
                        }),
                    )
                    .await
                }
                None => {
                    open.insert(uri.clone(), 1);
                    self.notify(
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": language_id(&ext),
                                "version": 1,
                                "text": text
                            }
                        }),
                    )
                    .await
                }
            }
        };
        sent.ok()?;
        let deadline = tokio::time::Instant::now() + DIAG_WAIT;
        loop {
            let published = self.diags.lock().ok().and_then(|d| d.get(&key).cloned());
            if let Some(list) = published {
                return format_diagnostics(&display_path(path, root), &list);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(DIAG_POLL).await;
        }
    }
}

/// The path as shown in diagnostic lines — relative to the workspace root
/// when possible (same economy as the rest of the tool output).
fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

// ---- manager ----------------------------------------------------------------

enum Slot {
    Running(Arc<LspServer>),
    /// Spawn was tried and failed, or the server died — don't retry on
    /// every edit for the rest of the session.
    Unavailable,
}

/// Lazily-spawned language servers for one workspace root. Installed on the
/// root ToolCtx and shared into sub-agent ctxs.
pub struct LspManager {
    root: PathBuf,
    overrides: HashMap<String, LspServerOverride>,
    servers: Mutex<HashMap<String, Slot>>,
}

impl LspManager {
    /// Build from the config `lsp` field. None = disabled (`"lsp": false`).
    pub fn from_config(root: &Path, setting: Option<LspSetting>) -> Option<Arc<Self>> {
        let overrides = match setting {
            Some(LspSetting::Enabled(false)) => return None,
            Some(LspSetting::Servers(map)) => map,
            _ => HashMap::new(),
        };
        Some(Arc::new(Self {
            root: root.to_path_buf(),
            overrides,
            servers: Mutex::new(HashMap::new()),
        }))
    }

    /// The argv to spawn for a language, binary resolved on PATH; None =
    /// disabled or nothing installed.
    fn resolve_command(&self, lang: &str) -> Option<Vec<String>> {
        let with_path = |argv: &[String]| {
            find_binary(argv.first()?).map(|p| {
                let mut c = vec![p.display().to_string()];
                c.extend(argv[1..].iter().cloned());
                c
            })
        };
        if let Some(o) = self.overrides.get(lang) {
            if o.disabled {
                return None;
            }
            if !o.command.is_empty() {
                return with_path(&o.command);
            }
        }
        builtin_for_lang(lang)?
            .commands
            .iter()
            .find_map(|argv| with_path(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
    }

    /// Diagnostics for `path` holding `text`, as capped display lines.
    /// Every failure path degrades to None — diagnostics are a bonus,
    /// never an error.
    pub async fn diagnostics(&self, path: &Path, text: &str) -> Option<String> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        let lang = lang_for_ext(&ext, &self.overrides)?;
        let server = {
            let mut servers = self.servers.lock().await;
            match servers.get(&lang) {
                Some(Slot::Running(s)) if s.alive() => s.clone(),
                Some(Slot::Running(_)) => {
                    servers.insert(lang, Slot::Unavailable);
                    return None;
                }
                Some(Slot::Unavailable) => return None,
                None => {
                    let Some(command) = self.resolve_command(&lang) else {
                        servers.insert(lang, Slot::Unavailable);
                        return None;
                    };
                    match LspServer::spawn(&command, &self.root).await {
                        Ok(s) => {
                            servers.insert(lang.clone(), Slot::Running(s.clone()));
                            s
                        }
                        Err(_) => {
                            servers.insert(lang, Slot::Unavailable);
                            return None;
                        }
                    }
                }
            }
        };
        server.diagnostics(path, text, &self.root).await
    }

    /// (language, command, state) rows for /lsp: running / failed /
    /// available / not found / disabled.
    pub async fn status(&self) -> Vec<(String, String, &'static str)> {
        let servers = self.servers.lock().await;
        let mut langs: Vec<String> = BUILTINS.iter().map(|b| b.lang.to_string()).collect();
        for k in self.overrides.keys() {
            if !langs.contains(k) {
                langs.push(k.clone());
            }
        }
        langs
            .into_iter()
            .map(|lang| {
                let label = self
                    .overrides
                    .get(&lang)
                    .filter(|o| !o.command.is_empty())
                    .map(|o| o.command.join(" "))
                    .or_else(|| {
                        builtin_for_lang(&lang).map(|b| {
                            b.commands
                                .iter()
                                .map(|c| c[0])
                                .collect::<Vec<_>>()
                                .join(" | ")
                        })
                    })
                    .unwrap_or_default();
                let state = if self.overrides.get(&lang).is_some_and(|o| o.disabled) {
                    "disabled"
                } else {
                    match servers.get(&lang) {
                        Some(Slot::Running(s)) if s.alive() => "running",
                        Some(_) => "failed",
                        None if self.resolve_command(&lang).is_some() => "available",
                        None => "not found",
                    }
                };
                (lang, label, state)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip_and_eof() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let b = json!({"jsonrpc": "2.0", "method": "initialized", "params": {"x": "π"}});
        let mut stream = encode_frame(&a);
        stream.extend(encode_frame(&b));
        let mut r = BufReader::new(&stream[..]);
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(a));
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(b));
        assert_eq!(read_frame(&mut r).await.unwrap(), None); // clean EOF
    }

    #[tokio::test]
    async fn frame_reader_is_lenient_about_headers() {
        // Lowercase header name, an extra header, both tolerated.
        let body = r#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let raw = format!(
            "content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{body}",
            body.len()
        );
        let mut r = BufReader::new(raw.as_bytes());
        let msg = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(msg["id"], 7);
        // Garbage without a length is an error, not a hang or a panic.
        let mut r = BufReader::new(&b"no headers here\r\n\r\n"[..]);
        assert!(read_frame(&mut r).await.is_err());
    }

    #[test]
    fn registry_maps_extensions_to_languages() {
        let none = HashMap::new();
        assert_eq!(lang_for_ext("rs", &none).as_deref(), Some("rust"));
        assert_eq!(lang_for_ext("tsx", &none).as_deref(), Some("typescript"));
        assert_eq!(lang_for_ext("hpp", &none).as_deref(), Some("c"));
        assert_eq!(lang_for_ext("md", &none), None);
        // Config-added languages are keyed by extension.
        let mut over = HashMap::new();
        over.insert(
            "zig".to_string(),
            LspServerOverride {
                command: vec!["zls".into()],
                disabled: false,
            },
        );
        assert_eq!(lang_for_ext("zig", &over).as_deref(), Some("zig"));
    }

    #[test]
    fn binary_probe_misses_garbage() {
        assert!(find_binary("definitely-not-a-real-binary-a426").is_none());
    }

    #[test]
    fn uris_normalize_across_server_quirks() {
        let uri = path_to_uri(Path::new(r"C:\work\my proj\a.rs"));
        assert_eq!(uri, "file:///C:/work/my%20proj/a.rs");
        // rust-analyzer style: lowercase drive, percent-encoded colon.
        assert_eq!(norm_uri("file:///c%3A/work/my%20proj/a.rs"), norm_uri(&uri));
        assert_eq!(
            path_to_uri(Path::new("/home/x/a.rs")),
            "file:///home/x/a.rs"
        );
    }

    #[test]
    fn diagnostics_format_converts_filters_and_caps() {
        let diag = |sev: i64, line: i64, msg: &str| {
            json!({
                "severity": sev,
                "range": {"start": {"line": line, "character": 4}, "end": {"line": line, "character": 9}},
                "message": msg
            })
        };
        // 0-based → 1-based, severity words, hints/info dropped, first
        // message line only.
        let list = vec![
            diag(2, 0, "unused variable"),
            diag(1, 9, "mismatched types\nexpected `u32`"),
            diag(3, 2, "info: not shown"),
            diag(4, 3, "hint: not shown"),
        ];
        let out = format_diagnostics("src/a.rs", &list).unwrap();
        assert_eq!(
            out,
            "src/a.rs:10:5 error: mismatched types\nsrc/a.rs:1:5 warning: unused variable"
        );
        // Cap: 12 errors + 3 warnings → 10 error lines + (+5 more).
        let mut many: Vec<Value> = (0..3).map(|i| diag(2, i, "w")).collect();
        many.extend((0..12).map(|i| diag(1, i, "e")));
        let out = format_diagnostics("a.py", &many).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[..10].iter().all(|l| l.contains(" error: ")));
        assert_eq!(lines[10], "(+5 more)");
        // Nothing reportable → None (empty list = all clear).
        assert!(format_diagnostics("a.rs", &[]).is_none());
        assert!(format_diagnostics("a.rs", &[diag(3, 0, "just info")]).is_none());
    }

    #[test]
    fn manager_respects_config() {
        // `false` disables the whole subsystem.
        assert!(
            LspManager::from_config(Path::new("."), Some(LspSetting::Enabled(false))).is_none()
        );
        // A disabled language resolves to no command even if the binary
        // exists; unknown languages resolve to nothing.
        let mut map = HashMap::new();
        map.insert(
            "rust".to_string(),
            LspServerOverride {
                command: vec![],
                disabled: true,
            },
        );
        let mgr = LspManager::from_config(Path::new("."), Some(LspSetting::Servers(map))).unwrap();
        assert!(mgr.resolve_command("rust").is_none());
        assert!(mgr.resolve_command("cobol").is_none());
    }

    #[test]
    fn lsp_setting_parses_both_shapes() {
        let s: LspSetting = serde_json::from_str("false").unwrap();
        assert!(matches!(s, LspSetting::Enabled(false)));
        let s: LspSetting =
            serde_json::from_str(r#"{"rust": {"disabled": true}, "zig": {"command": ["zls"]}}"#)
                .unwrap();
        let LspSetting::Servers(map) = s else {
            panic!("expected map")
        };
        assert!(map["rust"].disabled);
        assert_eq!(map["zig"].command, vec!["zls"]);
    }

    /// A scripted stdio LSP server: answers initialize and publishes one
    /// error diagnostic whenever the synced text contains "boom".
    const MOCK_SERVER_PY: &str = r#"
import sys, json
def read_msg():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: return None
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
    if length is None: return None
    return json.loads(sys.stdin.buffer.read(length))
def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()
while True:
    m = read_msg()
    if m is None: break
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        p = m["params"]; td = p["textDocument"]
        text = td.get("text")
        if text is None:
            text = (p.get("contentChanges") or [{}])[0].get("text", "")
        diags = []
        if "boom" in text:
            diags = [{"severity": 1,
                      "range": {"start": {"line": 1, "character": 2},
                                "end": {"line": 1, "character": 6}},
                      "message": "boom found"}]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": td["uri"], "diagnostics": diags}})
"#;

    // End-to-end against a scripted server: spawn, handshake, didOpen with
    // a diagnostic, didChange back to clean. Skips when no working python
    // is on PATH (the Windows Store alias stub doesn't count).
    #[tokio::test]
    async fn mock_server_end_to_end() {
        let python = ["python3", "python"].iter().find(|p| {
            std::process::Command::new(p)
                .arg("-c")
                .arg("print(1)")
                .output()
                .is_ok_and(|o| o.status.success())
        });
        let Some(python) = python else { return };
        let dir = std::env::temp_dir().join(format!("rift-lsp-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(&script, MOCK_SERVER_PY).unwrap();
        let mut map = HashMap::new();
        map.insert(
            "mock".to_string(),
            LspServerOverride {
                command: vec![python.to_string(), script.display().to_string()],
                disabled: false,
            },
        );
        let mgr = LspManager::from_config(&dir, Some(LspSetting::Servers(map))).unwrap();
        let file = dir.join("a.mock");
        let out = mgr.diagnostics(&file, "ok\nx boom x\n").await.expect("diagnostic expected");
        assert_eq!(out, "a.mock:2:3 error: boom found");
        // Clean text → publish with an empty list → nothing to report.
        assert!(mgr.diagnostics(&file, "all fine\n").await.is_none());
        assert!(mgr.status().await.iter().any(|(l, _, s)| l == "mock" && *s == "running"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Live handshake against a real rust-analyzer — opt in with
    // RIFT_LSP_LIVE=1 (needs rust-analyzer on PATH).
    #[tokio::test]
    async fn live_rust_analyzer_handshake() {
        if std::env::var("RIFT_LSP_LIVE").is_err() {
            return;
        }
        // A rustup shim without the component "exists" on PATH but dies on
        // startup — only a binary that answers --version is a real server.
        let works = std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !works {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rift-lsp-live-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mgr = LspManager::from_config(&dir, None).unwrap();
        // The handshake must complete and the wait must degrade quietly
        // (a cold server rarely publishes within the window).
        let _ = mgr
            .diagnostics(
                &dir.join("src/main.rs"),
                "fn main() { let x: u32 = \"\"; }\n",
            )
            .await;
        let status = mgr.status().await;
        assert!(status
            .iter()
            .any(|(l, _, s)| l == "rust" && *s == "running"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
