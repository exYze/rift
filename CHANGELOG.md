# Changelog

All notable changes to rift. Versions follow the roadmap phases in
[docs/ROADMAP.md](docs/ROADMAP.md); dates are release dates.

## v0.8.0 — 2026-07-02 · v0.7 phase complete: cloud + swarm

- **Cross-provider WarpDrive**: one swarm race can mix providers — each
  candidate's model string (`gemma4:26b` vs `anthropic/claude-sonnet-4-6`)
  resolves through a provider factory to its own client
- **Swarm auto-judge** (`--judge <model>`): a referee model scores every
  candidate's diff and recommends a winner; TUI shows the verdict in the
  winner's log, `--no-tui` prints a machine-parseable `JUDGE: winner=` line
- **Cost display** for metered providers: `/stats` and the headless summary
  show estimated $; billed input tracked as summed per-call prompt tokens;
  built-in Anthropic rates plus a config `pricing` map
- `bench/judge_bench.py`: measures judge accuracy against verify-script
  ground truth (discriminative-case accuracy is the headline number)
- Swarm diffs exclude interpreter cache junk (`__pycache__`, `*.pyc`) that
  unfairly penalized candidates in judged races

## v0.7.2 — 2026-07-02

- First per-model prompt target shipped provisionally: `gemma.md` puts the
  tool-application contract first (from trace analysis: 8 of gemma4:26b's
  10 bench failures were chat-only answers)
- The apply-nudge now keys on *mutating* tool use (write/edit/bash), closing
  the explored-but-never-edited escape
- Fixed `t12_cents` bench task (was unpassable: float-representation trap)
- 3-model matrix results published in README/BENCHMARKS

## v0.7.1 — 2026-07-02

- **Turn traces + failure counters**: opt-in `--trace <file>` / `RIFT_TRACE`
  JSONL records model, tokens, tool calls, hardening/failure counters, and
  outcome per turn; `/stats` shows a recoveries line
- **Prompt-target machinery**: per-model-family system prompts compiled into
  the binary (`crates/rift-core/prompts/<family>.md`), selected by model
  name, user-overridable via `~/.config/rift/prompts/`
- **Bench model matrix**: `bench.py --models a,b,c` runs the suite per model
  and diffs pass rate / tokens / wall time; `--tasks` subset filter

## v0.7.0 — 2026-07-01 · cloud providers

- **Native Anthropic provider**: the Messages API spoken natively (SSE
  content blocks, tool_use/tool_result, thinking with signature round-trip);
  `anthropic/<model>` works with just `ANTHROPIC_API_KEY` in the env
- `openai/<model>` built-in provider over the existing OpenAI protocol
- Fenced code blocks render as labeled boxes in the TUI

## v0.6.4 — 2026-07-01

- Per-provider hardening test suite: deterministic mock-server tests in CI
  (SSE/NDJSON framing, mid-stream errors, tool-call accumulation,
  truncation detection) plus env-gated live suites against real servers

## v0.6.3 — 2026-07-01

- Completed the v0.5 UX scope: `@file` mentions with palette completion,
  syntax highlighting (syntect, feature-gated), live diff pane (Ctrl+D),
  built-in themes (`dark`/`light`/`mono`)

## v0.6.2 — 2026-07-01

- Config merge semantics: project `.rift.json` overlays the user config;
  permissions only tighten (deny-list union, approve stays on)
- `/mcp trust` management; transport retries

## v0.6.1 — 2026-07-01

- Hardening sweep: data safety, provider robustness, MCP trust gating for
  project-defined servers

## v0.6.0 — 2026-07-01 · provider abstraction

- `Provider` trait extracted — Ollama becomes one implementation
- **OpenAI-compatible provider**: vLLM, LM Studio, llama.cpp server,
  LiteLLM, OpenRouter; per-provider base URLs and keys in config
- Token accounting normalized across providers

## v0.5.1 — 2026-06-30

- Ctrl+C no longer exits the TUI (use `/quit`)

## v0.5.0 — 2026-06-30 · command + UX expansion

- New slash commands: `/retry`, `/stats`, `/system`, `/temp`, `/ctx`,
  `/worktrees`, `/save`, named `/sessions`, `/quit`
- Startup resilience: unreachable server or missing model opens the TUI
  anyway with a recovery hint

## v0.4.x — 2026-06-11 → 2026-06-28 · plan view + trust

- Plan tool with live activity-pane checklist; `ask_user` elicitation;
  interactive `/model` and `/sessions` pickers
- Approval mode (`--approve`): y/n gate on write/edit/bash with per-session
  always-allow
- Agent Skills standard (`.rift/skills/` SKILL.md), `/skill:<name>`
- RIFT.md / AGENTS.md / CLAUDE.md auto-loaded into the system prompt
- Full input-line editing (cursor movement, word jumps, bracketed paste)
- Cross-platform: Windows support (cmd.exe shell, USERPROFILE fallback,
  install.ps1), CI build/test matrix on macOS/Linux/Windows
- 50-task benchmark vs opencode: 44/50 vs 42/50, −57% prompt tokens,
  3.4× faster (see docs/BENCHMARKS.md)

## v0.3.x — 2026-06-11 · polish

- `rift update` self-updater with startup check and `/update`
- `/copy` command and Ctrl+T native-selection toggle
- Paste truncation, ANSI corruption, and false-hang fixes
- Model-failure harness: timeout salvage, server probe, edit hints

## v0.2.0 — 2026-06-11

- Slash-command palette popup

## v0.1.0 — 2026-06-11 · initial release

- Native Ollama `/api/chat` terminal coding agent: pre-wrapped flicker-free
  TUI, AST-based context compaction, textual tool-call recovery, doom-loop
  guard, WarpDrive worktree races, MCP, 16 slash commands
- Release pipeline: CI, cross-platform builds, one-line install script
