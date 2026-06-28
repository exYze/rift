//! Built-in tool set. Limits mirror what battle-tested agents use (opencode):
//! reads capped at 2000 lines / 50KB, grep at 100 matches, bash output at 30KB
//! — all to protect the model's context window, which is the scarcest resource
//! on local models.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rift_ollama::ToolDef;
use serde_json::{json, Map, Value};

const READ_MAX_LINES: usize = 2000;
const READ_MAX_LINE_CHARS: usize = 2000;
const READ_MAX_BYTES: usize = 50_000;
const GREP_MAX_MATCHES: usize = 100;
const GLOB_MAX_RESULTS: usize = 200;
const BASH_MAX_OUTPUT: usize = 30_000;
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 60;
const BASH_MAX_TIMEOUT_SECS: u64 = 300;
/// Default cap for commands that look like dev servers/watchers — they never
/// exit, so waiting the full default timeout just wastes the model's time.
const SERVER_PROBE_SECS: u64 = 15;

/// Shell patterns refused regardless of configuration.
const BASH_DENY_BUILTIN: &[&str] = &[
    "sudo", "sudo *", "rm -rf /", "rm -rf /*", "rm -fr /", "rm -fr /*",
    "shutdown*", "reboot*", "halt*", "poweroff*", "mkfs*", "dd if=*of=/dev/*",
    ":(){*", "chmod -R 777 /*", "chown -R * /",
];

/// The built-in shell deny list (for display, e.g. `/permissions`).
pub fn builtin_bash_deny() -> &'static [&'static str] {
    BASH_DENY_BUILTIN
}

/// One file mutation made by the write/edit tools, with enough state to
/// restore the file to how it was before (`prior` = None means the file
/// didn't exist).
#[derive(Debug, Clone)]
pub struct EditRecord {
    pub path: PathBuf,
    pub prior: Option<String>,
    pub turn: u64,
}

/// A clarifying question from the model (the `ask_user` tool), answered by
/// whatever frontend is attached. `choices` empty = free-text answer.
pub struct AskRequest {
    pub question: String,
    pub choices: Vec<String>,
    pub reply: tokio::sync::oneshot::Sender<String>,
}

/// One step of the model's self-declared task checklist (the `plan` tool).
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub text: String,
    pub done: bool,
}

#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    deny: std::sync::Arc<std::sync::Mutex<(globset::GlobSet, Vec<String>)>>,
    journal: std::sync::Arc<std::sync::Mutex<Vec<EditRecord>>>,
    turn: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ask: Option<tokio::sync::mpsc::UnboundedSender<AskRequest>>,
    plan: std::sync::Arc<std::sync::Mutex<Vec<PlanItem>>>,
    /// Approval mode: mutating tools (write/edit/bash) pause for a y/n.
    /// Atomic so /approve and /config reload can flip it mid-session.
    approve: std::sync::Arc<std::sync::atomic::AtomicBool>,
    approved_kinds: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

