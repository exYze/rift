# Rift roadmap

Where rift goes from v0.4.x. Ordered by phase; each phase is shippable on its
own. Principles that don't change along the way:

- **Local-first.** Everything works offline against your own server. Cloud
  providers become an *option*, never a requirement.
- **Zero tool-calling errors.** Every new provider or feature keeps the
  hardening that made local models reliable (truncation detection, textual
  tool-call recovery, alias resolution, doom-loop guard).
- **One small binary.** No runtimes, no daemons. Watch the size budget
  (currently 9.5MB; tree-sitter grammars are the main pressure).

---

## v0.4 — Agent plan view + trust

The agent should *show its plan* and *ask before dangerous things*.

- [x] **Interactive pickers** (shipped 0.4.0) — `/model` and `/sessions` open
  ↑↓/Enter list overlays; direct `/model <name>` still works
- [x] **Elicitation** (shipped 0.4.0) — `ask_user` tool: the model asks
  clarifying questions mid-turn; choices reuse the picker, free-text answers
  go through the input box; headless/swarm runs stay autonomous

- [x] **Plan tool + activity-pane checklist** (the to-do list)
  - New built-in `plan` tool the model calls to set/update its task list:
    `plan(set=["fix parser", "add test", "run tests"])`, `plan(done=1)`
  - Rendered pinned at the top of the activity pane: `☑ fix parser`,
    `◐ add test` (in progress), `☐ run tests`
  - System prompt nudges the model to plan first on multi-step tasks and
    check items off as it goes — also measurably helps local models stay
    on track
  - `/plan` command to view/clear it
- [x] **Write/bash approval mode**
  - Today rift auto-executes everything. Add `--approve` mode (and config
    default) where `write`/`edit`/`bash` pause for y/n in the TUI, with a
    per-session "always allow" memory; deny list stays as the hard floor
- [x] **Input editing basics** — cursor movement (←/→, word jumps,
  Home/End), insert anywhere, bracketed paste. Today input is
  append/backspace only; this is the biggest day-one UX gap
