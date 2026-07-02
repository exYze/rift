//! Persistent outline cache for `repo_map` (docs/ROADMAP.md, v0.8).
//!
//! Outlining is a full read + tree-sitter parse per file, and repo_map does
//! it for every listed file on every call — in a big repo that's the whole
//! cost of the tool. Outlines are pure functions of file content, so cache
//! them keyed by (mtime, size): a hit skips both the read and the parse.
//!
//! One JSON cache file per repo root, under the user data dir
//! (`~/.local/share/rift/outline-cache/<fnv-of-root>.json`) — nothing is
//! written into the repo. Everything is best-effort: a missing, corrupt, or
//! stale cache file just means outlines are recomputed; a failed save is
//! ignored. Multiple rift processes may race on the file — last writer wins,
//! which at worst loses some cached entries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Drop the least-recently-verified entries past this many files. Big enough
/// for any repo repo_map will realistically walk (it outlines at most 60
/// files per call, but the cache accumulates across calls).
const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Modification time, nanos since epoch (0 if unavailable).
    mtime: u128,
    size: u64,
    outline: String,
    /// Last time this entry was used or refreshed (unix secs) — the LRU key.
    used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OutlineCache {
    entries: HashMap<PathBuf, Entry>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    file: Option<PathBuf>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// FNV-1a over the root path — stable, dependency-free cache-file naming.
fn root_key(root: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

impl OutlineCache {
    /// Load the cache for a repo root (empty cache on any failure).
    pub fn load(root: &Path) -> Self {
        let file = crate::paths::data_dir().map(|d| d.join("rift/outline-cache").join(format!("{}.json", root_key(root))));
        let mut cache: OutlineCache = file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        cache.file = file;
        cache
    }

    /// A cache that never touches disk (tests, headless one-offs).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The cached outline for `path` if its (mtime, size) still match —
    /// a hit needs no file read at all. Refreshes the LRU stamp.
    pub fn get(&mut self, path: &Path, meta: &std::fs::Metadata) -> Option<String> {
        let entry = self.entries.get_mut(path)?;
        if entry.mtime != mtime_nanos(meta) || entry.size != meta.len() || entry.mtime == 0 {
            return None;
        }
        entry.used = now_secs();
        self.dirty = true; // LRU stamp moved
        Some(entry.outline.clone())
    }

    pub fn insert(&mut self, path: &Path, meta: &std::fs::Metadata, outline: String) {
        self.entries.insert(
            path.to_path_buf(),
            Entry { mtime: mtime_nanos(meta), size: meta.len(), outline, used: now_secs() },
        );
        self.dirty = true;
    }

    /// Persist if anything changed. Best-effort; errors are swallowed by the
    /// caller's design — a lost cache only costs a re-parse.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(file) = self.file.clone() else { return };
        if self.entries.len() > MAX_ENTRIES {
            let mut by_age: Vec<(u64, PathBuf)> =
                self.entries.iter().map(|(p, e)| (e.used, p.clone())).collect();
            by_age.sort();
            for (_, path) in by_age.into_iter().take(self.entries.len() - MAX_ENTRIES) {
                self.entries.remove(&path);
            }
        }
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec(&self) {
            let _ = std::fs::write(&file, json);
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_on_unchanged_miss_on_touch() {
        let dir = std::env::temp_dir().join(format!("rift-ocache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("m.py");
        std::fs::write(&f, "def a():\n    pass\n").unwrap();
        let meta = std::fs::metadata(&f).unwrap();

        let mut cache = OutlineCache::in_memory();
        assert!(cache.get(&f, &meta).is_none(), "cold cache misses");
        cache.insert(&f, &meta, "OUTLINE-1".into());
        assert_eq!(cache.get(&f, &meta).as_deref(), Some("OUTLINE-1"));

        // Change the content (different size) — the entry must invalidate.
        std::fs::write(&f, "def a():\n    pass\n\ndef b():\n    pass\n").unwrap();
        let meta2 = std::fs::metadata(&f).unwrap();
        assert!(cache.get(&f, &meta2).is_none(), "changed file misses");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrips_through_disk() {
        // Point the data dir at a temp HOME so the test never touches the
        // real user cache.
        let dir = std::env::temp_dir().join(format!("rift-ocache-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("m.py");
        std::fs::write(&f, "def a():\n    pass\n").unwrap();
        let meta = std::fs::metadata(&f).unwrap();

        let mut cache = OutlineCache::load(&dir);
        // Redirect the save target into the temp dir regardless of HOME.
        cache.file = Some(dir.join("cache.json"));
        cache.insert(&f, &meta, "SAVED".into());
        cache.save();

        let mut reloaded: OutlineCache =
            serde_json::from_str(&std::fs::read_to_string(dir.join("cache.json")).unwrap()).unwrap();
        assert_eq!(reloaded.get(&f, &meta).as_deref(), Some("SAVED"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
