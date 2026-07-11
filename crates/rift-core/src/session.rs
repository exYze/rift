//! Session persistence: one JSON file per session under
//! `~/.local/share/rift/sessions/`, written after every completed turn
//! so a crash never loses more than the in-flight turn.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rift_provider::{Message, Role};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedSession {
    pub model: String,
    pub saved_at: u64,
    pub cwd: String,
    pub messages: Vec<Message>,
}

pub struct SessionStore {
    path: PathBuf,
}

/// Backfill tool-call ids in histories saved before ids were synthesized
/// (Ollama-era sessions). Every assistant tool call gets an id, and the
/// role=tool results that follow are re-paired with those ids (matched by
/// tool name in call order). Without this, resuming an old session under an
/// OpenAI-compatible provider sends unanswerable tool_call ids — strict
/// servers reject the whole request.
pub fn normalize_tool_call_ids(messages: &mut [Message]) {
    for idx in 0..messages.len() {
        if messages[idx].role != Role::Assistant || messages[idx].tool_calls.is_empty() {
            continue;
        }
        // (name, id) of this message's calls, in order, unanswered so far.
        let mut pending: Vec<(String, String)> = Vec::new();
        for (i, call) in messages[idx].tool_calls.iter_mut().enumerate() {
            let id = call.id.get_or_insert_with(|| format!("call_{idx}_{i}")).clone();
            pending.push((call.function.name.clone(), id));
        }
        // The results for these calls are the role=tool messages directly
        // after the assistant message.
        let mut j = idx + 1;
        while j < messages.len() && messages[j].role == Role::Tool && !pending.is_empty() {
            match &messages[j].tool_call_id {
                Some(existing) => pending.retain(|(_, id)| id != existing),
                None => {
                    // Prefer the first unanswered call with the same tool
                    // name; fall back to plain call order.
                    let pos = messages[j]
                        .tool_name
                        .as_ref()
                        .and_then(|n| pending.iter().position(|(name, _)| name == n))
                        .unwrap_or(0);
                    let (_, id) = pending.remove(pos);
                    messages[j].tool_call_id = Some(id);
                }
            }
            j += 1;
        }
    }
}

/// Prefix of the hidden context note `resume_brief` builds. Frontends already
/// hide user messages starting with "[system]" from the transcript; the full
/// prefix additionally lets `load`/`resume` strip briefs left by earlier
/// restarts before a fresh one is appended, so they never pile up.
pub const RESUME_BRIEF_PREFIX: &str = "[system] session resumed —";

/// At most this many file paths are listed per section of the brief; the
/// rest collapse to "(+N more)".
const BRIEF_MAX_LISTED: usize = 20;