- [x] **Load RIFT.md automatically** — `/init` generates it, but the agent
  doesn't read it back yet. Inject it into the system prompt when present
  (that's the whole point of the file). Also loads AGENTS.md and CLAUDE.md
  (the cross-tool standards), concatenated and capped
- [x] **Skills** (Agent Skills standard, pi/Claude-style) — `.rift/skills/`
  + `~/.config/rift/skills/` SKILL.md files with frontmatter; listed to the
  model by name+description, bodies loaded on demand via the `skill` tool;
  user-invocable as `/skill:<name> [task]` with palette completion; /skills
- [x] **/config + /approve** — view config, edit in $EDITOR with hot-reload
  of permissions, session-level approval toggle

## v0.5 — Command + UX expansion

- [x] More slash commands (shipped 0.5.0):
  - `/retry` — re-run the last prompt (after an interruption or bad answer)
  - `/stats` — session totals: tokens, calls, tool counts, compactions
  - `/system [text]` — view or override the system prompt
  - `/temp <t>` · `/ctx <n>` — runtime knobs without restarting
  - `/worktrees` — list swarm worktrees + patches with cleanup hints
  - `/save <name>` · `/sessions` named sessions (not just timestamps)
  - `/quit`
- [x] **@-file mentions** (shipped 0.6.x) — `@src/main.rs` in a prompt
  auto-attaches an outline (not the raw file — stay token-stingy), with
  palette completion like the `/` popup; unsupported file types attach a
  capped head instead
- [x] **Syntax highlighting** (shipped 0.6.x) — syntect in fenced code
  blocks, stateful per block; pushed the binary to ~12MB so it sits behind a
  default-on `highlight` feature (`--no-default-features` builds lean)
- [x] **Streaming diff pane** (shipped 0.6.x) — Ctrl+D flips the activity
  pane to a live working-tree diff, refreshed after every write/edit tool
  result and at turn end
- [x] Themes (shipped 0.6.x) — built-in `dark`/`light`/`mono` palettes;
  `"theme"` in config, `/theme <name>` at runtime; syntect theme follows

## v0.6 — Provider abstraction (beyond Ollama, part 1: local)

The architectural step: extract a `Provider` trait so `OllamaClient` becomes
one implementation, not the foundation.

- [x] `Provider` trait (shipped 0.6.0): `chat_stream`, `show`, `tags` —
  everything the agent loop and swarm already consume
- [x] **OpenAI-compatible provider** (shipped 0.6.0, hardened 0.6.1/0.6.2) —
  unlocks vLLM, LM Studio, llama.cpp server, LiteLLM, OpenRouter, and
  Ollama's own compat endpoint; 0.6.x added timeouts, transport retries,
  mid-stream error surfacing, SSE tail flushing, and tool-call id repair
- [x] Config + model addressing (shipped 0.6.0): `openrouter/qwen3` routes
  through per-provider base URLs and keys in config; project config merges
  over user config with tighten-only permissions (0.6.2)
- [x] Token accounting per provider (shipped 0.6.0; usage fields normalized
  into the shared `ChatStats`)
- [x] **Per-provider hardening test suite** (shipped 0.6.4) — two layers:
  a deterministic mock-server suite in CI (SSE/NDJSON framing across split
  reads, missing `[DONE]`, mid-stream error events, tool-call accumulation
  and id handling, `stream_options` rejection recovery, truncated-argument
  errors, front-truncation detection, proxy error statuses), plus an
  env-gated live suite (`RIFT_LIVE_OLLAMA` / `RIFT_LIVE_OPENAI` +
  `RIFT_LIVE_MODEL`) run against real servers before a provider is called
  supported. v1.0's "provider matrix green in CI" builds on the live layer

## v0.7 — Cloud providers + cross-provider swarm

- [x] **Anthropic + OpenAI native providers** (shipped 0.7 phase 1) —
  rift-anthropic speaks the Messages API natively (SSE content-block events,
  tool_use/tool_result blocks, thinking with signature round-trip via
  `Message::provider_data`, adaptive thinking, /v1/models discovery), with
  its own mock + live hardening suites (`RIFT_LIVE_ANTHROPIC`). OpenAI rides
  the existing rift-openai protocol. `anthropic/<model>` and `openai/<model>`
  work with just ANTHROPIC_API_KEY / OPENAI_API_KEY in the env — no config
  entry needed; `kind: "anthropic"` on a provider entry selects the protocol
  for custom endpoints. API keys via env/config; never required
- [x] **Cross-provider WarpDrive** (shipped 0.8.0) — race `gemma4:26b`
  (free, local) vs `anthropic/claude-sonnet-4-6` (cloud) on the same task
  in isolated worktrees and merge whoever wins: each candidate's model
  string resolves through a provider factory, so one race spans providers.
  No other TUI does this
- [x] Cost display for metered providers (shipped 0.8.0) — `/stats` and the
  headless summary show estimated $; billed input tracked as summed
  per-call prompt tokens (not last-of-turn); built-in Anthropic rates,
  config `pricing` map for everything else
- [x] Swarm auto-judge (shipped 0.8.0) — `--judge <model>` scores every
  candidate's diff and recommends a winner (TUI: verdict in the winner's
  log, tab auto-selected; headless: machine-parseable `JUDGE: winner=`
  line). `bench/judge_bench.py` measures judge accuracy against verify.sh
  ground truth — discriminative-case accuracy is the headline number
- [x] **Turn traces + failure counters** — the hardening layer already
  *detects* the interesting failures (textual tool-call recovery, alias
  resolution, doom-loop guard, truncation detection, fuzzy edit misses)
  and then discards the signal. Count them per turn in `TurnStats`, and add
  an opt-in JSONL trace export (`--trace <path>` / config) recording model,
  tokens, tool calls, failure flags, and outcome per turn. Local files
  only, never on by default — this is the data source every optimization
  item below consumes
- [x] **Bench model matrix** — `bench.py --models a,b,c` runs the suite
  across models and diffs pass rate / prompt tokens / wall time per model;
  bench runs always emit traces. Every model-specific optimization needs
  this as its fitness function

## v0.8 — Context engine v2

Method for this phase: design offline, implement deterministically. Feed
traces + compaction logs + bench results to a frontier model acting as
performance engineer; implement its packing strategy as plain Rust; gate
every change on the bench matrix. The frontier model is never in the
inference path.

