//! Compactor: keep the conversation inside the model's real context window.
//!
//! Local models silently degrade when the prompt overflows `num_ctx` (Ollama
//! front-truncates with no error), so the budget is enforced CLIENT-side,
//! before every request:
//!
//! 1. cheap estimate (chars/4), continuously calibrated against the server's
//!    actual `prompt_eval_count` from the previous call
//! 2. stage 1 — non-destructive-ish prune: elide bulky tool outputs and old
//!    thinking from everything except the last two user turns
//! 3. stage 2 — LLM summarization: collapse pruned history into a structured
//!    summary, keep the current turn verbatim

use anyhow::Result;
use rift_provider::{ChatOptions, ChatRequest, Message, Provider, Role, StreamDelta};

/// Tokens reserved for the model's output within num_ctx.
pub const OUTPUT_RESERVE: u64 = 4096;
/// Safety margin for template overhead / estimator error.
pub const SAFETY_MARGIN: u64 = 1024;
/// Don't bother eliding tool outputs smaller than this (chars).
const PRUNE_MIN_CHARS: usize = 800;
/// User turns (counted from the end) whose tool outputs are never pruned.
const PROTECTED_TURNS: usize = 2;

pub fn usable_budget(num_ctx: u64) -> u64 {
    num_ctx.saturating_sub(OUTPUT_RESERVE + SAFETY_MARGIN)
}

pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64) / 4 + 1
}

/// Rough prompt-token estimate for a request. `calibration` is the running
/// ratio of actual/estimated from previous calls (1.0 = trust the heuristic).
pub fn estimate_prompt_tokens(messages: &[Message], tools_overhead: u64, calibration: f64) -> u64 {
    let mut total = tools_overhead;
    for m in messages {
        total += estimate_tokens(&m.content) + 8;
        if let Some(t) = &m.thinking {
            total += estimate_tokens(t);
        }
        for tc in &m.tool_calls {
            total += estimate_tokens(&serde_json::to_string(&tc.function).unwrap_or_default());
        }
    }
    ((total as f64) * calibration).ceil() as u64
}

/// Index of the first message belonging to the last `PROTECTED_TURNS` user
/// turns (everything from there on is never pruned).
fn protected_start(messages: &[Message]) -> usize {
    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    if user_idxs.len() <= PROTECTED_TURNS {
        return 0;
    }
    user_idxs[user_idxs.len() - PROTECTED_TURNS]
}

/// Stage 1: elide bulky tool outputs and drop stale thinking in older turns.
/// Returns the number of messages touched. The elision note tells the model
/// how to recover the data (re-run the tool) so this is safe.
pub fn prune_old_turns(messages: &mut [Message]) -> usize {
    let cutoff = protected_start(messages);
    let mut touched = 0;
    for m in &mut messages[..cutoff] {
        match m.role {
            Role::Tool if m.content.len() > PRUNE_MIN_CHARS => {
                let head: String = m.content.chars().take(200).collect();
                m.content = format!(
                    "{head}\n[... elided {} bytes to save context; re-run the tool if you need this again]",
                    m.content.len()
                );
                touched += 1;
            }
            Role::Assistant if m.thinking.is_some() => {
                m.thinking = None;
                touched += 1;
            }
            _ => {}
        }
    }
    touched
}

/// Render history (minus the system prompt and the current turn) as plain
/// text for the summarizer, with per-message caps AND a total cap. The total
/// cap matters: compaction fires precisely when history is over budget, and
/// an over-budget summarization request would itself be front-truncated by
/// the server — losing the oldest context (the goal/constraints), which is
/// exactly what the summary needs most. When over the cap, keep both ends
/// and drop the middle.
fn transcript_for_summary(messages: &[Message], max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in messages {
        let role = match m.role {
            Role::System => continue,
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::Tool => "TOOL RESULT",
        };
        let content: String = m.content.chars().take(1500).collect();
        if !content.trim().is_empty() {
            lines.push(format!("{role}: {content}\n"));
        }
        for tc in &m.tool_calls {
            let args = serde_json::to_string(&tc.function.arguments).unwrap_or_default();
            let args: String = args.chars().take(400).collect();
            lines.push(format!("ASSISTANT CALLED: {}({args})\n", tc.function.name));
        }
    }
    let total: usize = lines.iter().map(String::len).sum();
    if total <= max_chars {
        return lines.concat();
    }
    // 40% head (goal, constraints, early decisions), 60% tail (recent state).
    let head_budget = max_chars * 2 / 5;
    let tail_budget = max_chars - head_budget;
    let mut head_end = 0;
    let mut used = 0;
    for l in &lines {
        if used + l.len() > head_budget {
            break;
        }
        used += l.len();
        head_end += 1;
    }
    let mut tail_start = lines.len();
    let mut used_tail = 0;
    for l in lines.iter().rev() {
        if used_tail + l.len() > tail_budget || tail_start == head_end {
            break;
        }
        used_tail += l.len();
        tail_start -= 1;
    }
    let mut out = lines[..head_end].concat();
    out.push_str(&format!("[... {} messages omitted for length ...]\n", tail_start - head_end));
    out.push_str(&lines[tail_start..].concat());
    out
}

