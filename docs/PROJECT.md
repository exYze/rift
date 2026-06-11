# Rift — project state

Updated: 2026-06-11 (iteration 8 — slash commands; renamed GhostWriter → Rift, published to github.com/exYze/rift)

## Vision

One Rust TUI combining three concepts: **GhostWriter** (flawless multi-pane TUI, sub-10MB binary), **Compactor** (AST-skeleton context minimization), **WarpDrive** (parallel agents in git worktrees with side-by-side diff merge). Backend: Ollama native API at `http://localhost:11434`, test model `gemma4:26b`. Goal: beat current agent-TUI baselines on token efficiency by ≥10% (benchmark harness TBD).

## Status — iteration 1 (done)

- [x] Research: Ollama tool-calling protocol (verified live), opencode architecture + failure-mode catalog → `docs/RESEARCH.md`
- [x] `gw-ollama`: native client — NDJSON streaming, tool calls (id preserved), thinking, /api/show capabilities, truncation detection, textual-tool-call fallback parser (unit tested)
- [x] `gw-core`: tool registry (read/write/edit/bash/ls/grep/glob with context-protecting caps), agent loop with alias resolution, error-as-tool-result, doom-loop guard
- [x] `gw-tui`: `gw` binary — headless `--prompt` mode + ratatui TUI (pre-wrapped buffer, bottom-anchored scroll, streaming/scroll arbitration, mouse wheel)
- [x] Live E2E test passed: gemma4:26b found/fixed/verified a bug in 6 iterations, zero tool errors, 62 tok/s

## Roadmap (work top-down; update this file each iteration)

### Iteration 2 — TUI polish + sessions (done)
- [x] Multi-pane layout: transcript + activity (tool log) panes, independently scrollable, Tab focus, Ctrl+L toggle, mouse-wheel routed to pane under cursor
- [x] Esc-to-cancel via CancellationToken (clean mid-stream/mid-tool abort; pending tool calls get "cancelled" results so history stays valid)
- [x] Multi-line input (Ctrl+J / Alt+Enter), input history (Alt+Up/Down), growing input pane
- [x] Session persistence: atomic JSON saves after each turn under ~/.local/share/ghostwriter/sessions/; `-c/--continue` resumes latest, `--resume <path>` specific; verified memory across processes
- [x] Fenced code blocks styled separately, hard-cut (not re-wrapped) so indentation stays exact
- [x] Draw-on-change only (no constant 60fps redraw)
- [x] PTY-driven E2E test via expect: typed prompt → live gemma4:26b response rendered → clean Ctrl+C exit (note: expect PTYs default to 0x0; test sets stty rows/cols)
- [ ] User should try it interactively: `./target/release/gw`

### Iteration 3 — context engine (Compactor) (done)
- [x] Token budget enforced client-side before every request: usable = num_ctx − 4096 output reserve − 1024 safety; chars/4 estimator continuously calibrated (EMA) against server prompt_eval_count
- [x] Layered compaction: stage 1 prunes bulky tool outputs + stale thinking outside last 2 user turns; stage 2 LLM-summarizes older history (5-heading scheme), keeps current turn verbatim — verified live (15.5k → 6.2k → 1.3k tok)
- [x] Old turns' thinking stripped from requests (kept in stored history)
- [x] tree-sitter outlines: `outline` tool (signatures-only skeletons, .rs/.py/.js/.jsx/.ts/.tsx/.go) — live-verified ~2.6k tok turn vs ~5.8k tok raw read
- [x] `repo_map` tool: outlines of most-recently-modified source files, 24KB cap
- [x] BUG FOUND+FIXED in gw-ollama: non-streaming responses (no trailing newline) were never parsed — NDJSON buffer now flushed at stream end. Symptom was an empty compaction summary.
- Note: tree-sitter grammars grew the release binary 4.2M → 9.2M (still <10MB; adding more grammars will exceed — consider feature flags then)

### Iteration 4 — WarpDrive worktrees (done)
- [x] Swarm manager (gw-core/src/swarm.rs): detached worktrees under .gw/worktrees/, `.gw/` auto-added to .git/info/exclude, stale-worktree recovery, patch capture (staged diff incl. new files) to .gw/patches/
- [x] `run_swarm`: N candidates (model + temperature) in parallel, per-candidate capability preflight (think/num_ctx clamp), per-candidate failure isolation, events tagged by candidate index
- [x] CLI: `gw swarm "<task>" --models a,b [--explore]` (explore adds temp-0.8 variants), colored per-candidate streaming, comparison report; `gw merge <name> [--cleanup]` applies patch via git apply --3way
- [x] Live race verified (gemma4:12b vs gemma4:26b): root tree untouched during run, patches captured, merge + cleanup work. 26b correctly refused a fake bug and added a regression test; 12b leaked `<|tool_response>` → added template-token sanitizer (strip `<\|[^<>]{0,48}>`, unit tested)
- [ ] TUI swarm pane: side-by-side diff viewer + one-key merge → moved to iteration 5