- [x] Hydrate-on-demand (shipped 0.9.0) — an unbounded read of a 500+ line
  source file returns its line-numbered outline; the model fetches exact
  ranges. Measured on the hard tier: −18.4% prompt tokens suite-wide,
  −66% on the buried-bug big-module task, pass rate held (BENCHMARKS.md)
- [x] Persistent repo-map cache (shipped 0.8.x) — outlines cached per repo
  root keyed by (mtime, size); a hit skips the read and the tree-sitter
  parse. Best-effort JSON under the user data dir, LRU-capped
- [x] Smarter compaction triggers (shipped 0.8.x) — the budget check also
  runs right after each turn completes, so pruning/summarizing happens
  while the user reads the reply instead of mid-turn while they wait
- [x] Benchmark suite v2 (shipped 0.9.0) — hard tier (`bench/tasks2/`,
  `--dir tasks2`): multi-file symptom-not-location bugs, tamper-guarded
  fix-the-tests, long-session big-module tasks; `--runs N` reports
  per-run pass counts + flaky tasks; numbers published per release in
  BENCHMARKS.md/CHANGELOG
- [x] Repo-map ranking heuristics v2 (shipped 0.9.0) — import-centrality
  primary, recency tiebreak (cheap textual import extraction, cached with
  outlines; no embeddings, no index); degrades to pure recency when no
  imports resolve. Traces now record tool-call targets for future tuning

## v0.8.5 — Model targets

One hardcoded system prompt goes to every model today, but open models
differ dramatically in what prompting and formatting they want. Treat each
model family as a compiler target, with the bench matrix as the fitness
function.

- [x] **Prompt-target machinery** (landed early, alongside v0.7 phase 2) —
  `crates/rift-core/prompts/<family>.md` (markdown + frontmatter, the
  SKILL.md idiom: multi-line-friendly, zero new deps) embedded at compile
  time (`include_str!` — keeps the one-binary promise); `match:` substrings
  select a target by model name; `~/.config/rift/prompts/` overrides for
  experimenting without recompiling (user-level only — a cloned repo must
  never be able to replace the system prompt). Swarm candidates each get
  their own family prompt, so cross-provider races compare tuned targets
- [ ] **Family targets** — actual qwen/deepseek/glm/mistral files, each
  landed through the evolution gate; workflow in
  `crates/rift-core/prompts/README.md`. First candidate `gemma.md` is in
  provisionally: the first 3-model matrix run (2026-07) showed gemma4:26b
  at 40/50 with 8 chat-only failures and 3× the tokens of ornith/qwen
  (both 50/50 on the same default prompt) — validate vs that baseline on
  the next matrix run
- [ ] **Prompt evolution gate** — prompt files are versioned; a change
  merges only if it beats the incumbent on the bench matrix. Prompts are
  code: benchmark → review → revise → benchmark → merge
- [ ] **Tool-schema A/B on the matrix** — richer parameter schemas may help
  one family and hurt another (more params = more places for a small model
  to hallucinate); measure per family, don't assume

## v0.9 — Distribution + community

- [x] Homebrew tap (shipped 0.8.1) — `brew tap exYze/tap && brew install
  rift`, formula auto-regenerated by the release workflow from published
  checksums; scoop manifest installable from its raw URL. Still open:
  winget submission; homebrew-core once the repo clears the notability bar
- [x] Demo GIF/VHS tape in the README (shipped 0.8.1) — a real recorded
  session (VHS tape committed for reproducibility)
- [x] CHANGELOG.md, CONTRIBUTING.md, issue templates (shipped 0.8.1)
- [x] CI test matrix on macOS/Linux/Windows (build + test on all three landed
  early, in 0.4.x — Linux-only CI had let a Windows bug ship)
- [ ] Publish crates to crates.io (`rift-ollama` is useful standalone)

## v1.0 — The promise

Ship when: config format stable for 6 months of releases, provider matrix
green in CI against live servers, benchmark numbers published per release,
and `rift update` has carried users through 10+ versions without a manual
reinstall. 1.0 means breaking changes now require a major version — trust,
codified.

**Shipped 2026-07-06.** The 1.x line since (highlights): concurrent
sub-agents (1.0), model roles (1.1), vision attachments (1.2), post-edit
hooks (1.3), remote MCP (1.4), `web_search` (1.5), VS Code sidebar chat on
`--serve` (1.6), granular permission rules + inline diff review (1.7), and
merge-to-release CI. Full detail in CHANGELOG.md.