/// Stage 2: LLM summarization. Collapses everything between the system prompt
/// and the current turn into one structured summary message. Returns the new
/// message list: [system, summary, <current turn ...>].
pub async fn summarize_history(
    client: &dyn Provider,
    model: &str,
    num_ctx: u64,
    messages: &[Message],
) -> Result<Vec<Message>> {
    let current_turn_start = {
        // current turn = from the LAST user message onward
        messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(messages.len().saturating_sub(1))
    };
    let head = &messages[..current_turn_start];
    let tail = &messages[current_turn_start..];

    // ~4 chars/token estimate with margin: keep the request itself inside
    // the summarizer's own num_ctx.
    let max_chars = (usable_budget(num_ctx) as usize).saturating_mul(3);
    let transcript = transcript_for_summary(head, max_chars);
    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(
                "You compress coding-agent conversations. Summarize the transcript into exactly these sections:\n\
                 ## Goal\n## Constraints\n## Progress (files changed, commands run, results)\n## Key Decisions\n## Next Steps / Critical Context\n\
                 Be specific about file paths and findings. No preamble.",
            ),
            Message::user(transcript),
        ],
        tools: vec![],
        stream: false,
        think: Some(false),
        keep_alive: Some("10m".into()),
        options: Some(ChatOptions { num_ctx: Some(num_ctx), temperature: Some(0.0), num_predict: Some(1500) }),
    };
    let mut noop = |_: StreamDelta| {};
    let outcome = client.chat_stream(&req, &mut noop).await?;
    let summary = outcome.message.content;

    let mut rebuilt = Vec::with_capacity(tail.len() + 2);
    if let Some(sys) = messages.first().filter(|m| m.role == Role::System) {
        rebuilt.push(sys.clone());
    }
    rebuilt.push(Message::user(format!(
        "[Earlier conversation was compacted. Summary:]\n{summary}"
    )));
    rebuilt.extend(tail.iter().cloned());
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Message> {
        vec![
            Message::system("sys"),
            Message::user("turn 1"),
            Message::tool_result("read", "x".repeat(5000)),
            Message::user("turn 2"),
            Message::tool_result("read", "y".repeat(5000)),
            Message::user("turn 3"),
            Message::tool_result("read", "z".repeat(5000)),
        ]
    }

    #[test]
    fn prune_protects_last_two_turns() {
        let mut m = msgs();
        let touched = prune_old_turns(&mut m);
        assert_eq!(touched, 1);
        assert!(m[2].content.contains("elided"));
        assert!(!m[4].content.contains("elided"));
        assert!(!m[6].content.contains("elided"));
    }

    #[test]
    fn estimator_scales_with_calibration() {
        let m = vec![Message::user("a".repeat(4000))];
        let base = estimate_prompt_tokens(&m, 0, 1.0);
        let scaled = estimate_prompt_tokens(&m, 0, 1.5);
        assert!(base >= 1000);
        assert!(scaled > base);
    }

    #[test]
    fn budget_subtracts_reserves() {
        assert_eq!(usable_budget(32_768), 32_768 - OUTPUT_RESERVE - SAFETY_MARGIN);
    }

    #[test]
    fn transcript_cap_keeps_head_and_tail() {
        let msgs: Vec<Message> = (0..50)
            .map(|i| Message::user(format!("message number {i} {}", "x".repeat(100))))
            .collect();
        let full = transcript_for_summary(&msgs, usize::MAX);
        let capped = transcript_for_summary(&msgs, 2000);
        assert!(full.len() > capped.len());
        assert!(capped.len() < 2300, "cap overshot: {}", capped.len());
        // The goal lives at the start, current state at the end — both must survive.
        assert!(capped.contains("message number 0"));
        assert!(capped.contains("message number 49"));
        assert!(capped.contains("omitted for length"));
    }
}
