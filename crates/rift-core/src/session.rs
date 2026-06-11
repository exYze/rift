//! Session persistence: one JSON file per session under
//! `~/.local/share/rift/sessions/`, written after every completed turn
//! so a crash never loses more than the in-flight turn.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
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
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local/share/rift/sessions"))
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
