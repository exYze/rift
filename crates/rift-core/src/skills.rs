//! Skills: capability packages following the Agent Skills standard (the
//! SKILL.md convention used by pi, Claude Code, and others).
//!
//! Discovery: `.rift/skills/` in the project and `~/.config/rift/skills/`
//! for the user — either `<name>/SKILL.md` or flat `<name>.md`. Project
//! skills win on name collisions.
//!
//! Progressive disclosure: only name + description are listed in the system
//! prompt; the model loads a skill's full body on demand with the `skill`
//! tool, and users invoke one directly with `/skill:<name> [task]`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::tools::{Tool, ToolCtx};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: PathBuf,
}

/// Parse a minimal `--- key: value ---` frontmatter block. Returns
/// (name, description, body); missing fields fall back to caller defaults.
fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>, String) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, None, text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, None, text.to_string());
    };
    let (head, body) = rest.split_at(end);
    let body = body.trim_start_matches("\n---").trim_start_matches('-').trim_start().to_string();
    let mut name = None;
    let mut description = None;
    for line in head.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(value.trim().to_string()),
                "description" => description = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    (name, description, body)
}

/// Safety rail on a skill body before it reaches the model. Skills load on
/// demand (the model or user explicitly asks for one), so this is deliberately
/// generous — far larger than the per-session context-file cap — and only trims
/// a pathologically large SKILL.md that would otherwise blow the context budget.
const SKILL_BODY_MAX_CHARS: usize = 12_000;

fn cap_body(body: String) -> String {
    if body.chars().count() <= SKILL_BODY_MAX_CHARS {
        return body;
    }
    let mut capped: String = body.chars().take(SKILL_BODY_MAX_CHARS).collect();
    capped.push_str("\n\n[skill body truncated — keep SKILL.md focused]");
    capped
}

fn skill_from_file(path: &Path, default_name: &str) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let (name, description, body) = parse_frontmatter(&text);
    let description = description.unwrap_or_else(|| {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("(no description)")
            .chars()
            .take(120)
            .collect()
    });
    Some(Skill {
        name: name.unwrap_or_else(|| default_name.to_string()),
        description,
        body: cap_body(body),
        source: path.to_path_buf(),
    })
}

fn scan_dir(dir: &Path, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let skill = if path.is_dir() {
            skill_from_file(&path.join("SKILL.md"), &stem)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            skill_from_file(&path, &stem)
        } else {
            None
        };
        if let Some(s) = skill {
            // First writer wins — callers scan project dirs before user dirs.
            if !out.iter().any(|e| e.name == s.name) {
                out.push(s);
            }
        }
    }
}

/// All skills visible from `cwd`: project `.rift/skills/` first (wins on
/// name collisions), then the user-level `~/.config/rift/skills/`.
pub fn load_skills(cwd: &Path) -> Vec<Skill> {
    let mut out = vec![];
    scan_dir(&cwd.join(".rift/skills"), &mut out);
    if let Some(cfg) = crate::paths::config_dir() {
        scan_dir(&cfg.join("rift/skills"), &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One-line-per-skill listing for the system prompt (progressive disclosure:
/// bodies stay out of context until loaded).
pub fn skills_prompt_section(skills: &[Skill]) -> String {
    let mut out = String::from(
        "\n\nAvailable skills — packaged instructions for specific tasks. When one matches \
         the user's request, load it with the skill tool and follow it:\n",
    );
    for s in skills {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    out
}

/// Model-facing loader for skill bodies.
pub struct SkillTool {
    skills: std::sync::Arc<Vec<Skill>>,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills: std::sync::Arc::new(skills) }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Load the full instructions of an available skill by name. Use when a listed skill matches the current task, then follow its instructions."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "description": "Skill name exactly as listed"}
            }
        })
    }
    async fn execute(&self, args: &Map<String, Value>, _ctx: &ToolCtx) -> Result<String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required string parameter 'name'"))?;
        match self.skills.iter().find(|s| s.name == name) {
            Some(s) => Ok(format!("--- SKILL: {} ---\n{}", s.name, s.body)),
            None => bail!(
                "no skill named '{name}'. Available: {}",
                self.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parsed_and_optional() {
        let (n, d, b) = parse_frontmatter("---\nname: greet\ndescription: says hi\n---\n\nDo the thing.");
        assert_eq!(n.as_deref(), Some("greet"));
        assert_eq!(d.as_deref(), Some("says hi"));
        assert_eq!(b, "Do the thing.");

        let (n, d, b) = parse_frontmatter("# Just a body\nfirst line");
        assert!(n.is_none() && d.is_none());
        assert!(b.starts_with("# Just a body"));
    }

    #[test]
    fn loads_project_skills_with_fallback_description() {
        let dir = std::env::temp_dir().join(format!("rift-skills-{}", std::process::id()));
        let sdir = dir.join(".rift/skills");
        std::fs::create_dir_all(sdir.join("deploy")).unwrap();
        std::fs::write(
            sdir.join("deploy/SKILL.md"),
            "---\nname: deploy\ndescription: ship the app\n---\nRun the deploy script.",
        )
        .unwrap();
        std::fs::write(sdir.join("haiku.md"), "# Haiku\nWrite a haiku about the diff.").unwrap();

        let skills = load_skills(&dir);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "ship the app");
        assert_eq!(skills[1].name, "haiku");
        assert!(skills[1].description.contains("Write a haiku"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_skill_body_is_capped() {
        let capped = cap_body("x".repeat(SKILL_BODY_MAX_CHARS + 5000));
        assert!(capped.chars().count() <= SKILL_BODY_MAX_CHARS + 100, "body not capped");
        assert!(capped.contains("truncated"), "missing truncation marker");
        assert_eq!(cap_body("short".into()), "short"); // small bodies untouched
    }
}
