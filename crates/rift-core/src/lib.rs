pub mod agent;
pub mod compact;
pub mod config;
pub mod mcp;
pub mod outline;
pub mod outline_cache;
pub mod paths;
pub mod prompts;
pub mod session;
pub mod skills;
pub mod subagent;
pub mod swarm;
pub mod tasks;
pub mod tools;
pub mod trace;

pub use agent::{Agent, AgentConfig, AgentEvent, TurnStats, EFFORT_LEVELS};
pub use config::{
    mcp_entry_trusted, trust_mcp_entry, untrust_mcp_entry, Config, LoadedConfig, ProviderConfig,
};
pub use mcp::{McpClient, McpTool};
pub use session::{SavedSession, SessionStore};
pub use skills::{load_skills, skills_prompt_section, Skill, SkillTool};
pub use subagent::{AgentTool, SubAgentHandle};
pub use tasks::{BgTasks, TaskKind, TaskStatus, TaskView};
pub use swarm::{
    judge_swarm, run_swarm, Candidate, CandidateOutcome, JudgeVerdict, ProviderFactory, Swarm,
};
pub use tools::{
    builtin_bash_deny, AskRequest, AskUserTool, EditRecord, PlanItem, Tool, ToolCtx, ToolRegistry,
};
pub use trace::{FailureCounters, TraceWriter};

/// System prompt for a model, resolved through the prompt-target machinery
/// (crates/rift-core/prompts/): user overrides from ~/.config/rift/prompts/
/// are matched first, then the embedded family targets, then `default` —
/// short (every token counts against num_ctx), explicit about tool
/// mechanics, and firm that tool calls must be structured.
pub fn system_prompt_for(model: &str, cwd: &str) -> String {
    let mut targets = prompts::override_targets();
    targets.extend(prompts::embedded_targets());
    match prompts::select(model, &targets) {
        Some(t) => prompts::render(t, cwd),
        // Unreachable while default.md is embedded; a bare fallback beats a panic.
        None => format!("You are Rift, an expert coding agent working in the directory {cwd}."),
    }
}

/// The default-family prompt, model-agnostic. Prefer [`system_prompt_for`].
pub fn system_prompt(cwd: &str) -> String {
    system_prompt_for("", cwd)
}

/// Cap on how much project context gets injected into the system prompt —
/// the guides must help the context budget, not eat it.
const GUIDE_MAX_CHARS: usize = 6000;

/// Context files loaded at startup, in priority order. RIFT.md is ours;
/// AGENTS.md is the cross-tool standard (pi, Codex, …); CLAUDE.md for repos
/// already set up for Claude Code. All found files are concatenated.
const CONTEXT_FILES: &[&str] = &["RIFT.md", "AGENTS.md", "CLAUDE.md"];

/// System prompt for `model` plus the project's agent context files from
/// the root of `cwd`. Returns (prompt, names of the files that were loaded).
pub fn system_prompt_with_guide(model: &str, cwd: &std::path::Path) -> (String, Vec<String>) {
    let mut prompt = system_prompt_for(model, &cwd.display().to_string());
    let mut loaded = vec![];
    let mut remaining = GUIDE_MAX_CHARS;
    for file in CONTEXT_FILES {
        if remaining == 0 {
            break;
        }
        let Ok(guide) = std::fs::read_to_string(cwd.join(file)) else { continue };
        let guide = guide.trim();
        if guide.is_empty() {
            continue;
        }
        let mut shown: String = guide.chars().take(remaining).collect();
        remaining = remaining.saturating_sub(shown.chars().count());
        if shown.len() < guide.len() {
            shown.push_str(&format!("\n[{file} truncated]"));
        }
        prompt.push_str(&format!(
            "\n\nProject guide (from {file} — instructions for agents working in this repo):\n{shown}"
        ));
        loaded.push(file.to_string());
    }
    (prompt, loaded)
}