fn build_deny_set(extra: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for pat in BASH_DENY_BUILTIN.iter().copied().map(str::to_string).chain(extra.iter().cloned()) {
        if let Ok(glob) = globset::GlobBuilder::new(&pat).literal_separator(false).build() {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| globset::GlobSet::empty())
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_extra_deny(cwd, &[])
    }

    /// `extra_deny`: additional glob patterns from user config.
    pub fn with_extra_deny(cwd: impl Into<PathBuf>, extra_deny: &[String]) -> Self {
        Self {
            cwd: cwd.into(),
            deny: std::sync::Arc::new(std::sync::Mutex::new((build_deny_set(extra_deny), extra_deny.to_vec()))),
            journal: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ask: None,
            plan: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            approve: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            approved_kinds: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Require user approval before write/edit/bash execute. Only effective
    /// when an interactive frontend is attached via `with_interaction`.
    pub fn with_approval(self, approve: bool) -> Self {
        self.set_approval(approve);
        self
    }

    /// Flip approval mode at runtime (/approve, /config reload).
    pub fn set_approval(&self, on: bool) {
        self.approve.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn approval_enabled(&self) -> bool {
        self.approve.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Replace the user deny patterns at runtime (/config reload). The
    /// built-in list is always merged back in.
    pub fn set_deny(&self, extra: &[String]) {
        if let Ok(mut d) = self.deny.lock() {
            *d = (build_deny_set(extra), extra.to_vec());
        }
    }

    /// Gate a mutating action behind user approval. Returns Err when denied —
    /// the model sees that as a tool error and adjusts course.
    async fn check_approval(&self, kind: &str, summary: &str) -> Result<()> {
        if !self.approval_enabled() {
            return Ok(());
        }
        if self.approved_kinds.lock().map(|k| k.contains(kind)).unwrap_or(false) {
            return Ok(());
        }
        // Approval requires an interactive user; without one the mode is moot.
        let Some(ask) = &self.ask else { return Ok(()) };
        let always = format!("always allow {kind} this session");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = ask.send(AskRequest {
            question: format!("Allow {kind}: {summary}"),
            choices: vec!["allow".into(), always.clone(), "deny".into()],
            reply: reply_tx,
        });
        match reply_rx.await.as_deref() {
            Ok("allow") => Ok(()),
            Ok(a) if a == always => {
                if let Ok(mut kinds) = self.approved_kinds.lock() {
                    kinds.insert(kind.to_string());
                }
                Ok(())
            }
            _ => bail!("the user DENIED this {kind} action. Do not retry it; ask them how to proceed or choose another approach."),
        }
    }

    pub fn plan_snapshot(&self) -> Vec<PlanItem> {
        self.plan.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn clear_plan(&self) {
        if let Ok(mut p) = self.plan.lock() {
            p.clear();
        }
    }

    /// Attach an interactive frontend that can answer `ask_user` questions.
    /// Without one (headless, swarm candidates) the tool reports itself
    /// unavailable so the model proceeds on its own judgment.
    pub fn with_interaction(mut self, ask: tokio::sync::mpsc::UnboundedSender<AskRequest>) -> Self {
        self.ask = Some(ask);
        self
    }

    /// User-configured deny patterns (without the built-ins).
    pub fn user_deny_patterns(&self) -> Vec<String> {
        self.deny.lock().map(|d| d.1.clone()).unwrap_or_default()
    }

    fn bash_denied(&self, command: &str) -> bool {
        let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
        self.deny.lock().map(|d| d.0.is_match(&normalized)).unwrap_or(false)
    }

    /// Mark the start of a new agent turn; edits recorded after this group
    /// under the new turn for `undo_last_turn`.
    pub fn begin_turn(&self) {
        self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn record_edit(&self, path: PathBuf, prior: Option<String>) {
        let turn = self.turn.load(std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut j) = self.journal.lock() {
            // One snapshot per (turn, path) is enough — the FIRST prior state
            // of the turn is what undo must restore.
            if !j.iter().any(|r| r.turn == turn && r.path == path) {
                j.push(EditRecord { path, prior, turn });
            }
        }
    }

    /// Revert every file the write/edit tools touched in the most recent turn
    /// that made changes. Returns the restored paths (empty = nothing to undo).
    pub fn undo_last_turn(&self) -> Result<Vec<PathBuf>> {
        let records: Vec<EditRecord> = {
            let mut j = self.journal.lock().map_err(|_| anyhow!("edit journal poisoned"))?;
            let Some(last_turn) = j.iter().map(|r| r.turn).max() else {
                return Ok(vec![]);
            };
            let taken = j.iter().filter(|r| r.turn == last_turn).cloned().collect();
            j.retain(|r| r.turn != last_turn);
            taken
        };
        let mut restored = Vec::with_capacity(records.len());
        for rec in records {
            match &rec.prior {
                Some(content) => std::fs::write(&rec.path, content)
                    .with_context(|| format!("restoring {}", rec.path.display()))?,
                None => {
                    let _ = std::fs::remove_file(&rec.path);
                }
            }
            restored.push(rec.path);
        }
        Ok(restored)
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn standard() -> Self {
        Self {
            tools: vec![
                Box::new(ReadTool),
                Box::new(WriteTool),
                Box::new(EditTool),
                Box::new(BashTool),
                Box::new(LsTool),
                Box::new(GrepTool),
                Box::new(GlobTool),
                Box::new(OutlineTool),
                Box::new(RepoMapTool),
                Box::new(PlanTool),
            ],
        }
    }

    /// Add a tool (e.g. from an MCP server) to the registry.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef::function(t.name(), t.description(), t.parameters()))
            .collect()
    }

    /// Local models frequently hallucinate tool names from their finetuning
    /// data (`read_file` instead of `read`, etc). Map the common variants to
    /// our canonical names instead of failing the call.
    pub fn resolve_alias<'a>(&self, name: &'a str) -> &'a str {
        match name {
            "read_file" | "readfile" | "open_file" | "view_file" | "cat" | "view" => "read",
            "write_file" | "create_file" | "save_file" | "write_to_file" => "write",
            "edit_file" | "str_replace" | "replace_in_file" | "apply_edit" => "edit",
            "run_command" | "execute" | "execute_command" | "shell" | "run_shell_command"
            | "terminal" | "run_bash" | "exec" => "bash",
            "list_files" | "list_directory" | "list_dir" | "dir" => "ls",
            "search" | "search_files" | "grep_search" | "code_search" | "search_code" => "grep",
            "find_files" | "file_glob" | "file_search" => "glob",
            "skeleton" | "file_outline" | "symbols" | "get_outline" => "outline",
            "repo_overview" | "project_map" | "codebase_map" => "repo_map",
            other => other,
        }
    }
}

fn req_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required string parameter '{key}'"))
}

fn opt_u64(args: &Map<String, Value>, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Remove ANSI escape sequences (CSI + OSC) and stray control characters
/// from command output, keeping newlines and tabs.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.peek() == Some(&'\\')) {
                            if n == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Largest index <= max that lands on a char boundary.
fn floor_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index >= min that lands on a char boundary.
fn ceil_boundary(s: &str, min: usize) -> usize {
    let mut i = min.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = &s[..floor_boundary(s, max / 2)];
    let tail = &s[ceil_boundary(s, s.len() - max / 2)..];
    format!("{head}\n... [output truncated: {} of {} bytes shown] ...\n{tail}", max, s.len())
}

/// When a path doesn't exist, find real files with the same (or similar)
/// name so the model can correct itself in one step instead of exploring.
fn suggest_similar_paths(cwd: &Path, missing: &Path) -> Vec<String> {
    let Some(want) = missing.file_name().and_then(|n| n.to_str()) else {
        return vec![];
    };
    let want_lower = want.to_lowercase();
    let stem_lower = missing
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| want_lower.clone());
    let mut exact = vec![];
    let mut close = vec![];
    for entry in ignore::WalkBuilder::new(cwd).build().take(5000).flatten() {
        let Some(name) = entry.file_name().to_str() else { continue };
        let name_lower = name.to_lowercase();
        let shown = entry.path().strip_prefix(cwd).unwrap_or(entry.path()).display().to_string();
        if name_lower == want_lower {
            exact.push(shown);
        } else if name_lower.contains(&stem_lower) && entry.file_type().is_some_and(|t| t.is_file()) {
            close.push(shown);
        }
        if exact.len() >= 5 {
            break;
        }
    }
    exact.extend(close);
    exact.truncate(5);
    exact
}

fn enoent_hint(cwd: &Path, path: &Path) -> String {
    let found = suggest_similar_paths(cwd, path);
    if found.is_empty() {
        format!("{} does not exist. Use ls or glob to find the right path.", path.display())
    } else {
        format!("{} does not exist. Did you mean: {}?", path.display(), found.join(", "))
    }
}

/// Character-bigram Dice similarity — cheap, no deps, good enough to point
/// the model at the line it was probably trying to edit.
fn bigram_similarity(a: &str, b: &str) -> f64 {
    fn bigrams(s: &str) -> Vec<(char, char)> {
        let chars: Vec<char> = s.chars().collect();
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    }
    let (mut a_grams, b_grams) = (bigrams(a), bigrams(b));
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    let total = a_grams.len() + b_grams.len();
    let mut hits = 0usize;
    for g in &b_grams {
        if let Some(pos) = a_grams.iter().position(|x| x == g) {
            a_grams.swap_remove(pos);
            hits += 1;
        }
    }
    (2.0 * hits as f64) / total as f64
}

/// Best-matching line in `text` for the first substantial line of `target`.
fn closest_line(text: &str, target: &str) -> Option<(usize, String)> {
    let probe = target.lines().map(str::trim).find(|l| l.len() >= 8)?;
    let mut best: Option<(f64, usize, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let score = bigram_similarity(line.trim(), probe);
        if score > best.map_or(0.5, |(s, _, _)| s) {
            best = Some((score, i + 1, line));
        }
    }
    best.map(|(_, no, line)| {
        let line = line.trim();
        let cut = floor_boundary(line, 120);
        (no, line[..cut].to_string())
    })
}

// ---------------------------------------------------------------- read

struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a text file. Returns line-numbered content. Use offset/limit for large files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "File path (relative to the working directory or absolute)"},
                "offset": {"type": "integer", "description": "1-based line number to start from (default 1)"},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let path = ctx.resolve(req_str(args, "path")?);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{}", enoent_hint(&ctx.cwd, &path)),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        if looks_binary(&bytes) {
            bail!("{} appears to be a binary file", path.display());
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = opt_u64(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = opt_u64(args, "limit").unwrap_or(READ_MAX_LINES as u64) as usize;
        let limit = limit.min(READ_MAX_LINES);

        let total_lines = text.lines().count();
        let mut out = String::new();
        let mut shown = 0usize;
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let line = if line.len() > READ_MAX_LINE_CHARS {
                &line[..floor_boundary(line, READ_MAX_LINE_CHARS)]
            } else {
                line
            };
            out.push_str(&format!("{:>5}\t{}\n", i + 1, line));
            shown += 1;
            if out.len() > READ_MAX_BYTES {
                out.push_str(&format!("[truncated at {} bytes; use offset/limit to read more]\n", READ_MAX_BYTES));
                break;
            }
        }
        if shown == 0 {
            bail!("offset {} is past the end of the file ({} lines)", offset, total_lines);
        }
        if offset - 1 + shown < total_lines {
            out.push_str(&format!(
                "[showing lines {}-{} of {}; continue with offset={}]\n",
                offset,
                offset - 1 + shown,
                total_lines,
                offset + shown
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- write

struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates parent directories."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "File path to write"},
                "content": {"type": "string", "description": "Full file content"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let path = ctx.resolve(req_str(args, "path")?);
        let content = req_str(args, "content")?;
        ctx.check_approval("write", &format!("{} ({} bytes)", path.display(), content.len())).await?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        ctx.record_edit(path.clone(), tokio::fs::read_to_string(&path).await.ok());
        tokio::fs::write(&path, content).await.with_context(|| format!("cannot write {}", path.display()))?;
        Ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
    }
}

// ---------------------------------------------------------------- edit

struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Replace an exact string in a file. old_string must match exactly (including whitespace) and be unique unless replace_all is true."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "old_string": {"type": "string", "description": "Exact text to find"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let path = ctx.resolve(req_str(args, "path")?);
        let old = req_str(args, "old_string")?;
        let new = req_str(args, "new_string")?;
        if old.is_empty() {
            bail!("old_string must not be empty");
        }
        if old == new {
            bail!("old_string and new_string are identical");
        }
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{}", enoent_hint(&ctx.cwd, &path)),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let count = text.matches(old).count();
        if count == 0 {
            // Diagnose WHY it didn't match so the model can fix the call in
            // one step instead of re-reading and guessing.
            let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            if norm(&text).contains(&norm(old)) {
                bail!(
                    "old_string not found in {}, but the same text DOES exist with different whitespace/indentation. \
                     Re-read those lines and copy them exactly (tabs vs spaces, line breaks).",
                    path.display()
                );
            }
            if let Some((line_no, line)) = closest_line(&text, old) {
                bail!(
                    "old_string not found in {}. Closest match is line {line_no}: `{line}`. \
                     Read around that line and use its exact text.",
                    path.display()
                );
            }
            bail!("old_string not found in {}. Read the file again and match the text exactly.", path.display());
        }
        if count > 1 && !replace_all {
            bail!("old_string occurs {count} times in {}; include more surrounding context to make it unique, or set replace_all", path.display());
        }
        ctx.check_approval("edit", &path.display().to_string()).await?;
        let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        ctx.record_edit(path.clone(), Some(text.clone()));
        tokio::fs::write(&path, &updated).await?;
        Ok(format!("Edited {} ({} replacement{})", path.display(), count, if count == 1 { "" } else { "s" }))
    }
}

// ---------------------------------------------------------------- bash

/// Build a shell invocation for the host platform. On Windows commands run
/// through `cmd.exe /C` (honoring %COMSPEC%); everywhere else through `sh -c`.
/// This is what lets the bash tool behave the same on macOS, Linux and Windows
/// instead of failing to spawn `sh` on Windows.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the working directory and return its output and exit code. \
         Long-running servers/watchers (npm run dev, vite, …) are auto-killed after a short probe \
         and their startup output returned — prefer build/test commands for verification."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "Shell command to run"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 60, max 300)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let command = req_str(args, "command")?;
        if ctx.bash_denied(command) {
            bail!("command blocked by permission policy: {command}");
        }
        let preview: String = command.chars().take(120).collect();
        ctx.check_approval("bash", &preview).await?;
        let explicit_timeout = opt_u64(args, "timeout_secs");
        let server_like = looks_like_server_command(command);
        let mut timeout = explicit_timeout.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS).min(BASH_MAX_TIMEOUT_SECS);
        if server_like && explicit_timeout.is_none() {
            timeout = timeout.min(SERVER_PROBE_SECS);
        }

        let mut cmd = shell_command(command);
        cmd.current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout can kill the whole tree
        // (shell → npm → node → vite), not just the shell. On Windows the
        // equivalent tree-kill is `taskkill /T` in the timeout branch below.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().context("spawning shell")?;
        let pid = child.id();

        // Read pipes concurrently so output survives a timeout kill.
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                use tokio::io::AsyncReadExt;
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(p) = stderr_pipe.as_mut() {
                use tokio::io::AsyncReadExt;
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        });

        let status = match tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await {
            Ok(s) => Some(s?),
            Err(_) => {
                // Kill the whole process tree, not just the shell wrapper: on
                // Unix via the negative-pid process group, on Windows via
                // `taskkill /T` which walks and terminates child processes.
                if let Some(pid) = pid {
                    #[cfg(unix)]
                    {
                        let _ = tokio::process::Command::new("sh")
                            .arg("-c")
                            .arg(format!("kill -9 -{pid} 2>/dev/null"))
                            .output()
                            .await;
                    }
                    #[cfg(windows)]
                    {
                        let pid_str = pid.to_string();
                        let _ = tokio::process::Command::new("taskkill")
                            .args(["/F", "/T", "/PID", pid_str.as_str()])
                            .output()
                            .await;
                    }
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
                None
            }
        };
        let stdout_bytes = out_task.await.unwrap_or_default();
        let stderr_bytes = err_task.await.unwrap_or_default();

        let mut text = String::new();
        // Strip ANSI color/control sequences (npm, cargo, git…): they corrupt
        // the TUI if rendered and waste the model's tokens either way.
        let stdout = strip_ansi(&String::from_utf8_lossy(&stdout_bytes));
        let stderr = strip_ansi(&String::from_utf8_lossy(&stderr_bytes));
        if !stdout.trim().is_empty() {
            text.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push_str("\n--- stderr ---\n");
            }
            text.push_str(stderr.trim_end());
        }
        if text.is_empty() {
            text = "(no output)".into();
        }
        let mut text = truncate_middle(&text, BASH_MAX_OUTPUT);
        match status {
            Some(s) if !s.success() => {
                text.push_str(&format!("\n[exit code: {}]", s.code().unwrap_or(-1)));
            }
            None if server_like => {
                text.push_str(&format!(
                    "\n[process killed after {timeout}s — this looks like a dev server/watcher that never exits. \
                     If the output above shows it started (e.g. 'ready', 'listening', a local URL), treat that as \
                     SUCCESS and do NOT run it again. Verify code changes with a build or test command instead.]"
                ));
            }
            None => {
                text.push_str(&format!(
                    "\n[process killed after {timeout}s timeout — the output above is everything it printed. \
                     If this is a server/watcher that runs forever, don't re-run it; verify with a build or test \
                     command. If it's genuinely slow, retry with a larger timeout_secs (max {BASH_MAX_TIMEOUT_SECS}).]"
                ));
            }
            Some(_) => {}
        }
        Ok(text)
    }
}

