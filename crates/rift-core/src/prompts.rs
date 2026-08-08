//! Per-model-family system prompts — each model family is a compiler
//! target (docs/ROADMAP.md, v0.8.5 "Model targets").
//!
//! Targets are markdown files with a minimal frontmatter (the SKILL.md
//! idiom), embedded at compile time so the one-binary promise holds:
//!
//! ```text
//! ---
//! family: qwen
//! match: qwen, qwen3
//! ---
//! You are Rift, ... {cwd} ... {shell}
//! ```
//!
//! `match:` lists lowercase substrings tried against the model name; the
//! first target that matches wins, otherwise `default`. A bare `*` entry
//! matches every model. `{cwd}` and `{shell}` are the only placeholders.
//!
//! Experiments run from `~/.config/rift/prompts/*.md`, matched before the
//! embedded targets. User-level only, deliberately: RIFT.md from a cloned
//! repo is additive and capped, but a project-level prompt *replacement*
//! would let any cloned repo silently rewrite agent behavior. Prompts are
//! code — a family file merges only if it beats the incumbent on the bench
//! matrix (see crates/rift-core/prompts/README.md).
//!
//! `custom.md` in that directory is special only by convention: it is the
//! file behind "your own system prompt" — written by the TUI's
//! `/system save` and the editor extensions' settings UI — a `match: *`
//! target that replaces every embedded family prompt while a user's
//! concrete family override files still win (wildcard targets sort after
//! concrete ones within the overrides).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One prompt target: a family name, the model-name substrings that select
/// it, and the prompt template.
#[derive(Debug, Clone)]
pub struct PromptTarget {
    pub family: String,
    /// Lowercase substrings matched against the (lowercased) model name.
    /// Empty = never matched by model name; selected only as the
    /// `default` fallback.
    pub matches: Vec<String>,
    pub template: String,
}

/// Compile-time embedded targets, (file stem, contents). `default.md` must
/// always be present — it is the fallback for unmatched models.
const EMBEDDED: &[(&str, &str)] = &[
    ("default", include_str!("../prompts/default.md")),
    // Provisional (first bench matrix, 2026-07: 8/10 gemma4:26b failures were
    // chat-only answers that ignored two nudges + a temp-0 retry; 2 more
    // explored without editing). Validate against default.md's 40/50
    // baseline on the next matrix run — see prompts/README.md.
    ("gemma", include_str!("../prompts/gemma.md")),
    // The remaining families are provisional too — derived from each
    // family's documented failure modes, awaiting their first matrix run
    // through the evolution gate (scripts/prompt_gate.py). qwen: trim
    // narration and double-verification; deepseek: cap reasoning spill and
    // exploration; glm: chat-only tendency (gemma-style CRITICAL framing) +
    // reply-language pin; mistral: exact-match edit retries without
    // whole-file rewrites.
    ("qwen", include_str!("../prompts/qwen.md")),
    ("deepseek", include_str!("../prompts/deepseek.md")),
    ("glm", include_str!("../prompts/glm.md")),
    ("mistral", include_str!("../prompts/mistral.md")),
];

/// Parse a target file: `--- family: x / match: a, b ---` frontmatter,
/// body = template. Missing `family` falls back to the file stem.
fn parse_target(stem: &str, text: &str) -> PromptTarget {
    let mut family = stem.to_string();
    let mut matches = vec![];
    let body = match text.strip_prefix("---").and_then(|rest| rest.find("\n---").map(|end| {
        let (head, body) = rest.split_at(end);
        for line in head.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim() {
                    "family" => family = value.trim().to_string(),
                    "match" => {
                        matches = value
                            .split(',')
                            .map(|m| m.trim().to_lowercase())
                            .filter(|m| !m.is_empty())
                            .collect()
                    }
                    _ => {}
                }
            }
        }
        body.trim_start_matches("\n---").trim_start().to_string()
    })) {
        Some(body) => body,
        None => text.trim_start().to_string(),
    };
    PromptTarget { family, matches, template: body }
}

/// The targets compiled into the binary.
pub fn embedded_targets() -> Vec<PromptTarget> {
    EMBEDDED.iter().map(|(stem, text)| parse_target(stem, text)).collect()
}

/// User-level override targets: `~/.config/rift/prompts/*.md` first, then
/// `prompts/*.md` inside USER plugins (`~/.config/rift/plugins/*/`), sorted
/// so precedence is deterministic. Deliberately nothing project-level —
/// a cloned repo must never be able to replace the system prompt.
///
/// Wildcard (`match: *`) targets sort after the concrete ones so a user's
/// family-specific override still beats their catch-all custom prompt.
pub fn override_targets() -> Vec<PromptTarget> {
    let Some(dir) = crate::paths::config_dir() else { return vec![] };
    let mut out = targets_from_dir(&dir.join("rift/prompts"));
    if let Ok(entries) = std::fs::read_dir(dir.join("rift/plugins")) {
        let mut plugin_dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        plugin_dirs.sort();
        for p in plugin_dirs {
            out.extend(targets_from_dir(&p.join("prompts")));
        }
    }
    out.sort_by_key(|t| t.matches.iter().any(|m| m == "*")); // stable: wildcards last
    out
}