/// Build the resume brief: a hidden `[system]` user message appended to a
/// restored history so the model keeps the project understanding it already
/// earned instead of re-exploring the folder from scratch. Deterministic —
/// harvested from the saved tool calls (no LLM call, works offline): which
/// files were read/outlined, which were written/edited, how much other tool
/// traffic ran, and whether compaction elided older outputs. None for a
/// chat-only history (nothing was explored, nothing to protect).
pub fn resume_brief(messages: &[Message]) -> Option<String> {
    fn push_unique(list: &mut Vec<String>, path: &str) {
        if !list.iter().any(|p| p == path) {
            list.push(path.to_string());
        }
    }
    fn file_list(items: &[String]) -> String {
        let shown = items[..items.len().min(BRIEF_MAX_LISTED)].join(", ");
        match items.len().saturating_sub(BRIEF_MAX_LISTED) {
            0 => shown,
            more => format!("{shown} (+{more} more)"),
        }
    }

    let mut read: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let (mut searches, mut commands) = (0usize, 0usize);
    for m in messages.iter().filter(|m| m.role == Role::Assistant) {
        for tc in &m.tool_calls {
            let path = tc.function.arguments.get("path").and_then(|v| v.as_str());
            match tc.function.name.as_str() {
                "read" | "outline" => {
                    if let Some(p) = path {
                        push_unique(&mut read, p);
                    }
                }
                "write" | "edit" => {
                    if let Some(p) = path {
                        push_unique(&mut changed, p);
                    }
                }
                "ls" | "grep" | "glob" | "repo_map" => searches += 1,
                "bash" => commands += 1,
                _ => {}
            }
        }
    }
    if read.is_empty() && changed.is_empty() && searches == 0 && commands == 0 {
        return None;
    }

    let mut brief = format!(
        "{RESUME_BRIEF_PREFIX} the conversation above is your own earlier work in this \
         project, restored from disk. That understanding is still valid: do NOT \
         re-explore the project (no fresh ls/glob/grep/repo_map sweeps) to reorient yourself."
    );
    if !read.is_empty() {
        brief.push_str(&format!("\nFiles you already read or outlined: {}", file_list(&read)));
    }
    if !changed.is_empty() {
        brief.push_str(&format!("\nFiles you created or edited: {}", file_list(&changed)));
    }
    if searches + commands > 0 {
        brief.push_str(&format!(
            "\nYou also ran {searches} directory/search lookups and {commands} shell commands."
        ));
    }
    let elided = messages
        .iter()
        .any(|m| m.role == Role::Tool && m.content.contains(crate::compact::ELIDE_NOTE));
    brief.push_str(if elided {
        "\nSome older tool outputs above were elided to save context — re-read a file only \
         when you need those exact details or it may have changed on disk; otherwise answer \
         from the conversation above."
    } else {
        "\nRe-read a file only if it may have changed on disk; otherwise answer from the \
         conversation above."
    });
    Some(brief)
}

/// Drop resume briefs left by earlier restarts — each resume appends a fresh
/// one, so stale copies would otherwise stack up in long-lived sessions.
fn strip_stale_briefs(messages: &mut Vec<Message>) {
    messages.retain(|m| !(m.role == Role::User && m.content.starts_with(RESUME_BRIEF_PREFIX)));
}

/// Autosaved session files stay under this size — a long-lived session with
/// giant tool outputs otherwise grows a file that slows every save and
/// resume. Live context is untouched (compaction owns that); only what's
/// persisted is trimmed.
const MAX_SESSION_BYTES: usize = 10 * 1024 * 1024;

/// Cap the persisted history: keep the leading system messages, then drop
/// whole turns from the front (cuts land on User messages, so an assistant
/// tool call is never separated from its role=tool results) until the
/// serialized size fits. The final turn is always kept whatever its size.
fn cap_history(messages: &[Message]) -> Vec<Message> {
    let sizes: Vec<usize> =
        messages.iter().map(|m| serde_json::to_vec(m).map_or(0, |v| v.len())).collect();
    let mut total: usize = sizes.iter().sum();
    if total <= MAX_SESSION_BYTES {
        return messages.to_vec();
    }
    let sys_end = messages.iter().position(|m| m.role != Role::System).unwrap_or(messages.len());
    let mut start = sys_end;
    while total > MAX_SESSION_BYTES {
        let Some(next) = (start + 1..messages.len()).find(|&i| messages[i].role == Role::User)
        else {
            break;
        };
        total -= sizes[start..next].iter().sum::<usize>();
        start = next;
    }
    if start == sys_end {
        return messages.to_vec();
    }
    messages[..sys_end].iter().chain(&messages[start..]).cloned().collect()
}

fn sessions_dir() -> Result<PathBuf> {
    let dir = crate::paths::data_dir()
        .context("could not determine a home directory (set HOME or USERPROFILE)")?;
    Ok(dir.join("rift/sessions"))
}