// ---------------------------------------------------------------- plan

/// The model's visible task checklist. Steps render pinned in the activity
/// pane, so the user always sees what the agent intends and where it is.
struct PlanTool;

fn render_plan(items: &[PlanItem]) -> String {
    if items.is_empty() {
        return "(plan is empty)".into();
    }
    let mut out = String::from("Current plan:\n");
    for (i, item) in items.iter().enumerate() {
        out.push_str(&format!("{} {}. {}\n", if item.done { "[x]" } else { "[ ]" }, i + 1, item.text));
    }
    out
}

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> &str {
        "Maintain your visible task checklist. Pass `set` (array of short step descriptions) to create or \
         replace the plan before starting multi-step work; pass `done` (1-based step number) to check a step \
         off as you complete it. Returns the current checklist."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "set": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Replace the plan with these steps"
                },
                "done": {"type": "integer", "description": "Mark this step (1-based) as completed"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let set = args.get("set").and_then(|v| v.as_array());
        let done = opt_u64(args, "done");
        if set.is_none() && done.is_none() {
            bail!("pass `set` (array of steps) and/or `done` (step number)");
        }
        let mut plan = ctx.plan.lock().map_err(|_| anyhow!("plan state poisoned"))?;
        if let Some(steps) = set {
            *plan = steps
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| PlanItem { text: s.to_string(), done: false })
                .collect();
        }
        if let Some(n) = done {
            let idx = (n as usize).saturating_sub(1);
            match plan.get_mut(idx) {
                Some(item) => item.done = true,
                None => bail!("step {n} does not exist; the plan has {} steps", plan.len()),
            }
        }
        Ok(render_plan(&plan))
    }
}

