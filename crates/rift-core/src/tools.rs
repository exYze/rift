//! Built-in tool set. Limits mirror what battle-tested agents use (opencode):
//! reads capped at 2000 lines / 50KB, grep at 100 matches, bash output at 30KB
//! — all to protect the model's context window, which is the scarcest resource
//! on local models.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rift_provider::ToolDef;
use serde_json::{json, Map, Value};

const READ_MAX_LINES: usize = 2000;
const READ_MAX_LINE_CHARS: usize = 2000;
const READ_MAX_BYTES: usize = 50_000;
/// Hydrate-on-demand: an unbounded `read` of a source file longer than this
/// returns its line-numbered outline instead of a huge head — the model then
/// fetches exactly the ranges it needs. Explicit offset/limit always bypasses.
const READ_HYDRATE_LINES: usize = 500;
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
/// didn't exist). Prior content is kept as raw bytes so overwriting a
/// non-UTF-8 file (binary, UTF-16 — common on Windows) stays undoable.
#[derive(Debug, Clone)]
pub struct EditRecord {
    pub path: PathBuf,
    pub prior: Option<Vec<u8>>,
    pub turn: u64,
}

/// A clarifying question from the model (the `ask_user` tool), answered by
/// whatever frontend is attached. `choices` empty = free-text answer.
pub struct AskRequest {
    pub question: String,
    /// Supporting lines shown above the question — approval prompts put the
    /// pending diff here, rendered with diff coloring. Empty = none.
    pub detail: Vec<String>,
    pub choices: Vec<String>,
    pub reply: tokio::sync::oneshot::Sender<String>,
}

/// A proposed file mutation offered to a frontend for interactive diff
/// review (the VS Code inline diff with per-hunk accept/reject). Installed
/// via [`ToolCtx::set_edit_review`]; when present it replaces the plain
/// approval prompt for write/edit.
pub struct EditReviewRequest {
    /// Which tool proposed it: "edit" or "write".
    pub tool: String,
    pub path: PathBuf,
    /// Current file content; empty for a new file.
    pub old: String,
    /// Proposed content.
    pub new: String,
    pub reply: tokio::sync::oneshot::Sender<EditReviewReply>,
}

pub enum EditReviewReply {
    /// Write this content — the full proposal, or the reviewer's
    /// accepted-hunk subset of it.
    Apply(String),
    Deny,
}

/// A compact ±diff between two texts for approval previews: common prefix
/// and suffix lines are trimmed, the changed middle shows as -old/+new.
/// Whole-file precision isn't the goal — seeing what you're approving is.
pub fn preview_diff(old: &str, new: &str, max_lines: usize) -> Vec<String> {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let mut start = 0;
    while start < o.len() && start < n.len() && o[start] == n[start] {
        start += 1;
    }
    let (mut oe, mut ne) = (o.len(), n.len());
    while oe > start && ne > start && o[oe - 1] == n[ne - 1] {
        oe -= 1;
        ne -= 1;
    }
    if start == oe && start == ne {
        return vec![]; // identical
    }
    let mut out = Vec::new();
    if start > 0 || oe < o.len() {
        out.push(format!("@@ line {} @@", start + 1));
    }
    for l in &o[start..oe] {
        out.push(format!("-{l}"));
    }
    for l in &n[start..ne] {
        out.push(format!("+{l}"));
    }
    if out.len() > max_lines {
        let hidden = out.len() - max_lines;
        out.truncate(max_lines);
        out.push(format!("… [{hidden} more diff lines]"));
    }
    out
}

/// One alternating run of a line diff: shared lines, or an old→new change.
/// The reviewable unit for interactive diff review — each `Change` is one
/// hunk a frontend can accept or reject independently.
#[derive(Debug, Clone, PartialEq)]
pub enum DiffSegment {
    Same(Vec<String>),
    Change { old: Vec<String>, new: Vec<String> },
}

/// Line-based Myers diff of `old` → `new` as alternating segments. This is
/// the authoritative hunking for edit review (the serve protocol ships it
/// with every edit_review event) — frontends render and reassemble from it
/// instead of re-deriving their own diff. Degrades to a single whole-file
/// `Change` when the edit distance exceeds the cap: review still works,
/// just as one hunk.
pub fn diff_segments(old: &str, new: &str) -> Vec<DiffSegment> {
    let a: Vec<&str> = old.split('\n').collect();
    let b: Vec<&str> = new.split('\n').collect();
    let mut segs: Vec<DiffSegment> = Vec::new();
    let mut push = |op: u8, line: &str| {
        match (op, segs.last_mut()) {
            (b'=', Some(DiffSegment::Same(lines))) => lines.push(line.into()),
            (b'=', _) => segs.push(DiffSegment::Same(vec![line.into()])),
            (b'-', Some(DiffSegment::Change { old, .. })) => old.push(line.into()),
            (b'+', Some(DiffSegment::Change { new, .. })) => new.push(line.into()),
            (b'-', _) => segs.push(DiffSegment::Change { old: vec![line.into()], new: vec![] }),
            (b'+', _) => segs.push(DiffSegment::Change { old: vec![], new: vec![line.into()] }),
            _ => unreachable!(),
        }
    };
    for (op, line) in myers_ops(&a, &b) {
        push(op, line);
    }
    segs
}

/// Myers O(ND) as a flat op list: (b'=' shared | b'-' deleted | b'+'
/// inserted, the line). Capped at edit distance 2000 — past that, one
/// whole-file delete+insert (same fallback the reference JS reviewer used).
fn myers_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(u8, &'a str)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return b.iter().map(|l| (b'+', *l)).collect();
    }
    if m == 0 {
        return a.iter().map(|l| (b'-', *l)).collect();
    }
    let max_d = (n + m).min(2000);
    let offset = max_d as isize;
    let mut v = vec![0usize; 2 * max_d + 1];
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let mut found: isize = -1;
    'outer: for d in 0..=max_d as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let i = (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found = d;
                break 'outer;
            }
            k += 2;
        }
    }
    if found < 0 {
        let mut ops: Vec<(u8, &str)> = a.iter().map(|l| (b'-', *l)).collect();
        ops.extend(b.iter().map(|l| (b'+', *l)));
        return ops;
    }
    let mut ops: Vec<(u8, &str)> = Vec::with_capacity(n + m);
    let (mut x, mut y) = (n, m);
    for d in (1..=found).rev() {
        let vp = &trace[d as usize];
        let k = x as isize - y as isize;
        let i = (offset + k) as usize;
        let prev_k = if k == -d || (k != d && vp[i - 1] < vp[i + 1]) { k + 1 } else { k - 1 };
        let prev_x = vp[(offset + prev_k) as usize];
        let prev_y = (prev_x as isize - prev_k) as usize;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push((b'=', a[x]));
        }
        if x == prev_x {
            y -= 1;
            ops.push((b'+', b[y]));
        } else {
            x -= 1;
            ops.push((b'-', a[x]));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push((b'=', a[x]));
    }
    while x > 0 {
        x -= 1;
        ops.push((b'-', a[x]));
    }
    while y > 0 {
        y -= 1;
        ops.push((b'+', b[y]));
    }
    ops.reverse();
    ops
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
    /// Granular permission rules (allow/ask/deny `Tool(pattern)` entries,
    /// with the built-in bash deny list and the legacy bash_allow/bash_deny
    /// globs folded in). Deny always wins; see crate::permissions.
    rules: std::sync::Arc<std::sync::Mutex<crate::permissions::RuleSet>>,
    journal: std::sync::Arc<std::sync::Mutex<Vec<EditRecord>>>,
    turn: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ask: Option<tokio::sync::mpsc::UnboundedSender<AskRequest>>,
    plan: std::sync::Arc<std::sync::Mutex<Vec<PlanItem>>>,
    /// Approval mode: mutating tools (write/edit/bash) pause for a y/n.
    /// Atomic so /approve and /config reload can flip it mid-session.
    approve: std::sync::Arc<std::sync::atomic::AtomicBool>,
    approved_kinds: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Session-wide background task registry (bash run_in_background and
    /// background sub-agents). Shared into sub-agent ctxs so all tasks land
    /// in one table the frontend and the task tool both see.
    bg: crate::tasks::BgTasks,
    /// Provider handle for the `agent` tool. Installed by the frontend on the
    /// ROOT ctx only; sub-agent ctxs get None, which is what stops recursion.
    subagent: std::sync::Arc<std::sync::RwLock<Option<crate::subagent::SubAgentHandle>>>,
    /// post_edit hook commands (config `hooks`, project ones trust-gated);
    /// run after every successful write/edit, failures fed to the model.
    hooks: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Sandbox wrapper template ({cmd}/{cwd} placeholders) every bash
    /// invocation routes through; None = run directly.
    bash_wrapper: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Interactive diff-review channel (the VS Code inline diff): when
    /// installed, write/edit proposals route here instead of the plain
    /// approval prompt. Shared into sub-agent ctxs like `ask`.
    edit_review:
        std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<EditReviewRequest>>>>,
    /// SearXNG endpoint for the web_search tool; None = search unavailable.
    search_url: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared LSP manager: per-language diagnostics servers queried after
    /// write/edit. None = disabled or never installed — the edit result is
    /// then byte-identical to a no-LSP session.
    lsp: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<crate::lsp::LspManager>>>>,
    /// ± preview of the change the LAST successful write/edit applied. The
    /// agent loop takes it right after the call and surfaces it as an
    /// EditDiff event, so frontends can show what actually changed.
    last_diff: LastDiffSlot,
}

/// (edited file, capped ± preview lines) of the most recent write/edit.
type LastDiffSlot = std::sync::Arc<std::sync::Mutex<Option<(PathBuf, Vec<String>)>>>;