fn targets_from_dir(dir: &Path) -> Vec<PromptTarget> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            let text = std::fs::read_to_string(p).ok()?;
            Some(parse_target(stem, &text))
        })
        .collect()
}

/// Pick the target for a model: first (by list order) whose `match`
/// substrings hit the lowercased model name (`*` hits every model), else
/// the first `default` family, else the first target. Callers put
/// overrides before embedded targets so an override wins both the match
/// and the fallback.
pub fn select<'a>(model: &str, targets: &'a [PromptTarget]) -> Option<&'a PromptTarget> {
    let model = model.to_lowercase();
    targets
        .iter()
        .find(|t| t.matches.iter().any(|m| m == "*" || model.contains(m.as_str())))
        .or_else(|| targets.iter().find(|t| t.family == "default"))
        .or_else(|| targets.first())
}

// ---- the user's own system prompt (custom.md) -------------------------------

/// `~/.config/rift/prompts/custom.md` — the file behind `/system save` and
/// the editor extensions' system-prompt setting.
pub fn custom_prompt_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("rift/prompts/custom.md"))
}

/// The full file contents for a custom prompt body: a `match: *` target
/// that applies to every model.
fn custom_prompt_file_text(body: &str) -> String {
    format!("---\nfamily: custom\nmatch: *\n---\n{}\n", body.trim_end())
}

/// The saved custom prompt's body (frontmatter stripped), if one exists.
pub fn load_custom_prompt() -> Option<String> {
    let text = std::fs::read_to_string(custom_prompt_path()?).ok()?;
    Some(parse_target("custom", &text).template.trim_end().to_string())
}

/// Persist `body` as the user's custom system prompt (all models, all
/// sessions). `{cwd}` and `{shell}` placeholders are honored at render
/// time like any other target. Returns the path written.
pub fn save_custom_prompt(body: &str) -> Result<PathBuf> {
    anyhow::ensure!(!body.trim().is_empty(), "refusing to save an empty system prompt");
    let path = custom_prompt_path().context("no home directory for the custom prompt")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, custom_prompt_file_text(body))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Delete the custom prompt, restoring the built-in targets. Returns
/// whether a file was actually removed.
pub fn delete_custom_prompt() -> Result<bool> {
    let Some(path) = custom_prompt_path() else { return Ok(false) };
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(true)
}

/// Fill the template's placeholders. Plain string replacement — templates
/// may contain any other braces freely (tool examples etc.).
pub fn render(target: &PromptTarget, cwd: &str) -> String {
    target
        .template
        .replace("{cwd}", cwd)
        .replace("{shell}", shell_note())
        .trim_end()
        .to_string()
}