// ---------------------------------------------------------------- ask_user

/// Elicitation: lets the model pause and ask the user a clarifying question.
/// Registered only when an interactive frontend is attached.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the user a clarifying question when the task is ambiguous or a decision needs their input. \
         Provide 2-5 choices when the answer is a selection; omit choices for a free-text answer. \
         Blocks until the user responds. Use sparingly — only when guessing would risk doing the wrong work."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["question"],
            "properties": {
                "question": {"type": "string", "description": "The question to ask the user"},
                "choices": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of answers the user picks from"
                }
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let question = req_str(args, "question")?.to_string();
        let choices: Vec<String> = args
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let Some(ask) = &ctx.ask else {
            bail!("ask_user is not available in this session (no interactive user); decide using your best judgment");
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        ask.send(AskRequest { question, choices, reply: reply_tx })
            .map_err(|_| anyhow!("the interactive session is gone"))?;
        match reply_rx.await {
            Ok(answer) => Ok(format!("User answered: {answer}")),
            Err(_) => Ok("[the user dismissed the question without answering; proceed on your best judgment]".into()),
        }
    }
}

/// Commands that start a long-running server or watcher — they never exit,
/// so the default timeout only delays the inevitable.
fn looks_like_server_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    if c.contains("build") {
        return false;
    }
    const PHRASES: &[&str] = &[
        "npm run dev", "npm start", "npm run start", "npm run serve", "yarn dev", "yarn start",
        "pnpm dev", "pnpm run dev", "pnpm start", "bun dev", "bun run dev", "next dev", "nuxt dev",
        "astro dev", "webpack serve", "webpack-dev-server", "flask run", "rails server", "rails s ",
        "python -m http.server", "python3 -m http.server", "tail -f", "--watch", "http-server",
        "live-server", "nodemon", "uvicorn", "gunicorn", "cargo run --bin server", "cargo watch",
    ];
    if PHRASES.iter().any(|p| c.contains(p)) {
        return true;
    }
    // Bare binaries that are servers (token match avoids "vitest" etc.)
    c.split_whitespace().any(|w| {
        let w = w.rsplit('/').next().unwrap_or(w);
        matches!(w, "vite" | "serve")
    })
}

