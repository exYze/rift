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
        Ok(saved)
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
            messages: messages.to_vec(),
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
