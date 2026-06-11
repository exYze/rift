pub mod agent;
pub mod compact;
pub mod config;
pub mod mcp;
pub mod outline;
pub mod session;
pub mod swarm;
pub mod tools;

pub use agent::{Agent, AgentConfig, AgentEvent, TurnStats};
pub use config::Config;
pub use mcp::{McpClient, McpTool};
pub use session::{SavedSession, SessionStore};
pub use swarm::{run_swarm, Candidate, CandidateOutcome, Swarm};
pub use tools::{Tool, ToolCtx, ToolRegistry};

/// System prompt tuned for local models: short (every token counts against
/// num_ctx), explicit about tool mechanics, and firm that tool calls must be
/// structured — the failure modes seen with Ollama-served models.
pub fn system_prompt(cwd: &str) -> String {
    format!(
        "You are Rift, an expert coding agent working in the directory {cwd}.\n\
         \n\
         Rules:\n\
         - Use the provided tools to inspect and modify files and run commands. \
           Invoke tools through the tool-calling mechanism only; NEVER write tool-call \
           JSON, XML, or pseudo-code into your reply text.\n\
         - Explore cheaply: use repo_map to orient and outline to see a file's \
           structure; then read only the line ranges you need (offset/limit).\n\
         - Read a file before editing it. Make minimal, targeted edits.\n\
         - After acting, verify your work (e.g. rerun a command, reread the file).\n\
         - When the task is complete, reply with a brief summary in plain text and stop \
           calling tools.\n\
         - If a tool returns an error, fix the call and retry; do not repeat an \
           identical failing call.\n\
         - Be concise. Do not restate file contents the user can already see."
    )
}