// ---------------------------------------------------------------- ls

struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List the entries of a directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path (default: working directory)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let path = ctx.resolve(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        let mut rd = tokio::fs::read_dir(&path).await.with_context(|| format!("cannot list {}", path.display()))?;
        let mut entries = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push(if is_dir { format!("{name}/") } else { name });
        }
        entries.sort();
        if entries.is_empty() {
            return Ok("(empty directory)".into());
        }
        Ok(entries.join("\n"))
    }
}

// ---------------------------------------------------------------- grep

struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex. Respects .gitignore. Returns path:line: text matches."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "Directory or file to search (default: working directory)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let pattern = req_str(args, "pattern")?.to_string();
        let root = ctx.resolve(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        let cwd = ctx.cwd.clone();
        // File walking + regex matching is blocking work.
        tokio::task::spawn_blocking(move || {
            let re = regex::Regex::new(&pattern).map_err(|e| anyhow!("invalid regex: {e}"))?;
            let mut results = Vec::new();
            let walker = ignore::WalkBuilder::new(&root).hidden(true).build();
            'outer: for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(entry.path()) else { continue };
                if looks_binary(&bytes) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let display = entry.path().strip_prefix(&cwd).unwrap_or(entry.path());
                        let line = if line.len() > 250 { &line[..floor_boundary(line, 250)] } else { line };
                        results.push(format!("{}:{}: {}", display.display(), i + 1, line.trim_end()));
                        if results.len() >= GREP_MAX_MATCHES {
                            results.push(format!("[stopped at {GREP_MAX_MATCHES} matches; narrow the pattern]"));
                            break 'outer;
                        }
                    }
                }
            }
            if results.is_empty() {
                return Ok("no matches".to_string());
            }
            Ok(results.join("\n"))
        })
        .await?
    }
}

