pub mod agent;
pub mod compact;
pub mod config;
pub mod mcp;
pub mod outline;
pub mod paths;
pub mod session;
pub mod skills;
pub mod swarm;
pub mod tools;

pub use agent::{Agent, AgentConfig, AgentEvent, TurnStats};
pub use config::{mcp_entry_trusted, trust_mcp_entry, Config, ProviderConfig};
pub use mcp::{McpClient, McpTool};
pub use session::{SavedSession, SessionStore};
pub use skills::{load_skills, skills_prompt_section, Skill, SkillTool};
pub use swarm::{run_swarm, Candidate, CandidateOutcome, Swarm};
pub use tools::{
    builtin_bash_deny, AskRequest, AskUserTool, EditRecord, PlanItem, Tool, ToolCtx, ToolRegistry,
};

/// System prompt tuned for local models: short (every token counts against
/// num_ctx), explicit about tool mechanics, and firm that tool calls must be
/// structured — the failure modes seen with Ollama-served models.
pub fn system_prompt(cwd: &str) -> String {
    let shell = shell_note();
    format!(
        "You are Rift, an expert coding agent working in the directory {cwd}.\n\
         \n\
         Rules:\n\
         - Use the provided tools to inspect and modify files and run commands. \
           Invoke tools through the tool-calling mechanism only; NEVER write tool-call \
           JSON, XML, or pseudo-code into your reply text.\n\
         - {shell}\n\
         - Explore cheaply: use repo_map to orient and outline to see a file's \
           structure; then read only the line ranges you need (offset/limit).\n\
         - For multi-step tasks, first call plan(set=[...]) with your intended \
           steps, then plan(done=N) as you complete each one. Keep it current — \
           the user watches this checklist to follow your progress.\n\
         - Read a file before editing it. Make minimal, targeted edits.\n\
         - After acting, verify your work (e.g. rerun a command, reread the file).\n\
         - When the task is complete, reply with a brief summary in plain text and stop \
           calling tools.\n\
         - If a tool returns an error, fix the call and retry; do not repeat an \
           identical failing call.\n\
         - If the task is ambiguous and an ask_user tool is available, ask ONE \
           clarifying question (with choices when natural) before doing work that \
           could be wrong; otherwise proceed on your best judgment.\n\
         - Be concise. Do not restate file contents the user can already see."
    )
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

/// Cap on how much project context gets injected into the system prompt —
/// the guides must help the context budget, not eat it.
const GUIDE_MAX_CHARS: usize = 6000;

/// Context files loaded at startup, in priority order. RIFT.md is ours;
/// AGENTS.md is the cross-tool standard (pi, Codex, …); CLAUDE.md for repos
/// already set up for Claude Code. All found files are concatenated.
const CONTEXT_FILES: &[&str] = &["RIFT.md", "AGENTS.md", "CLAUDE.md"];

/// System prompt plus the project's agent context files from the root of
/// `cwd`. Returns (prompt, names of the files that were loaded).
pub fn system_prompt_with_guide(cwd: &std::path::Path) -> (String, Vec<String>) {
    let mut prompt = system_prompt(&cwd.display().to_string());
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
