# Rift roadmap

Where rift goes from v0.3.x. Ordered by phase; each phase is shippable on its
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

- [ ] **Plan tool + activity-pane checklist** (the to-do list)
  - New built-in `plan` tool the model calls to set/update its task list:
    `plan(set=["fix parser", "add test", "run tests"])`, `plan(done=1)`
  - Rendered pinned at the top of the activity pane: `☑ fix parser`,
    `◐ add test` (in progress), `☐ run tests`
  - System prompt nudges the model to plan first on multi-step tasks and
    check items off as it goes — also measurably helps local models stay
    on track
  - `/plan` command to view/clear it
- [ ] **Write/bash approval mode**
  - Today rift auto-executes everything. Add `--approve` mode (and config
    default) where `write`/`edit`/`bash` pause for y/n in the TUI, with a
    per-session "always allow" memory; deny list stays as the hard floor
- [ ] **Input editing basics** — cursor movement (←/→, word jumps,
  Home/End), insert anywhere, bracketed paste. Today input is
  append/backspace only; this is the biggest day-one UX gap
- [ ] **Load RIFT.md automatically** — `/init` generates it, but the agent
  doesn't read it back yet. Inject it into the system prompt when present
  (that's the whole point of the file)

## v0.5 — Command + UX expansion

- [ ] More slash commands:
  - `/retry` — re-run the last prompt (after an interruption or bad answer)
  - `/stats` — session totals: tokens, calls, tool counts, compactions
  - `/system [text]` — view or override the system prompt
  - `/temp <t>` · `/ctx <n>` — runtime knobs without restarting
  - `/worktrees` — list swarm worktrees + patches with cleanup hints
  - `/save <name>` · `/sessions` named sessions (not just timestamps)
  - `/quit`
- [ ] **@-file mentions** — `@src/main.rs` in a prompt auto-attaches an
  outline (not the raw file — stay token-stingy), with palette completion
  like the `/` popup
- [ ] **Syntax highlighting** in code blocks (syntect) — watch binary size;
  feature-flag if it pushes past ~12MB
- [ ] **Streaming diff pane** in the main TUI: live colored diff of what the
  agent is changing this turn (the GhostWriter concept, finally)
- [ ] Themes (a few built-in palettes; the activity-pane contrast issue
  showed colors need to be configurable)

## v0.6 — Provider abstraction (beyond Ollama, part 1: local)

The architectural step: extract a `Provider` trait so `OllamaClient` becomes
one implementation, not the foundation.

- [ ] `Provider` trait: `chat_stream`, `capabilities(model)`, `list_models`
  — everything the agent loop and swarm already consume
- [ ] **OpenAI-compatible provider** — one implementation unlocks vLLM,
  LM Studio, llama.cpp server, LiteLLM, OpenRouter, and Ollama's own
  compat endpoint
  - This is exactly the shim whose bugs rift was built to escape, so it
    ships with a **per-provider hardening test suite**: tool-call
    correlation, context-overflow behavior, streaming quirks — run against
    real servers before a provider is called supported
- [ ] Config + model addressing: `ollama/gemma4:26b`,
  `openai-compat/qwen3` with per-provider base URLs and keys in
  `.rift.json`; `/model` and `/host` grow provider awareness
- [ ] Token accounting per provider (usage fields differ; the Compactor's
  calibration loop already adapts, keep it provider-generic)

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
- [ ] CI test matrix on macOS/Linux/Windows (today: tests on Linux only)
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