/// The "always allow" pattern offered for a command: program + subcommand
/// when the second token looks like one (`git push …` → `git push *`),
/// otherwise just the program (`python x.py` → `python *`). Deliberately
/// prefix-shaped — narrow enough to mean something, broad enough to stop
/// re-prompting on every flag variation.
fn allow_pattern_for(command: &str) -> String {
    let mut it = command.split_whitespace();
    match (it.next(), it.next()) {
        (Some(a), Some(b)) if !b.starts_with('-') && !b.contains(['/', '\\', '.', '=']) => {
            format!("{a} {b} *")
        }
        (Some(a), _) => format!("{a} *"),
        _ => "*".into(),
    }
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_extra_deny(cwd, &[])
    }

    /// `extra_deny`: additional bash glob patterns from user config.
    pub fn with_extra_deny(cwd: impl Into<PathBuf>, extra_deny: &[String]) -> Self {
        let perms = crate::config::Permissions { bash_deny: extra_deny.to_vec(), ..Default::default() };
        let rules = crate::permissions::RuleSet::compile(&perms, BASH_DENY_BUILTIN, &mut vec![]);
        Self {
            cwd: cwd.into(),
            rules: std::sync::Arc::new(std::sync::Mutex::new(rules)),
            journal: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ask: None,
            plan: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            approve: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            approved_kinds: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            bg: crate::tasks::BgTasks::default(),
            subagent: std::sync::Arc::new(std::sync::RwLock::new(None)),
            hooks: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            bash_wrapper: std::sync::Arc::new(std::sync::Mutex::new(None)),
            edit_review: std::sync::Arc::new(std::sync::Mutex::new(None)),
            search_url: std::sync::Arc::new(std::sync::Mutex::new(None)),
            lsp: std::sync::Arc::new(std::sync::Mutex::new(None)),
            last_diff: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Record the ± preview of a just-applied write/edit (see `last_diff`).
    fn record_last_diff(&self, path: &Path, diff: Vec<String>) {
        if let Ok(mut d) = self.last_diff.lock() {
            *d = Some((path.to_path_buf(), diff));
        }
    }

    /// Take the pending change preview, leaving None.
    pub fn take_last_diff(&self) -> Option<(PathBuf, Vec<String>)> {
        self.last_diff.lock().ok().and_then(|mut d| d.take())
    }

    /// Set/clear the SearXNG endpoint (startup, /search, /config reload).
    pub fn set_search_url(&self, url: Option<String>) {
        if let Ok(mut u) = self.search_url.lock() {
            *u = url.map(|u| u.trim_end_matches('/').to_string()).filter(|u| !u.is_empty());
        }
    }

    pub fn search_url(&self) -> Option<String> {
        self.search_url.lock().ok().and_then(|u| u.clone())
    }

    /// Install the LSP manager (startup; None disables).
    pub fn set_lsp(&self, mgr: Option<std::sync::Arc<crate::lsp::LspManager>>) {
        if let Ok(mut l) = self.lsp.lock() {
            *l = mgr;
        }
    }

    pub fn lsp(&self) -> Option<std::sync::Arc<crate::lsp::LspManager>> {
        self.lsp.lock().ok().and_then(|l| l.clone())
    }

    /// LSP diagnostics for a just-edited file, formatted for the tool
    /// result. None whenever anything is off/missing/slow — an LSP failure
    /// must never affect the edit itself.
    pub(crate) async fn lsp_diagnostics(&self, path: &Path, text: &str) -> Option<String> {
        self.lsp()?.diagnostics(path, text).await
    }

    /// Set/clear the sandbox wrapper (startup, /config reload).
    pub fn set_bash_wrapper(&self, wrapper: Option<String>) {
        if let Ok(mut w) = self.bash_wrapper.lock() {
            *w = wrapper.filter(|t| t.contains("{cmd}"));
        }
    }

    pub fn bash_wrapper(&self) -> Option<String> {
        self.bash_wrapper.lock().ok().and_then(|w| w.clone())
    }

    /// The command bash actually executes: the raw command (what the deny
    /// list and approval saw) routed through the sandbox wrapper when one
    /// is configured.
    pub(crate) fn effective_command(&self, command: &str) -> String {
        match self.bash_wrapper() {
            Some(tpl) => wrap_sandbox(&tpl, command, &self.cwd),
            None => command.to_string(),
        }
    }

    /// Replace the post_edit hook commands (startup, /config reload).
    pub fn set_post_edit_hooks(&self, hooks: &[String]) {
        if let Ok(mut h) = self.hooks.lock() {
            *h = hooks.to_vec();
        }
    }

    pub fn post_edit_hooks(&self) -> Vec<String> {
        self.hooks.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// The session's background task registry.
    pub fn bg(&self) -> &crate::tasks::BgTasks {
        &self.bg
    }

    /// Install the sub-agent handle (frontends call this once on the root
    /// ctx; it's what makes the `agent` tool usable).
    pub fn set_subagent(&self, handle: crate::subagent::SubAgentHandle) {
        if let Ok(mut h) = self.subagent.write() {
            *h = Some(handle);
        }
    }

    /// Update the live client/cfg ONLY where a handle is installed —
    /// run_turn calls this so /model & /host switches propagate without
    /// enabling delegation in ctxs that never had it (sub-agents, swarm
    /// candidates). The routing parts (factory, roles) are startup-fixed.
    pub fn refresh_subagent(
        &self,
        client: std::sync::Arc<dyn rift_provider::Provider>,
        cfg: crate::agent::AgentConfig,
    ) {
        if let Ok(mut h) = self.subagent.write() {
            if let Some(h) = h.as_mut() {
                h.client = client;
                h.cfg = cfg;
            }
        }
    }

    pub fn subagent_handle(&self) -> Option<crate::subagent::SubAgentHandle> {
        self.subagent.read().ok()?.clone()
    }

    /// The ctx a sub-agent works in: same cwd, permission policy (deny list,
    /// approval mode + one shared "always allow" set), ask channel, and task
    /// registry — but its own plan, undo journal, and NO sub-agent handle.
    pub fn subagent_ctx(&self) -> ToolCtx {
        ToolCtx {
            cwd: self.cwd.clone(),
            rules: self.rules.clone(),
            journal: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ask: self.ask.clone(),
            plan: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
            approve: self.approve.clone(),
            approved_kinds: self.approved_kinds.clone(),
            bg: self.bg.clone(),
            subagent: std::sync::Arc::new(std::sync::RwLock::new(None)),
            // Sub-agents edit files too — the same verification applies.
            hooks: self.hooks.clone(),
            bash_wrapper: self.bash_wrapper.clone(),
            // Sub-agent edits go through the same interactive diff review.
            edit_review: self.edit_review.clone(),
            // Research sub-agents search too.
            search_url: self.search_url.clone(),
            // Sub-agent edits get the same diagnostics (same workspace root).
            lsp: self.lsp.clone(),
            // Own slot: a child's edits surface through its own loop.
            last_diff: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Run the post_edit hooks after a successful write/edit. Returns a
    /// report to append to the tool result when any hook fails — that puts
    /// broken builds/tests in front of the model in the same tool result,
    /// so it fixes them before moving on. Successes only log.
    pub(crate) async fn run_post_edit_hooks(&self, edited: &Path) -> Option<String> {
        const HOOK_TIMEOUT_SECS: u64 = 120;
        const HOOK_OUTPUT_CAP: usize = 4000;
        let hooks = self.post_edit_hooks();
        if hooks.is_empty() {
            return None;
        }
        let mut failures = String::new();
        for hook in hooks {
            let mut cmd = shell_command(&hook);
            cmd.current_dir(&self.cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let outcome = async {
                let out = cmd.output().await?;
                anyhow::Ok(out)
            };
            let result = tokio::time::timeout(Duration::from_secs(HOOK_TIMEOUT_SECS), outcome).await;
            match result {
                Ok(Ok(out)) if out.status.success() => {
                    self.bg.emit(crate::agent::AgentEvent::Info(format!("hook ✓ {hook}")));
                }
                Ok(Ok(out)) => {
                    self.bg.emit(crate::agent::AgentEvent::Info(format!("hook ✗ {hook}")));
                    let mut text = String::new();
                    text.push_str(&strip_ansi(&String::from_utf8_lossy(&out.stdout)));
                    let err = strip_ansi(&String::from_utf8_lossy(&out.stderr));
                    if !err.trim().is_empty() {
                        if !text.trim().is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&err);
                    }
                    failures.push_str(&format!(
                        "\n\n[post-edit hook FAILED (exit {}): {hook}]\n{}",
                        out.status.code().unwrap_or(-1),
                        truncate_middle(text.trim(), HOOK_OUTPUT_CAP)
                    ));
                }
                Ok(Err(e)) => {
                    failures.push_str(&format!("\n\n[post-edit hook FAILED to run: {hook}]\n{e:#}"));
                }
                Err(_) => {
                    failures.push_str(&format!(
                        "\n\n[post-edit hook TIMED OUT after {HOOK_TIMEOUT_SECS}s: {hook}]"
                    ));
                }
            }
        }
        (!failures.is_empty()).then(|| {
            format!(
                "{failures}\n\nThe edit to {} is applied, but the checks above now fail — fix them before moving on.",
                edited.display()
            )
        })
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

    /// Rebuild the whole rule set from config (startup, /config reload).
    /// Returned warnings name any malformed rules that were skipped.
    pub fn set_permissions(&self, perms: &crate::config::Permissions) -> Vec<String> {
        let mut warnings = vec![];
        let rules = crate::permissions::RuleSet::compile(perms, BASH_DENY_BUILTIN, &mut warnings);
        if let Ok(mut r) = self.rules.lock() {
            *r = rules;
        }
        warnings
    }

    /// Add one bash allow pattern at runtime (the "always allow" choice).
    pub fn add_allow_pattern(&self, pattern: &str) {
        self.add_allow_rule(&format!("Bash({pattern})"));
    }

    /// Add one `Tool(pattern)` allow rule at runtime.
    pub fn add_allow_rule(&self, rule: &str) {
        if let Ok(mut r) = self.rules.lock() {
            r.add_allow(rule);
        }
    }

    /// The active user rules — (allow, ask, deny), built-ins excluded — for
    /// /permissions.
    pub fn permission_rules(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        self.rules.lock().map(|r| r.user_rules()).unwrap_or_default()
    }

    /// The active allow rules (for /yolo's summary line).
    pub fn user_allow_patterns(&self) -> Vec<String> {
        self.permission_rules().0
    }

    /// What the rules say about a path-taking tool call; None = no rule.
    fn path_decision(
        &self,
        tool: &str,
        path: &Path,
    ) -> Option<(crate::permissions::Decision, String)> {
        let candidates = crate::permissions::path_candidates(path, &self.cwd);
        self.rules.lock().ok()?.decide(tool, &candidates)
    }

    /// A copy of the current rules, for blocking walkers (grep/glob) that
    /// filter files off the async runtime.
    pub(crate) fn rules_snapshot(&self) -> crate::permissions::RuleSet {
        self.rules.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Deny-rule enforcement for read-side tools (read/ls/grep/glob/outline)
    /// — `Read(~/.ssh/**)` blocks before the resource is touched. Allow/ask
    /// rules don't apply to reads (they never prompt).
    pub(crate) fn check_read_allowed(&self, tool: &str, path: &Path) -> Result<()> {
        if let Some((crate::permissions::Decision::Deny, rule)) = self.path_decision(tool, path) {
            bail!("{} blocked by permission rule '{rule}'", path.display());
        }
        Ok(())
    }

    /// Deny-rule gate for listing a directory: denied when the directory
    /// itself OR its arbitrary children are covered — `Read(secrets/**)`
    /// must refuse `ls secrets`, not just reads inside it.
    pub(crate) fn check_list_allowed(&self, tool: &str, dir: &Path) -> Result<()> {
        self.check_read_allowed(tool, dir)?;
        // A probe child no real rule names specifically: any `dir/**`-shaped
        // deny matches it, a single-file deny doesn't.
        if let Some((crate::permissions::Decision::Deny, rule)) =
            self.path_decision(tool, &dir.join("\u{1}"))
        {
            bail!("{} blocked by permission rule '{rule}'", dir.display());
        }
        Ok(())
    }

    /// Deny-rule enforcement for URL tools (fetch/web_fetch). URLs match
    /// as written and normalized (scheme+host lowercased, default port
    /// stripped) — `HTTPS://X.INTERNAL:443/a` can't evade `*://*.internal/*`.
    pub(crate) fn check_url_allowed(&self, tool: &str, url: &str) -> Result<()> {
        let candidates = crate::permissions::url_candidates(url);
        let decision = self.rules.lock().ok().and_then(|r| r.decide(tool, &candidates));
        if let Some((crate::permissions::Decision::Deny, rule)) = decision {
            bail!("{url} blocked by permission rule '{rule}'");
        }
        Ok(())
    }

    /// The chained segments of a shell command, whitespace-normalized —
    /// permission checks run per segment so `git status && curl evil` is
    /// judged as two commands. Deny, ask and allow all read the SAME
    /// normalization: expansions the shell turns into whitespace at run time
    /// (`git${IFS}push`) fold to spaces first, and `$` splits a segment, so
    /// an ask/deny rule can't be evaded the way a literal match could.
    fn bash_segments(command: &str) -> Vec<String> {
        let expanded =
            command.replace("${IFS}", " ").replace("$IFS", " ").replace("$@", " ").replace("$*", " ");
        expanded
            .split(['&', '|', ';', '\n', '(', ')', '`', '$'])
            .map(|seg| seg.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|seg| !seg.is_empty())
            .collect()
    }

    /// Is every chained segment of `command` pre-approved? Requires ALL
    /// segments to match — `git status && curl evil` must still prompt when
    /// only `git status` is allowed. (Deny is checked separately and wins.)
    fn bash_allowed(&self, command: &str) -> bool {
        let Ok(rules) = self.rules.lock() else { return false };
        if !rules.has_bash_allow() {
            return false;
        }
        let segments = Self::bash_segments(command);
        !segments.is_empty() && segments.iter().all(|seg| rules.bash_allow_match(seg))
    }

    /// The first ask rule any chained segment of `command` matches — a hit
    /// forces a prompt even when approval mode is off.
    fn bash_ask_rule(&self, command: &str) -> Option<String> {
        let rules = self.rules.lock().ok()?;
        Self::bash_segments(command).iter().find_map(|seg| rules.bash_ask_match(seg))
    }

    /// Approval gate for bash, with Claude Code-style allow tracking: an
    /// allow-listed command runs silently; otherwise the prompt offers a
    /// persistent "always allow '<pattern>'" (saved to the user config)
    /// alongside once/session/deny. An ask rule (`Bash(git push *)` in
    /// permissions.ask) forces the prompt even in /yolo mode.
    pub(crate) async fn check_bash_approval(&self, command: &str) -> Result<()> {
        let forced = self.bash_ask_rule(command);
        if !self.approval_enabled() && forced.is_none() {
            return Ok(());
        }
        if forced.is_none() {
            if self.approved_kinds.lock().map(|k| k.contains("bash")).unwrap_or(false) {
                return Ok(());
            }
            if self.bash_allowed(command) {
                return Ok(());
            }
        }
        // Approval requires an interactive user; without one the mode is
        // moot — except an ask rule, which explicitly demands a human.
        let Some(ask) = &self.ask else {
            return match forced {
                Some(rule) => bail!(
                    "command requires interactive approval (permission rule '{rule}') and no user is attached"
                ),
                None => Ok(()),
            };
        };
        let pattern = allow_pattern_for(command);
        let preview: String = command.chars().take(120).collect();
        let always = format!("always allow '{pattern}'");
        let session = "allow all bash this session".to_string();
        // A forced ask prompts every time by design — "always/session"
        // grants would be overridden by the ask rule anyway, so don't offer
        // choices that can't stick.
        let choices = match &forced {
            Some(_) => vec!["allow once".into(), "deny".into()],
            None => vec!["allow once".into(), always.clone(), session.clone(), "deny".into()],
        };
        let question = match &forced {
            Some(rule) => format!("Allow bash ('{rule}'): {preview}"),
            None => format!("Allow bash: {preview}"),
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = ask.send(AskRequest { question, detail: vec![], choices, reply: reply_tx });
        match reply_rx.await.as_deref() {
            Ok("allow once") => Ok(()),
            Ok(a) if a == always => {
                self.add_allow_pattern(&pattern);
                // Persistence failure must not fail the approved command —
                // the in-memory allow already covers this session.
                let _ = crate::config::append_user_bash_allow(&pattern);
                Ok(())
            }
            Ok(a) if a == session => {
                if let Ok(mut kinds) = self.approved_kinds.lock() {
                    kinds.insert("bash".to_string());
                }
                Ok(())
            }
            _ => bail!(
                "the user DENIED this bash action. Do not retry it; ask them how to proceed or choose another approach."
            ),
        }
    }

    /// Gate a mutating action behind user approval. Returns Err when denied —
    /// the model sees that as a tool error and adjusts course. `detail`
    /// lines (the pending diff) render above the question, diff-colored.
    /// `path` (write/edit) is judged against the permission rules first:
    /// deny bails, allow skips the prompt, ask forces it even in /yolo.
    async fn check_approval(
        &self,
        kind: &str,
        path: Option<&Path>,
        summary: &str,
        detail: Vec<String>,
    ) -> Result<()> {
        use crate::permissions::Decision;
        let decision = path.and_then(|p| self.path_decision(kind, p));
        let forced = match &decision {
            Some((Decision::Deny, rule)) => {
                bail!("this {kind} was blocked by permission rule '{rule}'")
            }
            Some((Decision::Allow, _)) => return Ok(()),
            Some((Decision::Ask, rule)) => Some(rule.clone()),
            None => None,
        };
        if !self.approval_enabled() && forced.is_none() {
            return Ok(());
        }
        if forced.is_none() && self.approved_kinds.lock().map(|k| k.contains(kind)).unwrap_or(false) {
            return Ok(());
        }
        // Approval requires an interactive user; without one the mode is
        // moot — except an ask rule, which explicitly demands a human.
        let Some(ask) = &self.ask else {
            return match forced {
                Some(rule) => bail!(
                    "this {kind} requires interactive approval (permission rule '{rule}') and no user is attached"
                ),
                None => Ok(()),
            };
        };
        let session = format!("always allow {kind} this session");
        // Persistent grant scoped to the file's work area, e.g. Edit(src/**).
        let persist_rule = path.map(|p| crate::permissions::suggest_edit_rule(p, &self.cwd));
        let persist = persist_rule.as_ref().map(|r| format!("always allow '{r}'"));
        let mut choices = vec!["allow".to_string()];
        if forced.is_none() {
            choices.push(session.clone());
            if let Some(p) = &persist {
                choices.push(p.clone());
            }
        }
        choices.push("deny".into());
        let question = match &forced {
            Some(rule) => format!("Allow {kind} ('{rule}'): {summary}"),
            None => format!("Allow {kind}: {summary}"),
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = ask.send(AskRequest { question, detail, choices, reply: reply_tx });
        match reply_rx.await.as_deref() {
            Ok("allow") => Ok(()),
            Ok(a) if a == session => {
                if let Ok(mut kinds) = self.approved_kinds.lock() {
                    kinds.insert(kind.to_string());
                }
                Ok(())
            }
            Ok(a) if persist.as_deref() == Some(a) => {
                let rule = persist_rule.unwrap_or_default();
                self.add_allow_rule(&rule);
                // Persistence failure must not fail the approved action —
                // the in-memory allow already covers this session.
                let _ = crate::config::append_user_permission_rule("allow", &rule);
                Ok(())
            }
            _ => bail!("the user DENIED this {kind} action. Do not retry it; ask them how to proceed or choose another approach."),
        }
    }

    /// Install the interactive diff-review channel (serve mode / VS Code).
    pub fn set_edit_review(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<EditReviewRequest>>) {
        if let Ok(mut r) = self.edit_review.lock() {
            *r = tx;
        }
    }

    /// Gate a write/edit behind interactive diff review when a review
    /// channel is installed, else the plain approval prompt. Returns the
    /// content to apply: the proposal, or the reviewer's accepted-hunk
    /// subset of it. Err = denied/blocked.
    pub(crate) async fn review_edit(
        &self,
        kind: &str,
        path: &Path,
        old: &str,
        new: &str,
        summary: &str,
        detail: Vec<String>,
    ) -> Result<String> {
        use crate::permissions::Decision;
        let review_tx = self.edit_review.lock().ok().and_then(|r| r.clone());
        let Some(review_tx) = review_tx else {
            self.check_approval(kind, Some(path), summary, detail).await?;
            return Ok(new.to_string());
        };
        // Rules first, same order as the plain gate.
        let decision = self.path_decision(kind, path);
        let forced = match &decision {
            Some((Decision::Deny, rule)) => {
                bail!("this {kind} was blocked by permission rule '{rule}'")
            }
            Some((Decision::Allow, _)) => return Ok(new.to_string()),
            Some((Decision::Ask, rule)) => Some(rule.clone()),
            None => None,
        };
        if !self.approval_enabled() && forced.is_none() {
            return Ok(new.to_string());
        }
        if forced.is_none() && self.approved_kinds.lock().map(|k| k.contains(kind)).unwrap_or(false) {
            return Ok(new.to_string());
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let sent = review_tx.send(EditReviewRequest {
            tool: kind.to_string(),
            path: path.to_path_buf(),
            old: old.to_string(),
            new: new.to_string(),
            reply: reply_tx,
        });
        if sent.is_err() {
            // Reviewer went away (extension closed): fall back to the ask
            // prompt rather than silently applying.
            self.check_approval(kind, Some(path), summary, detail).await?;
            return Ok(new.to_string());
        }
        match reply_rx.await {
            Ok(EditReviewReply::Apply(content)) => Ok(content),
            Ok(EditReviewReply::Deny) | Err(_) => bail!(
                "the user DENIED this {kind} action in review. Do not retry it; ask them how to proceed or choose another approach."
            ),
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

    /// User-configured deny rules (without the built-ins).
    pub fn user_deny_patterns(&self) -> Vec<String> {
        self.permission_rules().2
    }

    fn bash_denied(&self, command: &str) -> bool {
        let Ok(rules) = self.rules.lock() else { return false };
        // Match every chained segment, not just the whole string — otherwise
        // `true && sudo …` or `echo x; rm -rf /` sails past patterns anchored
        // at the start (see bash_segments for the shared normalization).
        // Still best-effort (heavy quoting can evade it); approval mode is
        // the real gate.
        Self::bash_segments(command).iter().any(|seg| rules.bash_deny_match(seg).is_some())
    }

    /// Mark the start of a new agent turn; edits recorded after this group
    /// under the new turn for `undo_last_turn`.
    pub fn begin_turn(&self) {
        self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// The current turn number (checkpoint id) — /rewind targets these.
    pub fn current_turn(&self) -> u64 {
        self.turn.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn record_edit(&self, path: PathBuf, prior: Option<Vec<u8>>) {
        // How many turns back /undo and /rewind can reach. Bounds journal
        // memory: a long session rewriting large files would otherwise hold
        // every prior version until exit.
        const UNDO_KEEP_TURNS: u64 = 20;
        let turn = self.turn.load(std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut j) = self.journal.lock() {
            j.retain(|r| r.turn + UNDO_KEEP_TURNS > turn);
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

    /// Restore every write/edit made AFTER `target` turn (the /rewind
    /// machinery): for each touched file, the earliest snapshot past the
    /// target is its pre-rewind state. Like Claude Code's checkpoints, this
    /// covers the write/edit tools — changes made via bash (git, formatters,
    /// scripts) are outside the journal.
    pub fn undo_to_turn(&self, target: u64) -> Result<Vec<PathBuf>> {
        let records: Vec<EditRecord> = {
            let mut j = self.journal.lock().map_err(|_| anyhow!("edit journal poisoned"))?;
            let taken = j.iter().filter(|r| r.turn > target).cloned().collect();
            j.retain(|r| r.turn <= target);
            taken
        };
        let mut earliest: std::collections::HashMap<PathBuf, EditRecord> = std::collections::HashMap::new();
        for rec in records {
            let replace = earliest.get(&rec.path).map(|e| rec.turn < e.turn).unwrap_or(true);
            if replace {
                earliest.insert(rec.path.clone(), rec);
            }
        }
        let mut restored = Vec::with_capacity(earliest.len());
        for (path, rec) in earliest {
            match &rec.prior {
                Some(content) => std::fs::write(&path, content)
                    .with_context(|| format!("restoring {}", path.display()))?,
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
            restored.push(path);
        }
        restored.sort();
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
                Box::new(TaskTool),
                Box::new(RememberTool),
                Box::new(FetchTool),
                Box::new(WebSearchTool),
            ],
        }
    }

    /// Add a tool (e.g. from an MCP server) to the registry.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Keep only the named tools (persona whitelists). Unknown names are
    /// ignored — a typo narrows the set rather than erroring the spawn.
    pub fn retain_tools(&mut self, allowed: &[String]) {
        self.tools.retain(|t| allowed.iter().any(|a| a == t.name()));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let lean = std::env::var("RIFT_TOOL_SCHEMA").is_ok_and(|v| v.eq_ignore_ascii_case("lean"));
        self.tools
            .iter()
            .map(|t| {
                if lean {
                    lean_def(t.name(), t.description(), t.parameters())
                } else {
                    ToolDef::function(t.name(), t.description(), t.parameters())
                }
            })
            .collect()
    }

    /// Local models frequently hallucinate tool names from their finetuning
    /// data (`read_file` instead of `read`, etc). Map the common variants to
    /// our canonical names instead of failing the call.
    pub fn resolve_alias<'a>(&self, name: &'a str) -> &'a str {
        resolve_alias_impl(name)
    }
}

/// The `RIFT_TOOL_SCHEMA=lean` variant for tool-schema A/B on the bench
/// matrix (ROADMAP v1.8): tool description cut to its first sentence,
/// per-parameter descriptions dropped. Richer schemas may help one family
/// and hurt another — more tokens, more places for a small model to
/// hallucinate — so both variants are measurable without a rebuild.
fn lean_def(name: &str, description: &str, mut parameters: serde_json::Value) -> ToolDef {
    let first = match description.find(". ") {
        Some(end) => &description[..=end],
        None => description,
    };
    strip_schema_annotations(&mut parameters);
    ToolDef::function(name, first.trim(), parameters)
}

/// Remove `description`/`examples` annotations from every schema node,
/// recursively. Only nodes that carry a `type` are schema nodes — a
/// `properties` map is not, so a parameter that happens to be NAMED
/// "description" survives.
fn strip_schema_annotations(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if map.contains_key("type") {
                map.remove("description");
                map.remove("examples");
            }
            for (_, val) in map.iter_mut() {
                strip_schema_annotations(val);
            }
        }
        serde_json::Value::Array(items) => {
            for val in items {
                strip_schema_annotations(val);
            }
        }
        _ => {}
    }
}

fn resolve_alias_impl(name: &str) -> &str {
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
        "internet_search" | "google" | "web" | "duckduckgo" | "search_web" => "web_search",
        "web_fetch" | "http_get" | "get_url" | "fetch_url" | "curl" | "wget" => "fetch",
        "tasks" | "bg" | "task_status" | "check_task" | "background_task" | "task_output" => "task",
        "Task" | "subagent" | "sub_agent" | "spawn_agent" | "delegate" | "dispatch_agent" => "agent",
        other => other,
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

// The similar-path walk is synchronous directory I/O (up to 5000 entries) —
// run it off the async runtime like the grep/glob walks, or a bad path on a
// big repo / network drive stalls the whole event loop.
async fn enoent_hint(cwd: &Path, path: &Path) -> String {
    let cwd = cwd.to_path_buf();
    let missing = path.to_path_buf();
    let found = tokio::task::spawn_blocking(move || suggest_similar_paths(&cwd, &missing))
        .await
        .unwrap_or_default();
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
        "Read a text file. Returns line-numbered content. An unbounded read of a large source file returns its outline first — pass offset/limit to read exact line ranges."
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
        ctx.check_read_allowed("read", &path)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{}", enoent_hint(&ctx.cwd, &path).await),
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
        if total_lines == 0 {
            // A legitimately empty file, not a bad offset — an error here
            // sends the model chasing other offsets.
            return Ok("(empty file)\n".into());
        }
        // Hydrate-on-demand (v0.8): outline first for big files, exact
        // ranges as the model asks. Only when the model gave no bounds —
        // an explicit offset/limit is always honored verbatim.
        let bounded = args.contains_key("offset") || args.contains_key("limit");
        if !bounded && total_lines > READ_HYDRATE_LINES && crate::outline::supports(&path) {
            if let Ok(outline) = crate::outline::outline_source(&path, &text) {
                return Ok(format!(
                    "{} has {total_lines} lines — showing its outline (line numbers on the left). \
                     Re-read with offset/limit for the exact ranges you need.\n{outline}",
                    path.display()
                ));
            }
        }
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
        // Snapshot as bytes: a prior read failure other than NotFound must
        // abort the write, not degrade to "file didn't exist" — undo would
        // then delete a file it should restore. Read before the approval so
        // the prompt can show what actually changes.
        let prior = match tokio::fs::read(&path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot snapshot {} for undo", path.display())),
        };
        let summary = format!("{} ({} bytes)", path.display(), content.len());
        // Binary prior: no line diff to review — plain approval prompt.
        let content = match &prior {
            Some(bytes) if looks_binary(bytes) => {
                ctx.check_approval(
                    "write",
                    Some(&path),
                    &summary,
                    vec!["(overwriting a binary file)".into()],
                )
                .await?;
                content.to_string()
            }
            _ => {
                let old = prior.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                let diff = preview_diff(&old, content, 40);
                // Interactive review may return a hunk-filtered subset.
                ctx.review_edit("write", &path, &old, content, &summary, diff).await?
            }
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        ctx.record_edit(path.clone(), prior.clone());
        tokio::fs::write(&path, &content).await.with_context(|| format!("cannot write {}", path.display()))?;
        // Surface what actually changed (post-review content) — skipped for
        // binary priors, where a line diff means nothing.
        if !prior.as_ref().is_some_and(|b| looks_binary(b)) {
            let old = prior.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            ctx.record_last_diff(&path, preview_diff(&old, &content, 80));
        }
        let mut result = format!("Wrote {} bytes to {}", content.len(), path.display());
        if content != req_str(args, "content")? {
            result.push_str(
                "\nNOTE: the user accepted only part of your proposal in review — re-read the file to see what was applied.",
            );
        }
        // Diagnostics before the hooks: hooks may rewrite the file (fmt),
        // and the server should see exactly what was written.
        if let Some(diags) = ctx.lsp_diagnostics(&path, &content).await {
            result.push_str(&format!("\n── diagnostics ──\n{diags}"));
        }
        if let Some(report) = ctx.run_post_edit_hooks(&path).await {
            result.push_str(&report);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------- edit

/// Reconcile line endings between `old_string`/`new_string` and the on-disk
/// text. The read tool strips `\r` from CRLF files (via `str::lines`), so
/// models routinely produce LF-only strings for Windows files that never match
/// the raw CRLF bytes. When the literal `old` isn't present but the file's
/// dominant line ending differs, promote/demote `old` (and `new`) to the file's
/// convention so the match succeeds and the write keeps the file's endings.
/// Only rewrites when the converted `old` actually matches; genuine mismatches
/// fall through unchanged so the normal diagnostics still fire.
fn reconcile_line_endings(text: &str, old: &str, new: &str) -> (String, String) {
    if text.contains(old) {
        return (old.to_string(), new.to_string());
    }
    let to_crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
    let to_lf = |s: &str| s.replace("\r\n", "\n");
    let file_crlf = text.contains("\r\n");
    if file_crlf && !old.contains('\r') {
        // File is CRLF, old_string arrived LF-only: promote both to CRLF.
        let old_crlf = to_crlf(old);
        if text.contains(&old_crlf) {
            return (old_crlf, to_crlf(new));
        }
    } else if !file_crlf && old.contains("\r\n") {
        // File is LF, old_string carries CRLF: demote both to LF.
        let old_lf = to_lf(old);
        if text.contains(&old_lf) {
            return (old_lf, to_lf(new));
        }
    }
    (old.to_string(), new.to_string())
}

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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{}", enoent_hint(&ctx.cwd, &path).await),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        // The read tool renders CRLF files with their \r stripped (str::lines
        // drops it), so a model copying those lines back produces an LF-only
        // old_string that never matches the raw CRLF text on disk — the loop
        // that leaves Windows edits stuck. Reconcile the line endings so the
        // match succeeds and the replacement preserves the file's convention.
        let (old, new) = reconcile_line_endings(&text, old, new);
        let (old, new): (&str, &str) = (&old, &new);
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
        let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        // Interactive review may return a hunk-filtered subset of the change.
        let applied = ctx
            .review_edit(
                "edit",
                &path,
                &text,
                &updated,
                &path.display().to_string(),
                preview_diff(&text, &updated, 40),
            )
            .await?;
        ctx.record_edit(path.clone(), Some(text.clone().into_bytes()));
        tokio::fs::write(&path, &applied).await?;
        ctx.record_last_diff(&path, preview_diff(&text, &applied, 80));
        let mut result =
            format!("Edited {} ({} replacement{})", path.display(), count, if count == 1 { "" } else { "s" });
        if applied != updated {
            result.push_str(
                "\nNOTE: the user accepted only part of your proposal in review — re-read the file to see what was applied.",
            );
        }
        if let Some(diags) = ctx.lsp_diagnostics(&path, &applied).await {
            result.push_str(&format!("\n── diagnostics ──\n{diags}"));
        }
        if let Some(report) = ctx.run_post_edit_hooks(&path).await {
            result.push_str(&report);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------- bash

/// Substitute a command into the sandbox wrapper template: `{cmd}` gets the
/// command (single-quote-escaped so `sh -c '{cmd}'` forms survive quotes in
/// the command), `{cwd}` the working directory.
fn wrap_sandbox(template: &str, command: &str, cwd: &Path) -> String {
    let escaped = command.replace('\'', "'\\''");
    template.replace("{cmd}", &escaped).replace("{cwd}", &cwd.display().to_string())
}

/// Build a shell invocation for the host platform. On Windows commands run
/// through `cmd.exe /C` (honoring %COMSPEC%); everywhere else through `sh -c`.
/// This is what lets the bash tool behave the same on macOS, Linux and Windows
/// instead of failing to spawn `sh` on Windows.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut cmd = std::process::Command::new(shell);
        // Rust's default argument quoting targets the MSVCRT parser, but
        // cmd.exe uses its own rules and mangles embedded double quotes —
        // a `python -c "import x"` argument came out as a broken `"import`
        // and the command failed. Build the command line ourselves with
        // `raw_arg` and use `/S`, which tells cmd to strip exactly the outer
        // quote pair and run everything between them untouched. That keeps
        // any quoting inside `command` intact for any command shape.
        cmd.raw_arg(format!("/S /C \"{command}\""));
        tokio::process::Command::from(cmd)
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
         Set run_in_background=true for long jobs (big builds, full test suites, dev servers): \
         the command keeps running while you continue working, you get a task id back \
         immediately, a [task notification] arrives when it finishes, and the task tool shows \
         its output any time. Foreground servers/watchers (npm run dev, vite, …) are auto-killed \
         after a short probe — run those in the background instead."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "Shell command to run"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 60, max 300); ignored with run_in_background"},
                "run_in_background": {"type": "boolean", "description": "true = don't wait: run concurrently as a background task and return its id immediately"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let command = req_str(args, "command")?;
        if ctx.bash_denied(command) {
            bail!("command blocked by permission policy: {command}");
        }
        ctx.check_bash_approval(command).await?;
        if args.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false) {
            return bash_background(command, ctx);
        }
        let explicit_timeout = opt_u64(args, "timeout_secs");
        let server_like = looks_like_server_command(command);
        let mut timeout = explicit_timeout.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS).min(BASH_MAX_TIMEOUT_SECS);
        if server_like && explicit_timeout.is_none() {
            timeout = timeout.min(SERVER_PROBE_SECS);
        }

        // Deny list and approval saw the RAW command; execution routes
        // through the sandbox wrapper when one is configured.
        let effective = ctx.effective_command(command);
        let mut cmd = shell_command(&effective);
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

/// The bash tool's background mode: spawn the command as a session-wide
/// background task and return immediately. Output accumulates in the task
/// registry; completion notifies the frontend (which turns it into a
/// [task notification] for the model). Tasks die with the rift process —
/// no orphans survive an exit.
fn bash_background(command: &str, ctx: &ToolCtx) -> Result<String> {
    let mut cmd = shell_command(&ctx.effective_command(command));
    cmd.current_dir(&ctx.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().context("spawning shell")?;
    let pid = child.id();
    let (id, cancel) = ctx.bg().register(crate::tasks::TaskKind::Shell, command, pid)?;

    let reg = ctx.bg().clone();
    // Interactive input: lines sent via the task tool (or /tasks send) are
    // written to the process's stdin, so REPLs and y/n prompts stay usable.
    if let Some(mut stdin) = child.stdin.take() {
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        reg.set_input(id, in_tx);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(line) = in_rx.recv().await {
                if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
    }
    let out_task = tokio::spawn(pump(child.stdout.take(), reg.clone(), id));
    let err_task = tokio::spawn(pump(child.stderr.take(), reg.clone(), id));
    tokio::spawn(async move {
        let status = tokio::select! {
            biased;
            // Killed via the task tool: the registry already tree-killed the
            // pid, so the pipes close and the readers drain what's left.
            _ = cancel.cancelled() => None,
            s = child.wait() => s.ok(),
        };
        let _ = out_task.await;
        let _ = err_task.await;
        reg.finish(id, status.and_then(|s| s.code()).or(Some(-1)));
    });

    Ok(format!(
        "started background task #{id}{}: {command}\n\
         It keeps running while you continue — do other work or end your turn; a [task \
         notification] arrives when it completes. Poll status/output any time with the task \
         tool (id={id}).",
        pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
    ))
}

/// Stream one pipe of a background command into its task output buffer.
async fn pump<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>, reg: crate::tasks::BgTasks, id: u64) {
    use tokio::io::AsyncReadExt;
    let Some(mut p) = pipe else { return };
    let mut buf = [0u8; 8192];
    loop {
        match p.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => reg.append_output(id, &String::from_utf8_lossy(&buf[..n])),
        }
    }
}

// ---------------------------------------------------------------- fetch

/// Minimal web fetch: GET a URL, strip HTML down to readable text, cap the
/// size. Enough for docs pages, READMEs, and API references; anything
/// fancier (search, auth, JS rendering) belongs to an MCP server.
struct FetchTool;

const FETCH_MAX_OUTPUT: usize = 20_000;
const FETCH_TIMEOUT_SECS: u64 = 20;

/// Strip HTML to readable text: drops tags, script/style bodies, comments;
/// decodes a handful of common entities; collapses blank runs.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let lower = html.to_lowercase();
    let mut skip_until: Option<usize> = None;
    for (i, c) in html.char_indices() {
        if let Some(end) = skip_until {
            if i < end {
                continue;
            }
            skip_until = None;
        }
        if c == '<' {
            // script/style/comment bodies vanish entirely.
            for (open, close) in [("<script", "</script>"), ("<style", "</style>"), ("<!--", "-->")] {
                if lower[i..].starts_with(open) {
                    if let Some(rel) = lower[i..].find(close) {
                        skip_until = Some(i + rel + close.len());
                    } else {
                        skip_until = Some(html.len());
                    }
                    break;
                }
            }
            if skip_until.is_some() {
                continue;
            }
            // Block-level closers become newlines so structure survives.
            for tag in ["</p>", "</div>", "</li>", "</h1>", "</h2>", "</h3>", "</h4>", "</tr>", "<br"] {
                if lower[i..].starts_with(tag) {
                    out.push('\n');
                    break;
                }
            }
            // Skip to the tag's end.
            let mut end = html.len();
            for (j, cc) in html[i..].char_indices() {
                if cc == '>' {
                    end = i + j + 1;
                    break;
                }
            }
            skip_until = Some(end);
            continue;
        }
        out.push(c);
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse whitespace runs while keeping paragraph breaks.
    let mut lines: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            lines.push(line);
        } else if !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
    }
    lines.join("
")
}

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL (GET) and return its content as readable text: HTML is stripped to text,          JSON and plain text pass through. For docs, READMEs, changelogs, API references.          Output is capped; no auth, no JS rendering."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string", "description": "http(s) URL to fetch"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let url = req_str(args, "url")?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("only http(s) URLs are supported");
        }
        ctx.check_url_allowed("fetch", url)?;
        // Deny rules must hold across redirects too — a permitted URL 302ing
        // to a denied one is the classic bypass. Same timeouts as the shared
        // client, plus a per-hop permission check.
        let rules = ctx.rules_snapshot();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > 10 {
                    return attempt.error("too many redirects");
                }
                let hop = attempt.url().to_string();
                let candidates = crate::permissions::url_candidates(&hop);
                if let Some((crate::permissions::Decision::Deny, rule)) = rules.decide("fetch", &candidates) {
                    return attempt.error(format!("redirect to {hop} blocked by permission rule '{rule}'"));
                }
                attempt.follow()
            }))
            .build()
            .unwrap_or_else(|_| rift_provider::http_client());
        let resp = tokio::time::timeout(
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            client.get(url).header("user-agent", concat!("rift/", env!("CARGO_PKG_VERSION"))).send(),
        )
        .await
        .map_err(|_| anyhow!("fetch timed out after {FETCH_TIMEOUT_SECS}s"))?
        .with_context(|| format!("fetching {url}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = tokio::time::timeout(Duration::from_secs(FETCH_TIMEOUT_SECS), resp.text())
            .await
            .map_err(|_| anyhow!("fetch timed out reading the body"))??;
        let text = if content_type.contains("html") { html_to_text(&body) } else { body };
        let mut out = format!("[{status} {content_type}] {url}

");
        out.push_str(&truncate_middle(text.trim(), FETCH_MAX_OUTPUT));
        Ok(out)
    }
}

// ---------------------------------------------------------------- web_search

/// Web search through a SearXNG instance (config `search_url`, /search).
/// SearXNG aggregates engines locally, which keeps queries off third-party
/// APIs — the local-first way to give models the internet.
struct WebSearchTool;

const SEARCH_MAX_RESULTS: usize = 8;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web (via the configured SearXNG instance). Returns titles, URLs, and          snippets; follow up with the fetch tool to read a result in full."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string", "description": "Search query"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let query = req_str(args, "query")?;
        let Some(base) = ctx.search_url() else {
            bail!(
                "web search is not configured — the user can set it with /search <searxng-url> or                  search_url in the config. Proceed without web results or ask them to configure it."
            );
        };
        let client = rift_provider::http_client();
        let url = format!("{base}/search");
        let resp = tokio::time::timeout(
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            client
                .get(&url)
                .query(&[("q", query), ("format", "json")])
                .header("user-agent", concat!("rift/", env!("CARGO_PKG_VERSION")))
                .send(),
        )
        .await
        .map_err(|_| anyhow!("search timed out after {FETCH_TIMEOUT_SECS}s"))?
        .with_context(|| format!("searching via {base}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "search endpoint {base} returned {status} — if this is 403, the SearXNG instance                  must allow format=json (settings.yml: search.formats)"
            );
        }
        let body: Value = resp.json().await.with_context(|| format!("invalid JSON from {base}"))?;
        let results = body.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        if results.is_empty() {
            return Ok(format!("no results for: {query}"));
        }
        let mut out = format!("results for: {query}
");
        for r in results.iter().take(SEARCH_MAX_RESULTS) {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("(untitled)");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let snippet: String = content.chars().take(240).collect();
            out.push_str(&format!("
- {title}
  {url}
  {snippet}
"));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- remember

/// The model's write access to project memory (.rift/memory.md): durable
/// facts loaded into the system prompt of every future session.
struct RememberTool;

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }
    fn description(&self) -> &str {
        "Save a short durable fact to project memory (.rift/memory.md), loaded in every future          session. Use for non-obvious, lasting discoveries: build quirks, hidden conventions,          decisions and their reasons. Not for session-scoped state or anything obvious from the code."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["fact"],
            "properties": {
                "fact": {"type": "string", "description": "The fact to save - one to a few lines, self-contained"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let fact = req_str(args, "fact")?;
        let path = crate::memory::append_memory(&ctx.cwd, fact)?;
        Ok(format!("Remembered (saved to {}). It loads into the system prompt next session.", path.display()))
    }
}

// ---------------------------------------------------------------- task

/// Model-facing view of the background task table: list, inspect, kill.
struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Manage background tasks (bash run_in_background commands and background sub-agents). \
         No arguments: list all tasks with ids and statuses. With id: that task's status and \
         accumulated output. With id and input: send a line to the task's stdin (answer REPLs \
         and y/n prompts). With id and kill=true: terminate it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Task id (from the bash/agent tool result or the list)"},
                "input": {"type": "string", "description": "Line to write to the running task's stdin (newline appended)"},
                "close_stdin": {"type": "boolean", "description": "true = send EOF (programs that read stdin to the end finish only after this)"},
                "kill": {"type": "boolean", "description": "true = terminate the task instead of reading it"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, ctx: &ToolCtx) -> Result<String> {
        let Some(id) = opt_u64(args, "id") else {
            let tasks = ctx.bg().list();
            if tasks.is_empty() {
                return Ok("(no background tasks this session — start one with bash \
                           run_in_background=true or agent background=true)"
                    .into());
            }
            let mut out = String::from("background tasks:\n");
            for t in tasks {
                out.push_str(&format!(
                    "#{} [{}] {} ({}, {}s, {} bytes of output)\n",
                    t.id,
                    t.status.describe(),
                    t.label,
                    t.kind.label(),
                    t.elapsed_secs,
                    t.output_bytes
                ));
            }
            return Ok(out.trim_end().to_string());
        };
        if args.get("kill").and_then(|v| v.as_bool()).unwrap_or(false) {
            let view = ctx.bg().kill(id)?;
            return Ok(format!("killed task #{id} ({}) after {}s", view.label, view.elapsed_secs));
        }
        if let Some(line) = args.get("input").and_then(|v| v.as_str()) {
            ctx.bg().send_input(id, line)?;
            return Ok(format!(
                "sent to task #{id}'s stdin: {line}\nCheck the task's output shortly to see how it reacted."
            ));
        }
        if args.get("close_stdin").and_then(|v| v.as_bool()).unwrap_or(false) {
            ctx.bg().close_input(id)?;
            return Ok(format!("closed task #{id}'s stdin (EOF) — programs reading to the end will now finish"));
        }
        let (view, output) = ctx
            .bg()
            .output_of(id)
            .ok_or_else(|| anyhow!("no background task #{id} — call task with no arguments to list them"))?;
        let body = strip_ansi(&output);
        let body = if body.trim().is_empty() { "(no output yet)".to_string() } else { truncate_middle(body.trim(), 20_000) };
        Ok(format!(
            "task #{id} [{}] {} ({}, {}s elapsed)\n--- output ---\n{}",
            view.status.describe(),
            view.label,
            view.kind.label(),
            view.elapsed_secs,
            body
        ))
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
        ask.send(AskRequest { question, detail: vec![], choices, reply: reply_tx })
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
        ctx.check_list_allowed("ls", &path)?;
        let rules = ctx.rules_snapshot();
        let mut rd = tokio::fs::read_dir(&path).await.with_context(|| format!("cannot list {}", path.display()))?;
        let mut entries = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            // Individually denied entries stay out of the listing too.
            if rules.read_denied("ls", &path.join(e.file_name()), &ctx.cwd) {
                continue;
            }
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
        ctx.check_list_allowed("grep", &root)?;
        let cwd = ctx.cwd.clone();
        let rules = ctx.rules_snapshot();
        // File walking + regex matching is blocking work.
        tokio::task::spawn_blocking(move || {
            let re = regex::Regex::new(&pattern).map_err(|e| anyhow!("invalid regex: {e}"))?;
            let mut results = Vec::new();
            let walker = ignore::WalkBuilder::new(&root).hidden(true).build();
            'outer: for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                // Deny rules (`Read(secrets/**)`) hold inside the walk too.
                if rules.read_denied("grep", entry.path(), &cwd) {
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
        ctx.check_read_allowed("outline", &path)?;
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
            // Bound the ranking pool: newest 200 files are outlined/parsed;
            // in a bigger repo the long tail was never going to make the map.
            files.truncate(200);

            // Persistent outline cache: a (mtime, size) hit skips the read
            // AND the tree-sitter parse — on big repos that's most of the
            // tool's cost across sessions.
            let mut cache = crate::outline_cache::OutlineCache::load(&root);
            let mut entries: Vec<(std::time::SystemTime, PathBuf, String, Vec<String>)> = Vec::new();
            for (mtime, path) in files {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                let (outline, imports) = match cache.get(&path, &meta) {
                    Some(hit) => hit,
                    None => {
                        let Ok(source) = std::fs::read_to_string(&path) else { continue };
                        let Ok(fresh) = crate::outline::outline_source(&path, &source) else { continue };
                        let imports = crate::outline::extract_imports(&path, &source);
                        cache.insert(&path, &meta, fresh.clone(), imports.clone());
                        (fresh, imports)
                    }
                };
                entries.push((mtime, path, outline, imports));
            }
            cache.save();

            // Ranking v2 (docs/ROADMAP.md v0.8): centrality first, recency
            // as tiebreak. A file's in-degree = how many sibling files
            // import its stem — central modules make the map even when they
            // weren't touched recently. Repos with no resolvable imports
            // degrade to the old pure-recency order.
            let mut in_degree: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for (_, _, _, imports) in &entries {
                for import in imports {
                    in_degree.entry(import.clone()).or_insert(0);
                }
            }
            for (_, path, _, _) in &entries {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let stem = stem.to_lowercase();
                    let count = entries
                        .iter()
                        .filter(|(_, p, _, imports)| p != path && imports.contains(&stem))
                        .count() as u32;
                    in_degree.insert(stem, count);
                }
            }
            entries.sort_by_key(|(mtime, path, _, _)| {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                std::cmp::Reverse((in_degree.get(&stem).copied().unwrap_or(0), *mtime))
            });

            let mut out = String::new();
            let mut included = 0;
            for (_, path, outline, _) in entries.into_iter().take(max_files) {
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
                    "[{included} of {total_found} source files shown, most-imported first then newest; use outline for specific files]\n"
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
        ctx.check_list_allowed("glob", &root)?;
        let cwd = ctx.cwd.clone();
        let rules = ctx.rules_snapshot();
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
                // Deny rules hide matching paths from listings too.
                if rules.read_denied("glob", entry.path(), &cwd) {
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
    fn lean_schema_trims_annotations_but_never_structure() {
        let def = lean_def(
            "read",
            "Read a file's contents. Large files are outlined instead; fetch exact ranges with offset/limit.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "limit": {"type": "integer", "description": "Max lines", "examples": [100]},
                    // A parameter literally named "description" must survive
                    // (it is a properties key, not a schema annotation).
                    "description": {"type": "string", "description": "annotation to strip"}
                },
                "required": ["path"]
            }),
        );
        assert_eq!(def.function.description, "Read a file's contents.");
        let props = &def.function.parameters["properties"];
        assert!(props["path"].get("description").is_none());
        assert!(props["limit"].get("description").is_none());
        assert!(props["limit"].get("examples").is_none());
        // Structure intact: types, required, and the awkwardly-named param.
        assert_eq!(props["path"]["type"], "string");
        assert_eq!(def.function.parameters["required"][0], "path");
        assert_eq!(props["description"]["type"], "string");
        assert!(props["description"].get("description").is_none());
        // Single-sentence descriptions pass through whole.
        let short = lean_def("ls", "List a directory", serde_json::json!({"type": "object"}));
        assert_eq!(short.function.description, "List a directory");
    }

    #[tokio::test]
    async fn write_and_edit_record_a_last_diff() {
        let dir = std::env::temp_dir().join(format!("rift-lastdiff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx::new(&dir);
        let reg = ToolRegistry::standard();

        let args: Map<String, Value> =
            serde_json::from_value(serde_json::json!({"path": "f.txt", "content": "a\nb\n"})).unwrap();
        reg.get("write").unwrap().execute(&args, &ctx).await.unwrap();
        let (path, diff) = ctx.take_last_diff().expect("write records a diff");
        assert!(path.ends_with("f.txt"));
        assert!(diff.iter().any(|l| l == "+a"), "got: {diff:?}");
        // take drains: the agent loop consumes it once per call.
        assert!(ctx.take_last_diff().is_none());

        let args: Map<String, Value> = serde_json::from_value(
            serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "c"}),
        )
        .unwrap();
        reg.get("edit").unwrap().execute(&args, &ctx).await.unwrap();
        let (_, diff) = ctx.take_last_diff().expect("edit records a diff");
        assert!(diff.iter().any(|l| l == "-b"), "got: {diff:?}");
        assert!(diff.iter().any(|l| l == "+c"), "got: {diff:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn web_search_parses_searxng_results() {
        // Unconfigured -> a clear, actionable error for the model.
        let bare = ToolCtx::new("/tmp");
        let mut args = Map::new();
        args.insert("query".into(), Value::String("rust".into()));
        let err = WebSearchTool.execute(&args, &bare).await.unwrap_err();
        assert!(err.to_string().contains("/search"), "got: {err}");

        // Configured -> titles, urls, snippets from the JSON API.
        let server = rift_provider::test_support::MockServer::start(vec![
            rift_provider::test_support::MockResponse::json(
                200,
                "{\"results\":[{\"title\":\"Rust Language\",\"url\":\"https://rust-lang.org\",\"content\":\"A systems language.\"},{\"title\":\"Rust Book\",\"url\":\"https://doc.rust-lang.org/book\",\"content\":\"Learn Rust.\"}]}",
            ),
        ])
        .await;
        let ctx = ToolCtx::new("/tmp");
        ctx.set_search_url(Some(server.base_url.clone()));
        let out = WebSearchTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("Rust Language"), "got: {out}");
        assert!(out.contains("https://rust-lang.org"));
        assert!(out.contains("A systems language."));
        let req = &server.requests().await[0];
        assert!(req.contains("format=json"), "must use the JSON API: {req}");
    }

    #[test]
    fn sandbox_wrapper_substitutes_and_escapes() {
        let cwd = Path::new("/work");
        // {cmd} and {cwd} substitute; single quotes in the command survive
        // an sh -c '{cmd}' wrapper form.
        assert_eq!(
            wrap_sandbox("wsl -e sh -c '{cmd}'", "echo hi", cwd),
            "wsl -e sh -c 'echo hi'"
        );
        assert_eq!(
            wrap_sandbox("docker run -v {cwd}:/w alpine sh -c '{cmd}'", "echo 'a b'", cwd),
            "docker run -v /work:/w alpine sh -c 'echo '\\''a b'\\'''"
        );
        // No wrapper configured → commands run untouched.
        let ctx = ToolCtx::new("/tmp");
        assert_eq!(ctx.effective_command("ls"), "ls");
        // Templates without {cmd} are refused (they'd swallow the command).
        ctx.set_bash_wrapper(Some("firejail".into()));
        assert_eq!(ctx.effective_command("ls"), "ls");
        ctx.set_bash_wrapper(Some("echo SBX&& {cmd}".into()));
        assert_eq!(ctx.effective_command("ls"), "echo SBX&& ls");
    }

    #[tokio::test]
    async fn sandbox_wrapper_routes_bash_end_to_end() {
        let dir = std::env::temp_dir();
        let ctx = ToolCtx::new(&dir);
        // A prefix wrapper proves the wrapped form is what actually ran.
        ctx.set_bash_wrapper(Some("echo SANDBOXED&& {cmd}".into()));
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo inner".into()));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("SANDBOXED"), "wrapper did not run: {out}");
        assert!(out.contains("inner"), "command did not run: {out}");
    }

    #[test]
    fn html_to_text_strips_markup() {
        let html = "<html><head><style>body{color:red}</style><script>var x=1;</script></head>\
                    <body><h1>Title</h1><p>Hello &amp; welcome</p><!-- hidden -->\
                    <div>Second   line</div></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(text.contains("Second line"));
        assert!(!text.contains("color:red"), "style leaked: {text}");
        assert!(!text.contains("var x"), "script leaked: {text}");
        assert!(!text.contains("hidden"), "comment leaked: {text}");
        assert!(!text.contains('<'), "tags leaked: {text}");
    }

    #[test]
    fn preview_diff_trims_context_and_caps() {
        // Only the changed middle shows, with a location hint.
        let old = "a\nb\nc\nd";
        let new = "a\nB\nC\nd";
        let d = preview_diff(old, new, 40);
        assert_eq!(d, vec!["@@ line 2 @@", "-b", "-c", "+B", "+C"]);
        // New file: all additions, no hunk header.
        assert_eq!(preview_diff("", "x\ny", 40), vec!["+x", "+y"]);
        // Identical: empty.
        assert!(preview_diff("same", "same", 40).is_empty());
        // Cap: long diffs truncate with a count.
        let old: String = (0..100).map(|i| format!("o{i}\n")).collect();
        let new: String = (0..100).map(|i| format!("n{i}\n")).collect();
        let d = preview_diff(&old, &new, 10);
        assert_eq!(d.len(), 11);
        assert!(d.last().unwrap().contains("more diff lines"));
    }

    #[test]
    fn diff_segments_hunks_a_line_diff() {
        use DiffSegment::{Change, Same};
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Two separated changes → two hunks with shared context between.
        assert_eq!(
            diff_segments("a\nb\nc\nd\ne", "a\nB\nc\nd\nE"),
            vec![
                Same(s(&["a"])),
                Change { old: s(&["b"]), new: s(&["B"]) },
                Same(s(&["c", "d"])),
                Change { old: s(&["e"]), new: s(&["E"]) },
            ]
        );
        // Pure insertion keeps its surroundings as one Same run each side.
        assert_eq!(
            diff_segments("a\nc", "a\nb\nc"),
            vec![Same(s(&["a"])), Change { old: vec![], new: s(&["b"]) }, Same(s(&["c"]))]
        );
        // Identical input is a single Same segment.
        assert_eq!(diff_segments("x\ny", "x\ny"), vec![Same(s(&["x", "y"]))]);
        // Reassembling "all hunks accepted" must reproduce `new` exactly —
        // the invariant the edit reviewer's apply path depends on.
        let (old, new) = ("fn a() {}\nfn b() {}\nfn c() {}", "fn a() {}\nfn B() {}\nfn c() {}\nfn d() {}");
        let rebuilt: Vec<String> = diff_segments(old, new)
            .into_iter()
            .flat_map(|seg| match seg {
                Same(lines) => lines,
                Change { new, .. } => new,
            })
            .collect();
        assert_eq!(rebuilt.join("\n"), new);
        // Past the edit-distance cap the diff degrades to ONE whole-file
        // hunk — review still works, never panics.
        let big_old: String = (0..3000).map(|i| format!("o{i}\n")).collect();
        let big_new: String = (0..3000).map(|i| format!("n{i}\n")).collect();
        let segs = diff_segments(&big_old, &big_new);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Change { old, new } if old.len() == 3001 && new.len() == 3001));
    }

    #[tokio::test]
    async fn post_edit_hooks_feed_failures_back() {
        let dir = std::env::temp_dir().join(format!("rift-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx::new(&dir);
        // No hooks configured → silent.
        assert!(ctx.run_post_edit_hooks(Path::new("x.rs")).await.is_none());
        // A passing hook stays out of the tool result.
        ctx.set_post_edit_hooks(&["exit 0".to_string()]);
        assert!(ctx.run_post_edit_hooks(Path::new("x.rs")).await.is_none());
        // A failing hook's output and exit code reach the model.
        ctx.set_post_edit_hooks(&["echo boom&& exit 3".to_string()]);
        let report = ctx.run_post_edit_hooks(Path::new("x.rs")).await.expect("failure must report");
        assert!(report.contains("FAILED"), "got: {report}");
        assert!(report.contains("boom"), "hook output missing: {report}");
        assert!(report.contains("exit 3"), "exit code missing: {report}");
        // And it rides an edit tool result end-to-end.
        let mut args = Map::new();
        args.insert("path".into(), Value::String(dir.join("hooked.txt").display().to_string()));
        args.insert("content".into(), Value::String("hello".into()));
        let result = WriteTool.execute(&args, &ctx).await.unwrap();
        assert!(result.starts_with("Wrote 5 bytes"));
        assert!(result.contains("post-edit hook FAILED"), "got: {result}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn undo_to_turn_restores_across_turns() {
        let dir = std::env::temp_dir().join(format!("rift-rewind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx::new(&dir);
        let file = dir.join("f.txt");
        let write = |content: &str| {
            let mut args = Map::new();
            args.insert("path".into(), Value::String(file.display().to_string()));
            args.insert("content".into(), Value::String(content.into()));
            args
        };
        // Turn 1 creates, turns 2 and 3 rewrite.
        ctx.begin_turn();
        WriteTool.execute(&write("v1"), &ctx).await.unwrap();
        ctx.begin_turn();
        WriteTool.execute(&write("v2"), &ctx).await.unwrap();
        ctx.begin_turn();
        WriteTool.execute(&write("v3"), &ctx).await.unwrap();
        // Rewind past turns 3 and 2 → the file is back at v1.
        let restored = ctx.undo_to_turn(1).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        // Rewinding past turn 1 removes the file entirely (didn't exist).
        ctx.begin_turn();
        WriteTool.execute(&write("v4"), &ctx).await.unwrap();
        let _ = ctx.undo_to_turn(0).unwrap();
        assert!(!file.exists(), "file should be gone before its creating turn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_task_stdin_round_trips() {
        let dir = std::env::temp_dir();
        let ctx = ToolCtx::new(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.bg().set_notify(tx);
        // A filter that copies matching stdin lines and exits on EOF —
        // exercising both send_input and close_input, portably.
        #[cfg(windows)]
        let cmd = "findstr REPLY";
        #[cfg(not(windows))]
        let cmd = "grep REPLY";
        let mut args = Map::new();
        args.insert("command".into(), Value::String(cmd.into()));
        args.insert("run_in_background".into(), Value::Bool(true));
        BashTool.execute(&args, &ctx).await.unwrap();
        // TaskStarted first; then feed stdin, close it (EOF), and expect the
        // echoed line in the finish preview.
        matches!(rx.recv().await.unwrap(), crate::agent::AgentEvent::TaskStarted { .. });
        ctx.bg().send_input(1, "REPLY-hello").unwrap();
        ctx.bg().close_input(1).unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("task never finished")
            .unwrap();
        match ev {
            crate::agent::AgentEvent::TaskFinished { preview, .. } => {
                assert!(preview.contains("REPLY-hello"), "stdin did not reach the task: {preview}");
            }
            other => panic!("expected TaskFinished, got {other:?}"),
        }
        // Input to a finished task errors cleanly.
        assert!(ctx.bg().send_input(1, "again").is_err());
    }

    #[tokio::test]
    async fn bash_background_runs_and_notifies() {
        let dir = std::env::temp_dir();
        let ctx = ToolCtx::new(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.bg().set_notify(tx);

        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo bg-hello".into()));
        args.insert("run_in_background".into(), Value::Bool(true));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("background task #1"), "unexpected: {out}");

        // Started event fires immediately; Finished arrives when the process
        // exits (bounded wait so a hang fails the test instead of wedging it).
        match rx.recv().await.unwrap() {
            crate::agent::AgentEvent::TaskStarted { id, .. } => assert_eq!(id, 1),
            other => panic!("expected TaskStarted, got {other:?}"),
        }
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("background task never finished")
            .unwrap();
        match ev {
            crate::agent::AgentEvent::TaskFinished { id, ok, preview, .. } => {
                assert_eq!(id, 1);
                assert!(ok);
                assert!(preview.contains("bg-hello"), "output not captured: {preview}");
            }
            other => panic!("expected TaskFinished, got {other:?}"),
        }

        // The task tool sees the same registry.
        let mut targs = Map::new();
        targs.insert("id".into(), Value::from(1u64));
        let report = TaskTool.execute(&targs, &ctx).await.unwrap();
        assert!(report.contains("bg-hello"));
        let list = TaskTool.execute(&Map::new(), &ctx).await.unwrap();
        assert!(list.contains("#1"));
    }

    #[test]
    fn allow_patterns_gate_the_prompt() {
        let ctx = ToolCtx::new("/tmp");
        ctx.add_allow_pattern("git status *");
        ctx.add_allow_pattern("cargo *");
        assert!(ctx.bash_allowed("git status --short"));
        assert!(ctx.bash_allowed("git status")); // "x *" covers the bare form
        assert!(ctx.bash_allowed("cargo build --release"));
        assert!(!ctx.bash_allowed("git push origin main"));
        // Chained commands: every segment must be allowed.
        assert!(ctx.bash_allowed("cargo build && cargo test"));
        assert!(!ctx.bash_allowed("git status && curl evil.example"));
        // Empty allow list allows nothing (and an empty command is nothing).
        let bare = ToolCtx::new("/tmp");
        assert!(!bare.bash_allowed("ls"));
        assert!(!ctx.bash_allowed("  "));
        // Runtime growth (the "always allow" choice).
        ctx.add_allow_pattern("git push *");
        assert!(ctx.bash_allowed("git push origin main"));
        assert_eq!(ctx.user_allow_patterns().len(), 3);
    }

    #[test]
    fn allow_pattern_shapes() {
        assert_eq!(allow_pattern_for("git push origin main"), "git push *");
        assert_eq!(allow_pattern_for("cargo test"), "cargo test *");
        assert_eq!(allow_pattern_for("ls -la"), "ls *"); // flag ≠ subcommand
        assert_eq!(allow_pattern_for("python script.py"), "python *"); // path-ish arg
        assert_eq!(allow_pattern_for("make"), "make *");
        assert_eq!(allow_pattern_for("FOO=1 make"), "FOO=1 make *"); // env prefix keeps the program
    }

    #[tokio::test]
    async fn approval_skipped_for_allowed_commands_only() {
        // Approval on, with an interactive channel that would DENY anything
        // that asks: allowed commands must run without asking at all.
        let ctx = ToolCtx::new("/tmp").with_approval(true);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel::<AskRequest>();
        let ctx = ctx.with_interaction(ask_tx);
        ctx.add_allow_pattern("echo *");
        tokio::spawn(async move {
            while let Some(req) = ask_rx.recv().await {
                let _ = req.reply.send("deny".into());
            }
        });
        assert!(ctx.check_bash_approval("echo hello").await.is_ok());
        assert!(ctx.check_bash_approval("rm file").await.is_err()); // asked, denied
    }

    /// A ctx with the given permission rule lists compiled in.
    fn ctx_with_rules(cwd: &Path, allow: &[&str], ask: &[&str], deny: &[&str]) -> ToolCtx {
        let ctx = ToolCtx::new(cwd);
        let perms = crate::config::Permissions {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert!(ctx.set_permissions(&perms).is_empty());
        ctx
    }

    #[tokio::test]
    async fn deny_rule_blocks_edit_write_and_read() {
        let dir = std::env::temp_dir().join(format!("rift-rules-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("secrets")).unwrap();
        std::fs::write(dir.join("secrets/key.pem"), "PRIVATE").unwrap();
        std::fs::write(dir.join("root-key.pem"), "PRIVATE").unwrap();
        let ctx = ctx_with_rules(
            &dir,
            &[],
            &[],
            &["Edit(secrets/**)", "Read(secrets/**)", "Read(root-key.pem)"],
        );
        // Deny holds even with approval mode off and no interactive user.
        let mut args = Map::new();
        args.insert("path".into(), Value::String("secrets/key.pem".into()));
        args.insert("content".into(), Value::String("overwritten".into()));
        let err = WriteTool.execute(&args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("permission rule"), "got: {err}");
        let err = ReadTool.execute(&args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("permission rule"), "got: {err}");
        // The file is untouched and unread.
        assert_eq!(std::fs::read_to_string(dir.join("secrets/key.pem")).unwrap(), "PRIVATE");
        // grep skips denied files inside the walk.
        let mut gargs = Map::new();
        gargs.insert("pattern".into(), Value::String("PRIVATE".into()));
        let out = GrepTool.execute(&gargs, &ctx).await.unwrap();
        assert_eq!(out, "no matches");
        // Listing the denied directory is refused (Read(secrets/**) covers
        // its contents), and an individually denied file is hidden from a
        // parent listing (the directory's NAME may still show — knowing
        // secrets/ exists isn't reading it).
        let mut largs = Map::new();
        largs.insert("path".into(), Value::String("secrets".into()));
        let err = LsTool.execute(&largs, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("permission rule"), "got: {err}");
        largs.insert("path".into(), Value::String(".".into()));
        let listing = LsTool.execute(&largs, &ctx).await.unwrap();
        assert!(!listing.contains("root-key.pem"), "denied file leaked into listing: {listing}");
        assert!(listing.contains("secrets"), "listing lost unrelated entries: {listing}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn allow_rule_skips_the_edit_prompt() {
        let dir = std::env::temp_dir().join(format!("rift-allow-rule-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // Approval ON with a channel that denies everything: only the allow
        // rule can let the write through.
        let ctx = ctx_with_rules(&dir, &["Edit(src/**)"], &[], &[]).with_approval(true);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel::<AskRequest>();
        let ctx = ctx.with_interaction(ask_tx);
        tokio::spawn(async move {
            while let Some(req) = ask_rx.recv().await {
                let _ = req.reply.send("deny".into());
            }
        });
        let mut args = Map::new();
        args.insert("path".into(), Value::String("src/lib.rs".into()));
        args.insert("content".into(), Value::String("pub fn x() {}".into()));
        WriteTool.execute(&args, &ctx).await.unwrap();
        args.insert("path".into(), Value::String("elsewhere.rs".into()));
        assert!(WriteTool.execute(&args, &ctx).await.is_err()); // asked, denied
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ask_rule_forces_prompt_even_in_yolo() {
        // Approval OFF: an ask rule must still prompt (and deny headless).
        let ctx = ctx_with_rules(Path::new("/tmp"), &[], &["Bash(git push *)"], &[]).with_approval(false);
        // Headless (no ask channel): the ask rule denies rather than runs.
        let err = ctx.check_bash_approval("git push origin main").await.unwrap_err();
        assert!(err.to_string().contains("interactive approval"), "got: {err}");
        // Ask rules see the same expansion folding as deny — `${IFS}` must
        // not smuggle the command past the gate.
        assert!(ctx.check_bash_approval("git${IFS}push origin main").await.is_err());
        assert!(ctx.check_bash_approval("true && git push --force").await.is_err());
        // Non-matching commands sail through in yolo, as before.
        assert!(ctx.check_bash_approval("git status").await.is_ok());
        // With a user attached, the prompt fires and their answer rules.
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel::<AskRequest>();
        let ctx = ctx.with_interaction(ask_tx);
        tokio::spawn(async move {
            while let Some(req) = ask_rx.recv().await {
                // Forced prompts offer no session/persistent grants.
                assert_eq!(req.choices, vec!["allow once".to_string(), "deny".to_string()]);
                let _ = req.reply.send("allow once".into());
            }
        });
        assert!(ctx.check_bash_approval("git push origin main").await.is_ok());
    }

    #[tokio::test]
    async fn edit_review_applies_reviewer_content() {
        let dir = std::env::temp_dir().join(format!("rift-review-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        let ctx = ToolCtx::new(&dir).with_approval(true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EditReviewRequest>();
        ctx.set_edit_review(Some(tx));
        // Reviewer accepts a hunk-filtered subset of the proposal.
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                assert_eq!(req.old, "line1\nline2\n");
                assert_eq!(req.new, "line1\nCHANGED\n");
                let _ = req.reply.send(EditReviewReply::Apply("line1\nREVIEWED\n".into()));
            }
        });
        let mut args = Map::new();
        args.insert("path".into(), Value::String("a.txt".into()));
        args.insert("old_string".into(), Value::String("line2".into()));
        args.insert("new_string".into(), Value::String("CHANGED".into()));
        let result = EditTool.execute(&args, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "line1\nREVIEWED\n");
        // The model is told the proposal was modified in review.
        assert!(result.contains("accepted only part"), "got: {result}");
        // Undo restores the pre-review content (journal saw the true prior).
        let restored = ctx.undo_last_turn().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "line1\nline2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_review_deny_bails_without_writing() {
        let dir = std::env::temp_dir().join(format!("rift-review-deny-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "keep\n").unwrap();
        let ctx = ToolCtx::new(&dir).with_approval(true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EditReviewRequest>();
        ctx.set_edit_review(Some(tx));
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(EditReviewReply::Deny);
            }
        });
        let mut args = Map::new();
        args.insert("path".into(), Value::String("a.txt".into()));
        args.insert("content".into(), Value::String("clobbered\n".into()));
        let err = WriteTool.execute(&args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("DENIED"), "got: {err}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "keep\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_matches_lf_old_string_against_crlf_file() {
        // A model that copies lines out of the read tool gets LF-only text
        // even when the file on disk is CRLF (Windows). The edit must still
        // match, and the write must keep the file's CRLF endings intact.
        let dir = std::env::temp_dir().join(format!("rift-crlf-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w.py"), "def f():\r\n    return 1\r\n").unwrap();
        let ctx = ToolCtx::new(&dir);
        let mut args = Map::new();
        args.insert("path".into(), Value::String("w.py".into()));
        // old/new arrive LF-only, spanning a line break, as the model would send.
        args.insert("old_string".into(), Value::String("def f():\n    return 1".into()));
        args.insert("new_string".into(), Value::String("def f():\n    return 2".into()));
        let result = EditTool.execute(&args, &ctx).await.unwrap();
        assert!(result.contains("1 replacement"), "got: {result}");
        assert_eq!(std::fs::read_to_string(dir.join("w.py")).unwrap(), "def f():\r\n    return 2\r\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_matches_crlf_old_string_against_lf_file() {
        // The mirror case: a CRLF old_string against an LF file demotes to LF.
        let dir = std::env::temp_dir().join(format!("rift-lf-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w.py"), "a\nb\n").unwrap();
        let ctx = ToolCtx::new(&dir);
        let mut args = Map::new();
        args.insert("path".into(), Value::String("w.py".into()));
        args.insert("old_string".into(), Value::String("a\r\nb".into()));
        args.insert("new_string".into(), Value::String("a\r\nc".into()));
        EditTool.execute(&args, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("w.py")).unwrap(), "a\nc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn bash_deny_matches_chained_segments() {
        let ctx = ToolCtx::with_extra_deny("/tmp", &["docker push *".to_string()]);
        // A harmless prefix must not smuggle a denied command through.
        assert!(ctx.bash_denied("true && sudo whoami"));
        assert!(ctx.bash_denied("echo hi; docker push evil"));
        assert!(ctx.bash_denied("ls | sudo tee /etc/passwd"));
        assert!(ctx.bash_denied("(sudo rm x)"));
        assert!(ctx.bash_denied("echo `sudo id`"));
        assert!(!ctx.bash_denied("echo safe && ls -la"));
    }

    #[test]
    fn bash_deny_catches_variable_expansion_tricks() {
        // Shell expansions that fold to whitespace (or nothing) must not let a
        // denied command slip past the literal match. Every variant below runs
        // as a denied command once the shell expands it.
        let ctx = ToolCtx::with_extra_deny("/tmp", &[]);
        assert!(ctx.bash_denied("sudo${IFS}whoami"));
        assert!(ctx.bash_denied("sudo$@ whoami"));
        assert!(ctx.bash_denied("sudo${undefined}whoami"));
        assert!(ctx.bash_denied("rm${IFS}-rf${IFS}/"));
        assert!(ctx.bash_denied("dd${IFS}if=/dev/zero${IFS}of=/dev/sda"));
        assert!(ctx.bash_denied("sudo$IFS whoami"));
        // A `$` in an otherwise-harmless command stays allowed (no over-block).
        assert!(!ctx.bash_denied("echo $HOME"));
        assert!(!ctx.bash_denied("echo ${PATH}"));
        assert!(!ctx.bash_denied("git commit -m \"fix $bug\""));
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

    // Unix-only: encodes POSIX shell behavior (`;` sequencing + `sleep`). The
    // bash tool runs through cmd.exe on Windows, where this command would echo
    // a literal string and exit instead of timing out.
    #[tokio::test]
    async fn repo_map_ranks_imported_modules_first() {
        let dir = std::env::temp_dir().join(format!("rift-rmap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // core.py is imported by both others but has the OLDEST mtime —
        // recency-only ranking would put it last; centrality puts it first.
        std::fs::write(dir.join("core.py"), "def core_fn():\n    pass\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("a.py"), "import core\ndef a_fn():\n    pass\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("b.py"), "from core import core_fn\ndef b_fn():\n    pass\n").unwrap();

        let ctx = ToolCtx::new(&dir);
        let out = RepoMapTool.execute(&Map::new(), &ctx).await.unwrap();
        let core_pos = out.find("core.py").expect("core.py in map");
        let a_pos = out.find("a.py").expect("a.py in map");
        let b_pos = out.find("b.py").expect("b.py in map");
        assert!(core_pos < a_pos && core_pos < b_pos,
            "imported module must rank first despite oldest mtime:\n{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_hydrates_large_source_files_unless_bounded() {
        let dir = std::env::temp_dir().join(format!("rift-hydrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut src = String::from("def target_fn():\n    return 1\n");
        for i in 0..600 {
            src.push_str(&format!("# filler line {i}\n"));
        }
        std::fs::write(dir.join("big.py"), &src).unwrap();
        let ctx = ToolCtx::new(&dir);

        // Unbounded read of a big source file → outline + hint, not content.
        let mut args = Map::new();
        args.insert("path".into(), Value::String("big.py".into()));
        let out = ReadTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("showing its outline"), "expected hydration: {out}");
        assert!(out.contains("def target_fn()"));
        assert!(!out.contains("filler line 42"), "raw content leaked: {out}");

        // Explicit bounds always return the real lines.
        args.insert("offset".into(), Value::from(3));
        args.insert("limit".into(), Value::from(2));
        let out = ReadTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("filler line 0"), "bounded read must be verbatim: {out}");

        // Small files never hydrate.
        std::fs::write(dir.join("small.py"), "def tiny():\n    return 2\n").unwrap();
        let mut args = Map::new();
        args.insert("path".into(), Value::String("small.py".into()));
        let out = ReadTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("return 2"), "small file must read verbatim: {out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
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

    // Unix-only: relies on `tail -f /dev/null` blocking under sh (no cmd.exe
    // equivalent), so it's gated to the POSIX shell path of the bash tool.
    #[cfg(unix)]
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

    // Cross-platform: `echo <word>` and `exit <n>` behave the same under sh and
    // cmd.exe, so these run on every OS and give the Windows bash path — which
    // the #[cfg(unix)] timeout tests above can't reach — real coverage of
    // spawn + stdout capture + exit-code reporting.
    #[tokio::test]
    async fn bash_runs_command_and_captures_output() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo rift_xplat_marker".into()));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("rift_xplat_marker"), "echo output missing: {out}");
    }

    #[tokio::test]
    async fn bash_reports_nonzero_exit() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert("command".into(), Value::String("exit 3".into()));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains("exit code: 3"), "exit-code note missing: {out}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bash_preserves_embedded_double_quotes() {
        // Regression: a `python -c "import x"` argument used to reach the
        // shell as a broken `"import` because cmd.exe mangled Rust's escaped
        // quotes. A double-quoted argument must now round-trip verbatim
        // (cmd's `echo` prints its argument literally, quotes included).
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert("command".into(), Value::String(r#"echo "a b; c""#.into()));
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(out.contains(r#""a b; c""#), "quotes/semicolon corrupted: {out}");
        assert!(!out.contains('\\'), "backslash leaked from quote mangling: {out}");
    }

    // Windows analogue of the (unix-gated) timeout test: a hanging command must
    // be killed near the deadline via `taskkill /T`, with partial output kept.
    // `ping -n 10` runs ~9s; `&` chains commands under cmd.exe.
    #[cfg(windows)]
    #[tokio::test]
    async fn bash_timeout_kills_process_tree_on_windows() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo started & ping -n 10 127.0.0.1".into()));
        args.insert("timeout_secs".into(), Value::from(1));
        let start = std::time::Instant::now();
        let out = BashTool.execute(&args, &ctx).await.unwrap();
        assert!(start.elapsed().as_secs() < 8, "should be killed near the 1s deadline, took {:?}", start.elapsed());
        assert!(out.contains("started"), "partial output must survive the kill: {out}");
        assert!(out.contains("killed after 1s"), "timeout note missing: {out}");
    }

    #[test]
    fn closest_line_points_at_near_match() {
        let text = "fn alpha() {}\nconst products = [ { id: 1, name: 'Classic' } ];\nfn omega() {}";
        let (no, line) = closest_line(text, "const products = [ { id: 1, name: \"Classic\" } ]").unwrap();
        assert_eq!(no, 2);
        assert!(line.contains("products"));
        assert!(closest_line(text, "zzz qqq totally unrelated www").is_none());
    }

    #[tokio::test]
    async fn enoent_suggests_similar_paths() {
        let dir = std::env::temp_dir().join(format!("rift-enoent-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/App.jsx"), "x").unwrap();
        let hint = enoent_hint(&dir, &dir.join("App.jsx")).await;
        // Normalize separators: enoent_hint renders paths via Display, which
        // uses `\` on Windows, so compare against forward slashes either way.
        assert!(
            hint.replace('\\', "/").contains("src/App.jsx"),
            "should suggest the real path: {hint}"
        );
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

    // Overwriting a non-UTF-8 file (binary, UTF-16) must stay undoable —
    // the prior snapshot is bytes, not text, so undo restores instead of
    // deleting.
    #[tokio::test]
    async fn undo_restores_non_utf8_files() {
        let dir = std::env::temp_dir().join(format!("rift-undo-bin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("data.bin");
        let original: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x41, 0x80]; // not valid UTF-8
        std::fs::write(&binary, &original).unwrap();

        let ctx = ToolCtx::new(&dir);
        ctx.begin_turn();
        let mut args = Map::new();
        args.insert("path".into(), Value::from(binary.to_str().unwrap()));
        args.insert("content".into(), Value::from("plain text now"));
        WriteTool.execute(&args, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "plain text now");

        let restored = ctx.undo_last_turn().unwrap();
        assert_eq!(restored, vec![binary.clone()]);
        assert!(binary.exists(), "undo must restore the file, not delete it");
        assert_eq!(std::fs::read(&binary).unwrap(), original);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
