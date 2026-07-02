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
//! first target that matches wins, otherwise `default`. `{cwd}` and
//! `{shell}` are the only placeholders.
//!
//! Experiments run from `~/.config/rift/prompts/*.md`, matched before the
//! embedded targets. User-level only, deliberately: RIFT.md from a cloned
//! repo is additive and capped, but a project-level prompt *replacement*
//! would let any cloned repo silently rewrite agent behavior. Prompts are
//! code — a family file merges only if it beats the incumbent on the bench
//! matrix (see crates/rift-core/prompts/README.md).

use std::path::Path;

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
const EMBEDDED: &[(&str, &str)] = &[("default", include_str!("../prompts/default.md"))];

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

/// User-level override targets from `~/.config/rift/prompts/*.md`, for
/// experimenting without recompiling. Sorted by filename so precedence
/// between overrides is deterministic.
pub fn override_targets() -> Vec<PromptTarget> {
    match crate::paths::config_dir() {
        Some(dir) => targets_from_dir(&dir.join("rift/prompts")),
        None => vec![],
    }
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
/// substrings hit the lowercased model name, else the first `default`
/// family, else the first target. Callers put overrides before embedded
/// targets so an override wins both the match and the fallback.
pub fn select<'a>(model: &str, targets: &'a [PromptTarget]) -> Option<&'a PromptTarget> {
    let model = model.to_lowercase();
    targets
        .iter()
        .find(|t| t.matches.iter().any(|m| model.contains(m.as_str())))
        .or_else(|| targets.iter().find(|t| t.family == "default"))
        .or_else(|| targets.first())
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
        let t = select("gemma4:26b", &targets).unwrap();
        assert_eq!(t.family, "default");
        let p = render(t, "/work/repo");
        assert!(p.starts_with("You are Rift, an expert coding agent working in the directory /work/repo."));
        assert!(!p.contains("{cwd}") && !p.contains("{shell}"));
        assert!(p.contains("repo_map"));
        assert!(p.ends_with("already see."));
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
    fn override_dir_targets_precede_embedded() {
        let dir = std::env::temp_dir().join(format!("rift-prompts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.md"), "---\n---\noverridden {cwd}").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let mut targets = targets_from_dir(&dir);
        targets.extend(embedded_targets());
        let t = select("gemma4:26b", &targets).unwrap();
        assert_eq!(render(t, "/x"), "overridden /x");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