impl SessionStore {
    /// Create a store for a brand-new session file.
    pub fn create() -> Result<Self> {
        let dir = sessions_dir()?;
        std::fs::create_dir_all(&dir)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        // Reserve the name atomically (create_new) — a bare `exists()` check
        // races with other rift processes started in the same second, and two
        // stores on one path silently clobber each other's autosaves.
        let mut n = 0;
        loop {
            let path = if n == 0 {
                dir.join(format!("{stamp}.json"))
            } else {
                dir.join(format!("{stamp}-{n}.json"))
            };
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot create session file {}", path.display()))
                }
            }
        }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// All saved sessions, newest first.
    pub fn list() -> Result<Vec<PathBuf>> {
        let dir = sessions_dir()?;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut entries: Vec<(SystemTime, PathBuf)> = vec![];
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let meta = entry.metadata()?;
            // Zero bytes = a freshly reserved file that never saved a turn.
            if meta.len() == 0 {
                continue;
            }
            entries.push((meta.modified()?, path));
        }
        entries.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        Ok(entries.into_iter().map(|(_, p)| p).collect())
    }

    /// Most recently saved session file, if any.
    pub fn latest() -> Result<Option<PathBuf>> {
        let dir = sessions_dir()?;
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<(SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.len() == 0 {
                continue;
            }
            let modified = meta.modified()?;
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, path));
            }
        }
        Ok(newest.map(|(_, p)| p))
    }

    pub fn load(path: &std::path::Path) -> Result<SavedSession> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read session {}", path.display()))?;
        let mut saved: SavedSession = serde_json::from_str(&text)?;
        normalize_tool_call_ids(&mut saved.messages);
        strip_stale_briefs(&mut saved.messages);
        Ok(saved)
    }

    /// Resume a session file leniently — the `/restart` and `-c` path.
    /// Session files are reserved EMPTY at startup and only written after
    /// the first turn, so resuming a zero-turn session is normal, not an
    /// error: it continues fresh at the same path. A corrupt file is backed
    /// up (never silently overwritten) and the session starts fresh too —
    /// a chat tool must not brick its own startup on a bad autosave.
    /// Returns (store, recovered messages, optional user-facing notice).
    pub fn resume(path: PathBuf) -> (Self, Vec<Message>, Option<String>) {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                return (
                    Self { path },
                    vec![],
                    Some("session file not found — starting fresh".into()),
                )
            }
        };
        if text.trim().is_empty() {
            return (
                Self { path },
                vec![],
                Some("session has no saved turns yet — starting fresh".into()),
            );
        }
        match serde_json::from_str::<SavedSession>(&text) {
            Ok(mut saved) => {
                normalize_tool_call_ids(&mut saved.messages);
                strip_stale_briefs(&mut saved.messages);
                (Self { path }, saved.messages, None)
            }
            Err(e) => {
                let backup = path.with_extension("json.corrupt");
                let note = match std::fs::rename(&path, &backup) {
                    Ok(()) => format!("original kept at {}", backup.display()),
                    Err(_) => "original left in place; the next turn will overwrite it".into(),
                };
                let warning = format!(
                    "warning: session {} is unreadable ({e}); starting fresh — {note}",
                    path.display()
                );
                (Self { path }, vec![], Some(warning))
            }
        }
    }

    /// Give this session a friendly name: future autosaves go to `<name>.json`
    /// and the old (usually timestamped) file is removed. Returns the new path.
    pub fn save_as(&mut self, name: &str, model: &str, cwd: &str, messages: &[Message]) -> Result<PathBuf> {
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let safe = safe.trim_matches('-').to_string();
        if safe.is_empty() {
            bail!("invalid session name (use letters, digits, - or _)");
        }
        let dir = sessions_dir()?;
        std::fs::create_dir_all(&dir)?;
        let new_path = dir.join(format!("{safe}.json"));
        let old = std::mem::replace(&mut self.path, new_path.clone());
        self.save(model, cwd, messages)?;
        if old != new_path && old.exists() {
            let _ = std::fs::remove_file(&old);
        }
        Ok(new_path)
    }

    /// Atomic save (write temp + rename) so a crash mid-write never corrupts
    /// the session file.
    pub fn save(&self, model: &str, cwd: &str, messages: &[Message]) -> Result<()> {
        let saved = SavedSession {
            model: model.to_string(),
            saved_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            cwd: cwd.to_string(),
            messages: cap_history(messages),
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_provider::{ToolCall, ToolCallFunction};

    fn call(name: &str, id: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.map(str::to_string),
            function: ToolCallFunction { index: None, name: name.into(), arguments: Default::default() },
        }
    }

    fn tool_result(name: &str, id: Option<&str>) -> Message {
        let mut m = Message::tool_result(name, "ok");
        m.tool_call_id = id.map(str::to_string);
        m
    }

    #[test]
    fn cap_history_drops_whole_turns_from_the_front() {
        // 6 turns of ~3MB each (user + assistant tool call + tool result):
        // well past the 10MB cap, so the front turns must go.
        let big = "x".repeat(3 * 1024 * 1024);
        let mut messages = vec![Message::system("sys")];
        for i in 0..6 {
            messages.push(Message::user(format!("turn {i}")));
            let mut a = Message::user(big.clone());
            a.role = Role::Assistant;
            a.tool_calls = vec![call("read", Some(&format!("c{i}")))];
            messages.push(a);
            messages.push(tool_result("read", Some(&format!("c{i}"))));
        }

        let capped = cap_history(&messages);
        let size = serde_json::to_vec(&capped).unwrap().len();
        assert!(size <= MAX_SESSION_BYTES, "still {size} bytes");
        // System prompt survives; the cut lands on a turn boundary, so no
        // orphaned assistant call or role=tool result leads the history.
        assert_eq!(capped[0].role, Role::System);
        assert_eq!(capped[1].role, Role::User);
        // The most recent turn is always kept.
        assert!(capped.iter().any(|m| m.content == "turn 5"));
        assert!(!capped.iter().any(|m| m.content == "turn 0"));

        // Small histories come back untouched.
        let small = vec![Message::system("sys"), Message::user("hi")];
        assert_eq!(cap_history(&small).len(), 2);
    }

    #[test]
    fn normalize_backfills_ids_and_pairs_results() {
        let mut assistant = Message::user(""); // shape below overrides role
        assistant.role = Role::Assistant;
        assistant.tool_calls = vec![call("read", None), call("grep", None)];
        let mut messages = vec![
            Message::system("sys"),
            Message::user("do it"),
            assistant,
            tool_result("read", None),
            tool_result("grep", None),
        ];
        normalize_tool_call_ids(&mut messages);

        let ids: Vec<String> =
            messages[2].tool_calls.iter().map(|c| c.id.clone().unwrap()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        // Each result answers the call with the matching name.
        assert_eq!(messages[3].tool_call_id.as_deref(), Some(ids[0].as_str()));
        assert_eq!(messages[4].tool_call_id.as_deref(), Some(ids[1].as_str()));
    }

    fn call_with_path(name: &str, path: &str) -> ToolCall {
        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::Value::String(path.into()));
        ToolCall {
            id: None,
            function: ToolCallFunction { index: None, name: name.into(), arguments: args },
        }
    }

    fn assistant_calling(calls: Vec<ToolCall>) -> Message {
        let mut m = Message::user("");
        m.role = Role::Assistant;
        m.tool_calls = calls;
        m
    }

    #[test]
    fn resume_brief_indexes_prior_exploration() {
        let messages = vec![
            Message::system("sys"),
            Message::user("improve the proposal"),
            assistant_calling(vec![
                call_with_path("read", "proposal.md"),
                call_with_path("outline", "deck.md"),
                call_with_path("read", "proposal.md"), // duplicate — listed once
                call_with_path("grep", "."),
            ]),
            Message::tool_result("read", "contents"),
            assistant_calling(vec![call_with_path("edit", "proposal.md")]),
        ];
        let brief = resume_brief(&messages).unwrap();
        assert!(brief.starts_with(RESUME_BRIEF_PREFIX));
        // Once in the read list, once in the edited list — the duplicate
        // read collapsed.
        assert_eq!(brief.matches("proposal.md").count(), 2);
        assert!(brief.contains("deck.md"));
        assert!(brief.contains("1 directory/search lookups"));
        // Nothing was pruned, so the elision warning stays out.
        assert!(!brief.contains("Some older tool outputs"));

        // Chat-only history: nothing was explored, nothing to protect.
        assert!(resume_brief(&[Message::system("s"), Message::user("hi")]).is_none());
    }

    #[test]
    fn resume_brief_caps_file_lists_and_flags_elisions() {
        let paths: Vec<String> = (0..25).map(|i| format!("src/file{i}.rs")).collect();
        let mut pruned = Message::tool_result(
            "read",
            format!("head\n{}5000 bytes to save context; re-run the tool if you need this again]", crate::compact::ELIDE_NOTE),
        );
        pruned.tool_call_id = Some("c0".into());
        let messages = vec![
            Message::user("go"),
            assistant_calling(paths.iter().map(|p| call_with_path("read", p)).collect()),
            pruned,
        ];
        let brief = resume_brief(&messages).unwrap();
        assert!(brief.contains("file19.rs"));
        assert!(!brief.contains("file20.rs"), "list must cap at {BRIEF_MAX_LISTED}");
        assert!(brief.contains("(+5 more)"));
        assert!(brief.contains("Some older tool outputs"));
    }

    #[test]
    fn resume_and_load_strip_stale_briefs() {
        let dir = std::env::temp_dir().join(format!("rift-brief-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stale.json");
        let store = SessionStore::at(path.clone());
        let messages = vec![
            Message::user("hi"),
            Message::user(format!("{RESUME_BRIEF_PREFIX} note from an earlier restart")),
        ];
        store.save("m", "/tmp", &messages).unwrap();

        let (_, resumed, notice) = SessionStore::resume(path.clone());
        assert!(notice.is_none());
        assert_eq!(resumed.len(), 1, "stale brief must be dropped on resume");
        assert_eq!(resumed[0].content, "hi");

        let loaded = SessionStore::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_is_lenient_on_empty_missing_and_corrupt_files() {
        let dir = std::env::temp_dir().join(format!("rift-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Empty file — the exact /restart-before-first-turn shape (session
        // files are reserved empty at startup). Must resume fresh, not die
        // with "EOF while parsing a value".
        let empty = dir.join("empty.json");
        std::fs::write(&empty, "").unwrap();
        let (store, messages, notice) = SessionStore::resume(empty.clone());
        assert_eq!(store.path(), empty.as_path());
        assert!(messages.is_empty());
        assert!(notice.unwrap().contains("no saved turns"));

        // Missing file — fresh at the same path.
        let missing = dir.join("missing.json");
        let (_, messages, notice) = SessionStore::resume(missing);
        assert!(messages.is_empty());
        assert!(notice.unwrap().contains("not found"));

        // Corrupt file — backed up, never silently overwritten.
        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, "{definitely not json").unwrap();
        let (store, messages, notice) = SessionStore::resume(corrupt.clone());
        assert!(messages.is_empty());
        assert!(notice.unwrap().contains("unreadable"));
        assert_eq!(store.path(), corrupt.as_path());
        let backup = corrupt.with_extension("json.corrupt");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "{definitely not json");
        assert!(!corrupt.exists(), "corrupt original moved aside");

        // A valid file round-trips with no notice.
        let good = dir.join("good.json");
        let store = SessionStore::at(good.clone());
        store.save("m", "/tmp", &[Message::user("hello")]).unwrap();
        let (_, messages, notice) = SessionStore::resume(good);
        assert_eq!(messages.len(), 1);
        assert!(notice.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_keeps_existing_ids_untouched() {
        let mut assistant = Message::user("");
        assistant.role = Role::Assistant;
        assistant.tool_calls = vec![call("read", Some("srv_1")), call("read", None)];
        let mut messages = vec![
            assistant,
            tool_result("read", Some("srv_1")),
            tool_result("read", None),
        ];
        normalize_tool_call_ids(&mut messages);

        assert_eq!(messages[0].tool_calls[0].id.as_deref(), Some("srv_1"));
        let synthesized = messages[0].tool_calls[1].id.clone().unwrap();
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("srv_1"));
        // The id-less result pairs with the remaining (synthesized) call.
        assert_eq!(messages[2].tool_call_id.as_deref(), Some(synthesized.as_str()));
    }
}
