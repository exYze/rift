//! Project memory: durable facts saved across sessions in `.rift/memory.md`,
//! loaded into the system prompt alongside RIFT.md. Grown by the user
//! (`/remember`) and by the model (the `remember` tool) — the compounding
//! notebook a repo accumulates: build quirks, conventions, decisions.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Keep the memory readable and the context cost bounded: past this size,
/// appends refuse and ask for a manual prune (the file is plain markdown).
const MEMORY_MAX_BYTES: u64 = 16 * 1024;
/// How much memory content the system prompt loads (chars).
pub const MEMORY_PROMPT_CAP: usize = 4000;

pub fn memory_path(cwd: &Path) -> PathBuf {
    cwd.join(".rift/memory.md")
}

/// Append one fact as a dated bullet. Exact duplicates are dropped.
pub fn append_memory(cwd: &Path, fact: &str) -> Result<PathBuf> {
    let fact = fact.trim();
    if fact.is_empty() {
        bail!("nothing to remember — pass the fact to save");
    }
    if fact.lines().count() > 6 {
        bail!("memory entries are short facts (a few lines), not documents — distill it first");
    }
    let path = memory_path(cwd);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!("- {}", fact.replace('\n', "\n  "));
    if existing.lines().any(|l| l == line.lines().next().unwrap_or_default()) {
        bail!("that fact is already in {}", path.display());
    }
    if existing.len() as u64 > MEMORY_MAX_BYTES {
        bail!(
            "{} is over {}KB — prune it (it's plain markdown) before adding more",
            path.display(),
            MEMORY_MAX_BYTES / 1024
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut content = existing;
    if content.is_empty() {
        content.push_str("# Project memory\n\nFacts saved across rift sessions (via /remember and the model's remember tool).\n\n");
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The memory content for the system prompt (None = no memory yet).
pub fn load_memory(cwd: &Path) -> Option<String> {
    let text = std::fs::read_to_string(memory_path(cwd)).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut shown: String = trimmed.chars().take(MEMORY_PROMPT_CAP).collect();
    if shown.len() < trimmed.len() {
        shown.push_str("\n[memory truncated — read .rift/memory.md for the rest]");
    }
    Some(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_load_dedup_and_caps() {
        let dir = std::env::temp_dir().join(format!("rift-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_memory(&dir).is_none());

        append_memory(&dir, "tests need the vLLM server at 192.168.1.237 running").unwrap();
        append_memory(&dir, "cargo clippy must pass with -D warnings before push").unwrap();
        let loaded = load_memory(&dir).unwrap();
        assert!(loaded.contains("# Project memory"));
        assert!(loaded.contains("- tests need the vLLM server"));
        assert!(loaded.contains("- cargo clippy must pass"));

        // Exact duplicates refuse; empty and essay-length entries refuse.
        assert!(append_memory(&dir, "cargo clippy must pass with -D warnings before push").is_err());
        assert!(append_memory(&dir, "   ").is_err());
        let essay = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        assert!(append_memory(&dir, &essay).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
