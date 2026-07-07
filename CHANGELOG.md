# Changelog

All notable changes to rift. Versions follow the roadmap phases in
[docs/ROADMAP.md](docs/ROADMAP.md); dates are release dates.

## v1.0.3 — 2026-07-06

- **10 new color themes**: `dracula`, `nord`, `gruvbox`, `solarized-dark`,
  `solarized-light`, `tokyo-night`, `catppuccin`, `rose-pine`, `matrix`,
  `synthwave` — truecolor palettes that paint their own text, background,
  and border colors (the classic `dark`/`light`/`mono` stay
  terminal-native). The `Theme` struct gained `fg`/`bg`/`border`; bare
  `/theme` now opens an interactive picker, and each theme's syntect
  mapping is pinned by test

## v1.0.2 — 2026-07-06

- **Claude Code-style permissions**: interactive sessions now **ask before
  write/edit/bash by default**, and each bash prompt offers allow once /
  **always allow `<pattern>`** (e.g. `git push *`, persisted to
  `permissions.bash_allow` in the user config — never prompts again) /
  allow all bash this session / deny. Allow-listed commands run silently;
  chained commands need every segment allowed; the deny list still always
  wins. A project `.rift.json` can only tighten (its `bash_allow` is
  ignored, its `approve: true` is honored, `approve: false` is not).
  Opt out with `"approve": false` in the user config
- **`/yolo [off]`**: stop asking before write/edit/bash entirely (the
  built-in + configured deny list is still enforced); `/yolo off` restores
  prompts. `/permissions` now shows allowed and banned patterns side by
  side