// ---------------------------------------------------------------- outline

struct OutlineTool;

#[async_trait]
impl Tool for OutlineTool {
    fn name(&self) -> &str {
        "outline"
    }
    fn description(&self) -> &str {
        "Get a signatures-only skeleton of a source file (functions, classes, types with line numbers, bodies hidden). 10-20x cheaper than read — use this first, then read only the line ranges you need."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "Source file (.rs .py .js .jsx .ts .tsx .go)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let path = ctx.resolve(req_str(args, "path")?);
        let source = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        tokio::task::spawn_blocking(move || crate::outline::outline_source(&path, &source)).await?
    }
}

// ---------------------------------------------------------------- repo_map

struct RepoMapTool;

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &str {
        "repo_map"
    }
    fn description(&self) -> &str {
        "Overview of the codebase: outlines of the most recently modified source files. Use this to orient before searching or reading."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Root directory (default: working directory)"},
                "max_files": {"type": "integer", "description": "Max files to include (default 20)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let root = ctx.resolve(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        let max_files = opt_u64(args, "max_files").unwrap_or(20).min(60) as usize;
        let cwd = ctx.cwd.clone();
        tokio::task::spawn_blocking(move || {
            // Most recently modified supported source files first.
            let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
            let walker = ignore::WalkBuilder::new(&root).hidden(true).build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let path = entry.path();
                if !crate::outline::supports(path) {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                files.push((mtime, path.to_path_buf()));
            }
            if files.is_empty() {
                return Ok("no supported source files found (.rs .py .js .jsx .ts .tsx .go)".to_string());
            }
            files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
            let total_found = files.len();
            let mut out = String::new();
            let mut included = 0;
            for (_, path) in files.into_iter().take(max_files) {
                let Ok(source) = std::fs::read_to_string(&path) else { continue };
                let Ok(outline) = crate::outline::outline_source(&path, &source) else { continue };
                let display = path.strip_prefix(&cwd).unwrap_or(&path);
                out.push_str(&format!("=== {} ===\n{outline}\n", display.display()));
                included += 1;
                if out.len() > 24_000 {
                    out.push_str("[repo map truncated at 24KB]\n");
                    break;
                }
            }
            if total_found > included {
                out.push_str(&format!(
                    "[{included} of {total_found} source files shown, newest first; use outline for specific files]\n"
                ));
            }
            Ok(out)
        })
        .await?
    }
}