## v1.8 — Model targets, finished

Absorbs the open v0.8.5 items — each model family treated as a compiler
target, the bench matrix as the fitness function.

- [ ] **Family targets**: qwen/deepseek/glm/mistral prompt files landed
  through the evolution gate; validate the provisional `gemma.md` against
  the default-prompt baseline on the next matrix run
- [ ] **Prompt evolution gate**: prompt files are versioned; a change
  merges only if it beats the incumbent on the bench matrix
- [ ] **Tool-schema A/B on the matrix**: measure richer vs leaner
  parameter schemas per family — more params = more places for a small
  model to hallucinate; don't assume
- [ ] Per-family bench numbers published in BENCHMARKS.md

## v1.9 — Distribution, finished

Closes the v0.9 tail so every mainstream install path works.

- [ ] winget submission
- [ ] Publish crates to crates.io (`rift-ollama` is useful standalone)
- [ ] homebrew-core PR once the repo clears the notability bar
- [ ] Quick-wins batch: `/copy` palette completion, mouse-wheel scroll on
  the palette popup, `--version` update nudge, session file size cap

## v1.10 — Serve protocol v1

The bridge to the platform: the `--serve` surface becomes something third
parties can build on without fear.

- [ ] Versioned protocol (`protocol_version` in hello) with documented
  event/command names — docs/SERVE.md is the contract
- [ ] Compatibility tests: a conformance suite that pins the wire format;
  the VS Code extension runs against it and doubles as the reference
- [ ] Integration guide + minimal reference client for Neovim/JetBrains
  plugin authors

## v1.11 — Platform preview

Everything 2.0 will stabilize ships here first, behind flags, so the
breaking release is a promotion — not a surprise.

- [ ] **Experimental plugin API** (`.rift/plugins/`, user + project, trust-
  gated like hooks/MCP): rift-native slash commands, tools, themes, and
  prompt targets from a manifest — MCP covers external tools; plugins
  cover extending rift itself
- [ ] Config schema v2 draft + `rift config migrate --dry-run`
- [ ] Deprecation warnings on every path 2.0 changes, with the exact
  replacement named

## v2.0 — The agent platform

Breaking changes, bundled once, and only these:

- [ ] Plugin API stable: tools, slash commands, hooks, themes, prompt
  targets — semver'd
- [ ] Config schema v2 with one-shot automatic migration (old configs
  keep working through the migrator; no hand-editing)
- [ ] Serve protocol v1 frozen: editor integrations built on it keep
  working across every 2.x release
- [ ] The 1.0 stability promise resets for 2.x: config + protocol stable,
  provider matrix green, benchmarks per release

---

## Engineering process (ongoing, not versioned)

Frontier models (Fable/Opus-class) act as an **offline performance
engineer** — they profile, recommend, and leave. Never in the inference
path, never a runtime dependency, so the local-first promise holds.

- **Per-release architectural review**: source tree + bench deltas +
  PROJECT.md in; prioritized engineering tasks out
- **Failure-cluster analysis** as traces accumulate: cluster failure
  counters by model/task/tool, turn root causes into roadmap items —
  improving rift itself, not just prompts
- **Offline design, deterministic implementation**: prompt, packing, and
  ranking strategies are designed against trace data, land as plain Rust
  (or frozen prompt files), and must win on the bench matrix to merge

---

## Quick wins (grab whenever)

- `/copy` palette completion for `all`/`log` arguments
- Mouse-wheel scroll on the palette popup
- `rift --version` check against the update cache (nudge in headless too)
- Session file size cap (compact stored history past N MB)
- `RIFT.md` template improvements as real-world usage accumulates

## Known risks

| risk | mitigation |
|---|---|
| Provider shims reintroduce the bugs rift exists to fix | per-provider hardening tests against live servers; capability matrix gates "supported" status |
| Binary size creep (tree-sitter, syntect) | track size in CI; feature-flag heavy deps |
| Scope creep toward generic chat app | every feature must serve the coding-agent loop; say no to the rest |
| Solo-maintainer bus factor | CI is the reviewer: clippy -D warnings, tests, live PTY smoke tests |