- **`/btw <question>`** (modeled on Claude Code's /btw): a quick side
  question that sees the whole conversation but has no tools, never enters
  the main history (the agent never sees the exchange), and runs on a
  UI-side task — so it works even while the agent is mid-turn. Follow-up
  `/btw`s continue a small side thread (kept to the last 10 exchanges,
  `/btw clear` resets it); answers render as dimmed `(btw)` blocks,
  buffered so a streaming main turn is never garbled. Context comes from
  the session autosave (history up to the last completed turn); the side
  question always uses the session's current model

## v1.0.1 — 2026-07-06

Verified against DeepSeek V4 Flash on vLLM; fixes for OpenAI-compatible
providers generally:

- **Context-length discovery**: `show()` now reads the model's context
  window from `/v1/models` (vLLM `max_model_len`, OpenRouter
  `context_length`, LM Studio `max_context_length`, llama.cpp
  `meta.n_ctx_train`). When the user hasn't set `--num-ctx`, provider-routed
  models adopt the reported context as rift's working budget (capped at
  128k so huge hosted contexts don't bloat every request); `/model`
  switches retune the budget the same way. Ollama keeps its conservative
  default — there num_ctx sizes the server's KV cache
- **Real throughput stats**: the OpenAI-compatible path now measures
  stream timing (prefill = to first chunk, decode = rest), so tok/s shows
  real numbers instead of 0.0 in the status line and `/stats`

## v1.0.0 — 2026-07-06

- **Concurrent sub-agents**: the model gets an `agent` tool — 1–4
  self-contained tasks run as concurrent child agents, each with its own
  context window, tool set, plan, and undo journal (permission policy and
  the ask channel are shared; nesting is blocked). Foreground calls wait
  and return every child's final report; `background=true` launches them
  as background tasks that keep working across turns. `/model` and
  `/host` switches carry over to children automatically
- **Background tasks**: `bash run_in_background=true` starts a command as
  a session-wide background task and returns its id immediately — it
  keeps running while the conversation continues. Output accumulates in
  a capped buffer; the new `task` tool lists/inspects/kills tasks; the
  status bar shows a live "N bg tasks running" count; and when a task
  finishes on its own, a `[task notification]` auto-turn hands the result
  back to the model (kills are silent, Esc suppresses pending
  notifications, at most 8 tasks run at once). `/tasks [kill <id>]` is
  the user-facing view of the same registry. Background tasks terminate
  with the rift process — no orphans

## v0.9.5 — 2026-07-03

- **`/goal <condition>`**: keep working until the model verifies the goal —
  turns auto-continue (up to 25) until a verified line-anchored `GOAL MET`,
  `/goal clear`/Esc stops it, and the run cap stops with a resume hint.
  A failed turn pauses the goal instead of spinning
- **`/loop [30s|5m|2h] <prompt or /command>`**: re-run a body on a fixed
  interval (rescheduled from fire time — no catch-up bursts) or
  back-to-back without one; `/loop stop` or Esc ends it, and a
  back-to-back loop halts if a run fails. Esc during a running auto-turn
  cancels the turn and the automation
- **Generator scope**: `/skills new` and `/mcp new` take `--global` (`-g`)
  to install user-wide (`~/.config/rift/`, every project, absolute paths,
  no trust prompt) instead of the project-scoped default (`.rift/`,
  trust-gated)

## v0.9.4 — 2026-07-03

- **Self-extension**: `/skills new <desc>` — the agent writes its own skill
  file from your description; `/mcp new <desc>` — the agent writes,
  self-tests, and registers a local stdio MCP server (stdlib-only Python),
  trust-gated like any project-config server. `/restart` loads either
  without losing your chat
- MCP client accepts the common bare-content-array `tools/call` result
  shape instead of silently reading an empty string

## v0.9.3 — 2026-07-03

- Fix: `/restart` before the first turn crashed the relaunch ("EOF while
  parsing a value") — session files are reserved empty at startup and
  resume now handles that (and missing/corrupt files, which back up to
  `.json.corrupt`) gracefully instead of failing startup. Also fixes the
  latent `rift -c` crash after quitting an unused session

## v0.9.2 — 2026-07-03

- **`/restart`**: relaunch rift and resume the exact session in place —
  the post-`/update` path that keeps your chat. True exec on Unix (same
  terminal, same PID), spawn-and-wait on Windows; `/update`'s success
  message now points at it
- The status line and `/restart` carry the addressable model name
  (provider prefix intact), so `anthropic/…` models survive a restart

## v0.9.1 — 2026-07-03

- **Markdown rendering in the transcript**: `## headings` render bold in
  the accent color, bullets become `•`, `inline code` and **bold** get
  span colors — raw markdown from the model no longer displays as plain
  text. Fence-tag coverage pinned by test (markdown/md/json/yaml/diff/…)
- Prompts now ask models to put document/file-content answers in a
  language-tagged fenced code block, so they render in the highlighted box
- Explicit content requests ("do not use any tools", "in a code block")
  no longer trip the apply-nudge, which could make the model discard the
  requested content on its retry

## v0.9.0 — 2026-07-02 · v0.8 phase complete: context engine v2

- **Hydrate-on-demand reads**: an unbounded `read` of a 500+ line source
  file returns its line-numbered outline instead of a 2000-line dump; the
  model then fetches exact ranges (offset/limit always verbatim). Measured
  on the hard tier: **−18.4% prompt tokens** suite-wide, −66% on the
  buried-bug big-module task, pass rate held (docs/BENCHMARKS.md)
- **Repo-map centrality ranking**: files ranked by import in-degree first,
  recency as tiebreak — central modules make the map even when untouched;
  falls back to pure recency when no imports resolve. Imports cached
  alongside outlines
- Traces record each tool call's file/pattern target (data for future
  retrieval tuning)
- `bench.py --runs N` (per-run pass counts + flaky-task list) and
  `RIFT_BIN` override for old-vs-new binary A/Bs

## v0.8.1 — 2026-07-02

- **Persistent outline cache**: repo_map outlines cached per repo root
  keyed by (mtime, size) — a hit skips the file read and the tree-sitter
  parse; best-effort JSON under the user data dir
- **Idle-time compaction**: the context-budget check also runs right after
  each turn, so pruning/summarizing happens while you read the reply
  instead of mid-turn while you wait
- **Hard-tier benchmark suite** (`bench/tasks2/`, `--dir tasks2`): 10
  harder tasks — multi-file symptom-not-location bugs, fix-the-failing-test
  with tamper guards, needle-in-a-haystack long-session modules, a
  cross-file signature refactor
- **First judge-accuracy results**: qwen3.6:35b judging gemma4:26b vs
  ornith:35b races went 14/14 on discriminative cases (see
  docs/BENCHMARKS.md)
- Community docs: CHANGELOG, CONTRIBUTING, issue templates
- Demo GIF in the README (reproducible VHS tape committed)
- Packaging: Homebrew formula generator + release-workflow tap
  automation, scoop manifest with autoupdate

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