/// One-line note about the host shell so the model emits commands the bash tool
/// can actually run — cmd.exe on Windows has different builtins and quoting than
/// POSIX sh. Compile-time `cfg` is correct here: the binary is platform-specific.
fn shell_note() -> &'static str {
    #[cfg(windows)]
    {
        "You are on Windows: the bash tool runs commands through cmd.exe, so use \
         Windows command syntax (dir, type, del, copy, move, where; chain with &&, \
         not ;). Prefer the read, repo_map, outline and grep tools over shell \
         commands for inspecting files."
    }
    #[cfg(not(windows))]
    {
        "The bash tool runs commands through POSIX sh (sh -c)."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_renders_placeholders() {
        let targets = embedded_targets();
        let t = select("llama3.3:70b", &targets).unwrap();
        assert_eq!(t.family, "default");
        let p = render(t, "/work/repo");
        assert!(p.starts_with("You are Rift, an expert coding agent working in the directory /work/repo."));
        assert!(!p.contains("{cwd}") && !p.contains("{shell}"));
        assert!(p.contains("repo_map"));
        assert!(p.ends_with("already see."));
    }

    #[test]
    fn gemma_models_get_the_gemma_target() {
        let targets = embedded_targets();
        for model in ["gemma4:26b", "codegemma:7b", "hf.co/Jiunsong/SuperGemma4-31b-GGUF:latest"] {
            let t = select(model, &targets).unwrap();
            assert_eq!(t.family, "gemma", "{model}");
        }
        let p = render(select("gemma4:26b", &targets).unwrap(), "/work/repo");
        assert!(p.contains("CRITICAL"));
        assert!(!p.contains("{cwd}") && !p.contains("{shell}"));
        // The rules the trace data says gemma needs are actually present.
        assert!(p.contains("changes NOTHING"));
        assert!(p.contains("never stop after only reading files"));
    }

    #[test]
    fn every_family_target_selects_and_renders() {
        let targets = embedded_targets();
        // Each family catches its models — including hf.co/ paths and
        // finetune names — and renders with no leftover placeholders.
        for (model, family) in [
            ("qwen3:32b", "qwen"),
            ("qwen2.5-coder:14b", "qwen"),
            ("qwq:32b", "qwen"),
            ("deepseek-r1:70b", "deepseek"),
            ("deepseek-coder-v2:16b", "deepseek"),
            ("hf.co/unsloth/DeepSeek-V4-GGUF:latest", "deepseek"),
            ("glm-5:9b", "glm"),
            ("codegeex4:9b", "glm"),
            ("mistral:7b", "mistral"),
            ("devstral:24b", "mistral"),
            ("codestral:22b", "mistral"),
            ("mixtral:8x7b", "mistral"),
            ("gemma4:26b", "gemma"),
            ("llama3.3:70b", "default"),
        ] {
            let t = select(model, &targets).unwrap();
            assert_eq!(t.family, family, "{model}");
            let p = render(t, "/work/repo");
            assert!(!p.contains("{cwd}") && !p.contains("{shell}"), "{family}");
            // Every target keeps the non-negotiables: tool-call channel
            // discipline and the plan-tool nudge.
            assert!(p.contains("NEVER write tool-call JSON"), "{family}");
            assert!(p.contains("plan(set=[...])"), "{family}");
        }
    }

    #[test]
    fn family_matching_first_hit_wins_then_default() {
        let targets = vec![
            parse_target("qwen", "---\nmatch: qwen\n---\nqwen prompt"),
            parse_target("gemma", "---\nfamily: gemma\nmatch: gemma, codegemma\n---\ngemma prompt"),
            parse_target("default", "---\n---\ndefault prompt"),
        ];
        assert_eq!(select("hf.co/unsloth/Qwen3.6-27B:latest", &targets).unwrap().family, "qwen");
        assert_eq!(select("codegemma:7b", &targets).unwrap().family, "gemma");
        assert_eq!(select("deepseek-coder", &targets).unwrap().family, "default");
        assert_eq!(select("", &targets).unwrap().family, "default");
    }

    #[test]
    fn frontmatter_optional_and_stem_names_family() {
        let t = parse_target("mistral", "just a template");
        assert_eq!(t.family, "mistral");
        assert!(t.matches.is_empty());
        assert_eq!(t.template, "just a template");
    }

    #[test]
    fn wildcard_target_matches_every_model_but_concrete_wins_first() {
        let mut targets = vec![
            parse_target("gemma", "---\nmatch: gemma\n---\nfamily prompt"),
            parse_target("custom", "---\nfamily: custom\nmatch: *\n---\ncustom prompt"),
        ];
        targets.extend(embedded_targets());
        // The user's concrete family file still beats their catch-all…
        assert_eq!(select("gemma4:26b", &targets).unwrap().family, "gemma");
        assert_eq!(select("gemma4:26b", &targets).unwrap().template, "family prompt");
        // …while every other model (matched or default-bound) gets custom.
        for model in ["qwen3:32b", "deepseek-r1:70b", "llama3.3:70b", ""] {
            assert_eq!(select(model, &targets).unwrap().family, "custom", "{model}");
        }
    }

    #[test]
    fn custom_prompt_file_roundtrips_and_is_a_wildcard() {
        let text = custom_prompt_file_text("You are my rift.\nBe terse in {cwd}.\n\n");
        let t = parse_target("custom", &text);
        assert_eq!(t.family, "custom");
        assert_eq!(t.matches, vec!["*"]);
        // parse_target keeps the file's trailing newline; render() trims it.
        assert_eq!(t.template.trim_end(), "You are my rift.\nBe terse in {cwd}.");
        // Renders like any target: placeholders fill, nothing left behind.
        let p = render(&t, "/work/repo");
        assert!(p.contains("/work/repo") && !p.contains("{cwd}"));
    }

    #[test]
    fn override_dir_sorts_wildcards_after_concrete_files() {
        // "custom.md" sorts alphabetically before "gemma.md", so without the
        // wildcard re-sort the catch-all would shadow the family override.
        let dir = std::env::temp_dir().join(format!("rift-prompts-wild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("custom.md"), custom_prompt_file_text("catch-all")).unwrap();
        std::fs::write(dir.join("gemma.md"), "---\nmatch: gemma\n---\ngemma override").unwrap();

        let mut targets = targets_from_dir(&dir);
        targets.sort_by_key(|t| t.matches.iter().any(|m| m == "*"));
        targets.extend(embedded_targets());
        assert_eq!(select("gemma4:26b", &targets).unwrap().template, "gemma override");
        assert_eq!(select("qwen3:32b", &targets).unwrap().template.trim_end(), "catch-all");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn override_dir_targets_precede_embedded() {
        let dir = std::env::temp_dir().join(format!("rift-prompts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.md"), "---\n---\noverridden {cwd}").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let mut targets = targets_from_dir(&dir);
        targets.extend(embedded_targets());
        let t = select("llama3:8b", &targets).unwrap();
        assert_eq!(render(t, "/x"), "overridden /x");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