// ---------------------------------------------------------------- glob

struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern like **/*.rs. Respects .gitignore."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. **/*.rs or src/*.toml"},
                "path": {"type": "string", "description": "Root directory to search (default: working directory)"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let pattern = req_str(args, "pattern")?.to_string();
        let root = ctx.resolve(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        tokio::task::spawn_blocking(move || {
            let glob = globset::GlobBuilder::new(&pattern)
                .literal_separator(false)
                .build()
                .map_err(|e| anyhow!("invalid glob: {e}"))?
                .compile_matcher();
            let mut results = Vec::new();
            let walker = ignore::WalkBuilder::new(&root).hidden(true).build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                if glob.is_match(rel) {
                    results.push(rel.display().to_string());
                    if results.len() >= GLOB_MAX_RESULTS {
                        results.push(format!("[stopped at {GLOB_MAX_RESULTS} results]"));
                        break;
                    }
                }
            }
            if results.is_empty() {
                return Ok("no files matched".to_string());
            }
            results.sort();
            Ok(results.join("\n"))
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_deny_builtin_and_extra() {
        let ctx = ToolCtx::with_extra_deny("/tmp", &["curl *".to_string()]);
        assert!(ctx.bash_denied("sudo whoami"));
        assert!(ctx.bash_denied("sudo"));
        assert!(ctx.bash_denied("  sudo   rm  x")); // whitespace-normalized
        assert!(ctx.bash_denied("curl https://example.com"));
        assert!(!ctx.bash_denied("echo sudo is a word"));
        assert!(!ctx.bash_denied("ls -la"));
        assert!(!ctx.bash_denied("rm -rf ./build")); // only / and /* are blocked
    }

    #[test]
    fn server_command_detection() {
        assert!(looks_like_server_command("npm run dev"));
        assert!(looks_like_server_command("cd app && npm run dev"));
        assert!(looks_like_server_command("npx vite"));
        assert!(looks_like_server_command("python3 -m http.server 8080"));
        assert!(looks_like_server_command("cargo watch -x test"));
        assert!(looks_like_server_command("tsc --watch"));
        assert!(!looks_like_server_command("npm run build"));
        assert!(!looks_like_server_command("vite build"));
        assert!(!looks_like_server_command("npx vitest run"));
        assert!(!looks_like_server_command("cargo test"));
        assert!(!looks_like_server_command("ls -la"));
    }

    #[tokio::test]
    async fn bash_timeout_returns_partial_output() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo server-started; sleep 30".into()));
        args.insert("timeout_secs".into(), Value::from(1));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("server-started"), "partial output must survive the kill: {out}");
        assert!(out.contains("killed after 1s"), "timeout note missing: {out}");
    }

    #[tokio::test]
    async fn bash_server_probe_caps_default_timeout() {
        // No explicit timeout + server-like command → capped probe, output kept.
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert(
            "command".into(),
            Value::String("echo ready; tail -f /dev/null".into()),
        );
        let start = std::time::Instant::now();
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(start.elapsed().as_secs() < SERVER_PROBE_SECS + 10);
        assert!(out.contains("ready"));
        assert!(out.contains("dev server/watcher"), "server guidance missing: {out}");
    }

    #[test]
    fn closest_line_points_at_near_match() {
        let text = "fn alpha() {}\nconst products = [ { id: 1, name: 'Classic' } ];\nfn omega() {}";
        let (no, line) = closest_line(text, "const products = [ { id: 1, name: \"Classic\" } ]").unwrap();
        assert_eq!(no, 2);
        assert!(line.contains("products"));
        assert!(closest_line(text, "zzz qqq totally unrelated www").is_none());
    }

    #[test]
    fn enoent_suggests_similar_paths() {
        let dir = std::env::temp_dir().join(format!("rift-enoent-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/App.jsx"), "x").unwrap();
        let hint = enoent_hint(&dir, &dir.join("App.jsx"));
        assert!(hint.contains("src/App.jsx"), "should suggest the real path: {hint}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strip_ansi_removes_colors_and_osc() {
        assert_eq!(strip_ansi("\x1b[32madded\x1b[0m 13 packages"), "added 13 packages");
        assert_eq!(strip_ansi("\x1b]0;title\x07plain"), "plain");
        assert_eq!(strip_ansi("keep\nnewlines\tand tabs"), "keep\nnewlines\tand tabs");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn undo_restores_prior_content_and_deletes_new_files() {
        let dir = std::env::temp_dir().join(format!("rift-undo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("existing.txt");
        let created = dir.join("created.txt");
        std::fs::write(&existing, "original").unwrap();

        let ctx = ToolCtx::new(&dir);
        ctx.begin_turn();
        ctx.record_edit(existing.clone(), Some("original".into()));
        std::fs::write(&existing, "modified").unwrap();
        ctx.record_edit(created.clone(), None);
        std::fs::write(&created, "new file").unwrap();
        // A second snapshot of the same path in the same turn must not
        // overwrite the first prior state.
        ctx.record_edit(existing.clone(), Some("modified".into()));
        std::fs::write(&existing, "modified twice").unwrap();

        let restored = ctx.undo_last_turn().unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists());
        // Journal drained: a second undo is a no-op.
        assert!(ctx.undo_last_turn().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
