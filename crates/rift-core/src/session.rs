//! Session persistence: one JSON file per session under
//! `~/.local/share/rift/sessions/`, written after every completed turn
//! so a crash never loses more than the in-flight turn.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rift_ollama::Message;
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
        // Avoid collisions if two sessions start in the same second.
        let mut path = dir.join(format!("{stamp}.json"));
        let mut n = 1;
        while path.exists() {
            path = dir.join(format!("{stamp}-{n}.json"));
            n += 1;
        }
        Ok(Self { path })
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
            entries.push((entry.metadata()?.modified()?, path));
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
            let modified = entry.metadata()?.modified()?;
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, path));
            }
        }
        Ok(newest.map(|(_, p)| p))
    }

    pub fn load(path: &std::path::Path) -> Result<SavedSession> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read session {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
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
