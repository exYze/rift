# Contributing to rift

Thanks for considering a contribution. rift is a small, focused project —
this document is short on ceremony and long on the few things that matter.

## Ground rules

Three principles every change must respect (from [docs/ROADMAP.md](docs/ROADMAP.md)):

1. **Local-first.** Everything works offline against your own server. Cloud
   providers are an option, never a requirement. No telemetry, no network
   calls except to the user's own model server (and the opt-in update check).
2. **Zero tool-calling errors.** New providers and features keep the
   hardening that makes local models reliable: truncation detection, textual
   tool-call recovery, alias resolution, doom-loop guard.
3. **One small binary.** No runtimes, no daemons. Binary size is tracked;
   heavy dependencies go behind feature flags (see `highlight`).

Scope guard: every feature must serve the coding-agent loop. Generic
chat-app features will be declined.

## Building and testing

```sh
cargo build                                     # debug build
cargo clippy --workspace --all-targets -- -D warnings   # CI gate: zero warnings
cargo test --workspace                          # CI gate: all tests
```

CI is the reviewer: clippy `-D warnings` plus the full test suite on
macOS, Linux, and Windows. There is no rustfmt gate — match the style of
the surrounding code instead of reformatting.

Live provider tests (optional, need a real server):

```sh
RIFT_LIVE_OLLAMA=http://localhost:11434 RIFT_LIVE_MODEL=gemma4:26b \
  cargo test -p rift-ollama --test live
# likewise RIFT_LIVE_OPENAI / RIFT_LIVE_ANTHROPIC
```

## Benchmarks

Performance claims are measured, not asserted. The harness lives in
`bench/` (`proxy.py` records wire-level tokens; `bench.py --models a,b,c`
runs the task-suite matrix). If your change plausibly affects success rate
or token use, run the suite before and after and include both numbers.

Prompt changes are held to the **evolution gate**: a change to any file in
`crates/rift-core/prompts/` merges only if it beats the incumbent on the
bench matrix — workflow in `crates/rift-core/prompts/README.md`.

## Pull requests

- One logical change per PR; explain the *why* in the description
- Add or extend tests for what you changed — especially anything in the
  provider or agent-loop hardening paths
- New provider protocols need a mock-server hardening suite (see
  `crates/rift-*/tests/hardening.rs` for the pattern) and a live suite
- Update `CHANGELOG.md` under an Unreleased heading if user-visible

## Reporting bugs

Use the issue templates. The single most useful thing you can include is a
turn trace: re-run with `--trace /tmp/rift-trace.jsonl` and attach the
relevant lines (they contain token counts, tool calls, and failure
counters — no file contents beyond a capped prompt head).
