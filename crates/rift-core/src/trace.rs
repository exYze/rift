//! Opt-in per-turn JSONL traces (`--trace <path>` / `RIFT_TRACE`).
//!
//! The agent loop's hardening layer already *detects* the interesting
//! failure modes — textual tool-call recovery, alias resolution, doom-loop
//! guard, truncation detection — and then throws the signal away after
//! papering over it. Traces keep it: one JSON line per turn with the model,
//! token counts, tool calls, failure counters, and outcome, so prompt /
//! packing / model experiments have real data to optimize against (see
//! docs/ROADMAP.md, v0.7 + "Engineering process").
//!
//! Local files only, never on by default, nothing leaves the machine.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// Counters for the hardening interventions during one turn. All zeros is a
/// clean turn; anything nonzero is a model or context failure the loop
/// recovered from — exactly the signal worth aggregating across models and
/// tasks.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FailureCounters {
    /// Prompt filled `num_ctx`; front-truncation suspected.
    pub truncations: u32,
    /// Tool calls recovered from plain JSON in reply text.
    pub textual_recoveries: u32,
    /// Hallucinated tool names resolved to canonical ones.
    pub alias_hits: u32,
    /// Identical (name, args) call repeated 3× and refused.
    pub doom_loop_trips: u32,
    /// Calls to tools that don't exist (after alias resolution).
    pub unknown_tools: u32,
    /// Tool executions that returned an error result.
    pub tool_errors: u32,
    /// Chat-only answer on an actionable prompt; nudged to apply for real.
    pub apply_nudges: u32,
    /// Turn scrapped and retried at temperature 0 (headless/swarm only).
    pub greedy_retries: u32,
    /// Chat-template special tokens leaked into output and stripped.
    pub template_strips: u32,
    /// Context pressure: old tool outputs pruned (stage-1 compaction).
    pub compact_prunes: u32,
    /// Context pressure: history LLM-summarized (stage-2 compaction).
    pub compact_summaries: u32,
}

impl FailureCounters {
    /// Fold another turn's counters into a running (session) total.
    pub fn add(&mut self, other: &FailureCounters) {
        self.truncations += other.truncations;
        self.textual_recoveries += other.textual_recoveries;
        self.alias_hits += other.alias_hits;
        self.doom_loop_trips += other.doom_loop_trips;
        self.unknown_tools += other.unknown_tools;
        self.tool_errors += other.tool_errors;
        self.apply_nudges += other.apply_nudges;
        self.greedy_retries += other.greedy_retries;
        self.template_strips += other.template_strips;
        self.compact_prunes += other.compact_prunes;
        self.compact_summaries += other.compact_summaries;
    }

    /// Model-misbehavior interventions (excludes the compaction counters,
    /// which signal context pressure rather than a model failure).
    pub fn model_failures(&self) -> u32 {
        self.truncations
            + self.textual_recoveries
            + self.alias_hits
            + self.doom_loop_trips
            + self.unknown_tools
            + self.tool_errors
            + self.apply_nudges
            + self.greedy_retries
            + self.template_strips
    }
}

/// One executed (or refused) tool call within a turn.
#[derive(Debug, Clone, Serialize)]
pub struct ToolTraceRecord {
    pub name: String,
    pub ok: bool,
    /// The call's file/pattern argument when it has one — the data the
    /// repo-map ranking and retrieval heuristics are tuned against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The JSONL record for one completed turn.
#[derive(Debug, Serialize)]
pub struct TurnTrace<'a> {
    /// Unix seconds when the turn ended.
    pub ts: u64,
    pub model: &'a str,
    /// Provider base URL — distinguishes servers/protocols in mixed traces.
    pub provider: &'a str,
    pub num_ctx: u64,
    /// Capped head of the user prompt (newlines flattened).
    pub prompt: String,
    /// "answered" | "cancelled" | "max_iterations" | "error".
    pub outcome: &'a str,
    pub iterations: usize,
    pub prompt_tokens: u64,
    /// Summed per-call prompt tokens — the billable input for the turn.
    pub billed_prompt_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u128,
    pub tools: &'a [ToolTraceRecord],
    pub failures: &'a FailureCounters,
}

/// Appends turn records to a JSONL file. Each record opens the file in
/// append mode and writes one whole line, so concurrent rift processes
/// (bench matrix runs) interleave lines instead of corrupting each other.
pub struct TraceWriter {
    path: PathBuf,
}

impl TraceWriter {
    /// Validates the path is writable up front (creating parent directories)
    /// so a bad `--trace` fails at startup, not silently on the first turn.
    pub fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening trace file {}", path.display()))?;
        Ok(Self { path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, trace: &TurnTrace) -> Result<()> {
        let mut line = serde_json::to_string(trace)?;
        line.push('\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .with_context(|| format!("appending to trace file {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trace<'a>(tools: &'a [ToolTraceRecord], failures: &'a FailureCounters) -> TurnTrace<'a> {
        TurnTrace {
            ts: 1_700_000_000,
            model: "gemma4:26b",
            provider: "http://localhost:11434",
            num_ctx: 32_768,
            prompt: "fix the parser".into(),
            outcome: "answered",
            iterations: 3,
            prompt_tokens: 1200,
            billed_prompt_tokens: 2900,
            output_tokens: 340,
            duration_ms: 4200,
            tools,
            failures,
        }
    }

    #[test]
    fn records_append_as_jsonl() {
        let dir = std::env::temp_dir().join(format!("rift-trace-test-{}", std::process::id()));
        let path = dir.join("nested").join("trace.jsonl");
        let w = TraceWriter::new(&path).unwrap();

        let tools = vec![
            ToolTraceRecord { name: "read".into(), ok: true, target: Some("src/lib.rs".into()) },
            ToolTraceRecord { name: "edit".into(), ok: false, target: None },
        ];
        let failures = FailureCounters { tool_errors: 1, ..Default::default() };
        w.record(&sample_trace(&tools, &failures)).unwrap();
        w.record(&sample_trace(&tools, &failures)).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["model"], "gemma4:26b");
        assert_eq!(v["outcome"], "answered");
        assert_eq!(v["tools"][1]["ok"], false);
        assert_eq!(v["failures"]["tool_errors"], 1);
        assert_eq!(v["failures"]["doom_loop_trips"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counters_fold_and_classify() {
        let mut total = FailureCounters::default();
        let turn = FailureCounters { alias_hits: 2, compact_prunes: 1, ..Default::default() };
        total.add(&turn);
        total.add(&turn);
        assert_eq!(total.alias_hits, 4);
        assert_eq!(total.compact_prunes, 2);
        // compaction is context pressure, not a model failure
        assert_eq!(total.model_failures(), 4);
    }
}
