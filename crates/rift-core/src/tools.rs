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

#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    bash_deny: std::sync::Arc<globset::GlobSet>,
    user_deny: std::sync::Arc<Vec<String>>,
    journal: std::sync::Arc<std::sync::Mutex<Vec<EditRecord>>>,
    turn: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_extra_deny(cwd, &[])
    }

    /// `extra_deny`: additional glob patterns from user config.
    pub fn with_extra_deny(cwd: impl Into<PathBuf>, extra_deny: &[String]) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        for pat in BASH_DENY_BUILTIN.iter().copied().map(str::to_string).chain(extra_deny.iter().cloned()) {
            if let Ok(glob) = globset::GlobBuilder::new(&pat).literal_separator(false).build() {
                builder.add(glob);
            }
        }
        let set = builder.build().unwrap_or_else(|_| globset::GlobSet::empty());
        Self {
            cwd: cwd.into(),
            bash_deny: std::sync::Arc::new(set),
            user_deny: std::sync::Arc::new(extra_deny.to_vec()),
            journal: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// User-configured deny patterns (without the built-ins).
    pub fn user_deny_patterns(&self) -> &[String] {
        &self.user_deny
    }

    fn bash_denied(&self, command: &str) -> bool {
        let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
        self.bash_deny.is_match(&normalized)
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
        let bytes = tokio::fs::read(&path).await.with_context(|| format!("cannot read {}", path.display()))?;
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
        let text = tokio::fs::read_to_string(&path).await.with_context(|| format!("cannot read {}", path.display()))?;
        let count = text.matches(old).count();
        if count == 0 {
            bail!("old_string not found in {}. Read the file again and match the text exactly.", path.display());
        }
        if count > 1 && !replace_all {
            bail!("old_string occurs {count} times in {}; include more surrounding context to make it unique, or set replace_all", path.display());
        }
        let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        ctx.record_edit(path.clone(), Some(text.clone()));
        tokio::fs::write(&path, &updated).await?;
        Ok(format!("Edited {} ({} replacement{})", path.display(), count, if count == 1 { "" } else { "s" }))
    }
}

// ---------------------------------------------------------------- bash

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the working directory and return its output and exit code."
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
        let timeout = opt_u64(args, "timeout_secs")
            .unwrap_or(BASH_DEFAULT_TIMEOUT_SECS)
            .min(BASH_MAX_TIMEOUT_SECS);
        let fut = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(Duration::from_secs(timeout), fut)
            .await
            .map_err(|_| anyhow!("command timed out after {timeout}s"))??;

        let mut text = String::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
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
        if !output.status.success() {
            text.push_str(&format!("\n[exit code: {}]", output.status.code().unwrap_or(-1)));
        }
        Ok(text)
    }
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
