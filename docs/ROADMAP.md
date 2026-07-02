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

- [ ] **Anthropic + OpenAI native providers** (API keys via env/config;
  never required)
- [ ] **Cross-provider WarpDrive** — the killer demo: race
  `gemma4:26b` (free, local) vs `claude-sonnet` (cloud) on the same task in
  isolated worktrees and merge whoever wins. No other TUI does this
- [ ] Cost display for metered providers (`/stats` shows $ next to tokens)
- [ ] Swarm auto-judge: optional referee model scores candidate diffs and
  recommends a winner

## v0.8 — Context engine v2

- [ ] Hydrate-on-demand: outline first, then fetch exact line ranges as the
  model asks — measure token savings vs v1 in the bench suite
- [ ] Persistent repo-map cache (invalidate by mtime) so big repos don't
  re-outline every session
- [ ] Smarter compaction triggers: compact during idle time between turns,
  not mid-turn when the user is waiting
- [ ] Benchmark suite v2: more tasks, multi-run variance, long-session
  tests that exercise compaction, published per-release

## v0.9 — Distribution + community

- [ ] Homebrew tap (`brew install exYze/tap/rift`), winget/scoop manifests
- [ ] Demo GIF/VHS tape in the README (the single highest-leverage growth
  item — people install what they can see)
- [ ] CHANGELOG.md, CONTRIBUTING.md, issue templates
- [x] CI test matrix on macOS/Linux/Windows (build + test on all three landed
  early, in 0.4.x — Linux-only CI had let a Windows bug ship)
- [ ] Publish crates to crates.io (`rift-ollama` is useful standalone)

## v1.0 — The promise

Ship when: config format stable for 6 months of releases, provider matrix
green in CI against live servers, benchmark numbers published per release,
and `rift update` has carried users through 10+ versions without a manual
reinstall. 1.0 means breaking changes now require a major version — trust,
codified.

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