### Iteration 5 — MCP + permissions (done)
- [x] MCP stdio client (gw-core/src/mcp.rs): zero-dep JSON-RPC 2.0 — initialize handshake (protocol 2025-06-18), notifications/initialized, tools/list, tools/call (text content blocks, isError); server-initiated requests answered method-not-found; 60s request timeout
- [x] MCP tools registered into the normal registry as `<server>_<tool>`; live-verified: gemma4:26b called test_get_secret natively in 2 iterations/1.7s (vs 22 iterations/147s improvising via bash before registration — great demo of why MCP matters)
- [x] Config (gw-core/src/config.rs): `.ghostwriter.json` (project) → `~/.config/ghostwriter/config.json` (user); `mcp` servers + `permissions.bash_deny`
- [x] Bash permission policy: built-in deny list (sudo, rm -rf /, shutdown, mkfs, dd-to-device, fork bomb, …) + user globs, whitespace-normalized matching; live-verified both layers; model reports the block gracefully
- [x] Tool trait names now &str (dynamic), ToolRegistry::register for runtime tools
- [x] Test fixture: scripts/test_mcp_server.py (dependency-free MCP stdio server)
- [ ] TUI: swarm view (candidate tabs, side-by-side diff pane, one-key merge) → iteration 6
- Lesson: verify binary mtime after cargo build in scripted runs — one build silently no-opped and I tested a stale binary

### Iteration 6 — swarm TUI (done)
- [x] `gw swarm` now opens an interactive TUI by default on a terminal (--no-tui for plain streaming): candidate tabs (1-9/←/→), per-candidate activity log + colored diff pane (independently scrollable, mouse-routed), Esc cancels the whole race, `m` one-key merge, post-exit summary
- [x] run_swarm takes a shared CancellationToken (Esc aborts all candidates cleanly)
- [x] Pane primitives generalized: gap-less push_line for dense logs/diffs, hard-cut (no re-wrap) for code AND diff lines, diff Kind variants (add/del/hunk/meta)
- [x] BUG FOUND+FIXED: scroll-anchor adjustment overflowed (usize::MAX top-anchor + saturating_add) — caught by the PTY E2E test, would have been a silent wraparound scroll bug in release
- [x] Live PTY E2E: race → diff rendered → m merged patch into root (verified file fixed) → clean exit
- Note for PTY tests: expect's Tcl eats [brackets] in spawn args; ratatui cell-diffing means long strings aren't contiguous in the output stream — match short tokens

### Iteration 7 — benchmarks (done)
- [x] Harness: bench/proxy.py (wire-level token recording for any agent), bench/bench.py (fresh git repo per run, verify.sh scoring, warmup exclusion), 5-task suite in bench/tasks/
- [x] Baseline: opencode 1.14.48, same model/server/prompts, isolated per-task $HOME
- [x] RESULT: both 5/5; GhostWriter used **80.4% fewer prompt tokens** (54,431 vs 277,110) and was **2.3× faster** (128.8s vs 300.4s) — target was ≥10%
- [x] docs/BENCHMARKS.md published with methodology + honest caveats
- Hard-won harness lessons: opencode trusts $PWD over real cwd (subprocess must set PWD or it roots in the wrong dir — it edited our fixtures!); silent exit-0 on ProviderModelNotFoundError; global-config provider merge collisions → per-task throwaway HOME is the only reliable isolation

### Iteration 8 — slash commands (done)
- [x] 16 in-TUI commands intercepted before the model sees them: /help /model /clear /compact /tokens /sessions /tools /mcp /permissions /swarm /merge /undo /diff /init /host /think /export
- [x] Architecture: commands run inside the agent task (owns the Agent); results flow back over a dedicated UiEffect channel (Out/Log/Diff/Clear/Seed/Model/Done) so they never race AgentEvents; Esc cancels long commands (/compact, /swarm) via the same CancellationToken as turns
- [x] /undo backed by an edit journal in ToolCtx: write/edit snapshot the first prior state per (turn, path); restores files or deletes created ones; bash changes explicitly not tracked (unit tested)
- [x] /swarm runs a full WarpDrive race inside the chat TUI — progress streams to the activity pane, results + /merge hint to the transcript
- [x] /model /host /think all capability-preflighted via /api/show (same hardening as startup)
- [x] Live PTY E2E: /help, /tokens, /model (13 real models listed), /think, unknown-command error — all rendered, clean exit

### Iteration 9 — command palette + release pipeline (done)
- [x] v0.1.0 published: GH Actions release workflow (5 targets: mac arm64/x64, linux arm64/x64 static-musl, win x64), curl-able install.sh, CI (test+clippy); reqwest switched to rustls for static musl builds; Intel mac cross-compiled from arm64 runner (macos-13 runners are dead — queue forever)
- [x] Slash-command palette: typing `/` pops an overlay above the input listing all commands, filtered live by prefix; ↑↓ select, Tab completes (adds trailing space for arg commands), Enter runs selection, Esc dismisses (re-arms on next keystroke); COMMANDS table in commands.rs is the single source of truth for palette + /help
- [x] PTY-verified: open on /, filter on /c, ↓+Enter ran /compact, /to+Tab+Enter ran /tokens
- Lesson: in expect scripts a lone ESC byte gets paired by crossterm with the next byte as an Alt-chord — don't test Esc-then-type sequences in the same PTY breath

### Possible future work (roadmap complete; not scheduled)
- Streaming-diff pane in main chat TUI; session picker; syntax highlighting via syntect (binary budget!)
- Compactor v2: hydrate-on-demand line ranges, persistent repo map cache
- Swarm: candidate-vs-candidate auto-judge; partial hunk merge
- Benchmark suite v2: more tasks, multi-run variance, long-session compaction tests
- Publish: git commits, CI, README badges, demo GIF

## Decisions log

- Native /api/chat over OpenAI-compat shim (root cause of most opencode-Ollama bugs).
- Tool results correlate by `tool_name` (model's requested name, pre-aliasing) per native protocol.
- `think` left at server default for thinking models; forced false only when capability absent.
- num_ctx default 32768, clamped to model max from /api/show.
- No git commits made yet — user hasn't asked; ask or just init? repo was `git init`ed in iteration 1, no commits.
