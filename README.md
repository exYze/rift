# Rift

A fast, flicker-free terminal coding agent built in Rust for **local models via Ollama's native API** — no Node, no Python, one 9.5MB binary.

**Benchmarked vs opencode** (same model, same Ollama server, wire-measured tokens): equal task success, **80% fewer prompt tokens, 2.3× faster** — see [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

Rift is a ground-up Rust answer to opencode/Crush-style agents, designed around the three failure modes that plague them with local models:

1. **Broken scrolling/streaming UX** → pre-wrapped line buffer with bottom-anchored scrolling; streaming never fights your scroll position.
2. **Context blowout** → explicit `num_ctx` on every request, silent-truncation detection via `prompt_eval_count`, token-capped tool outputs, AST-based context compaction (Compactor subsystem).
3. **Local tool-calling fragility** → native `/api/chat` protocol (not the OpenAI-compat shim), textual-tool-call recovery, hallucinated-tool-name aliasing, error-as-tool-result self-correction, doom-loop guard.

Plus **WarpDrive**: parallel agent exploration in isolated git worktrees with side-by-side diff merge.

## Usage

```sh
# TUI (default)
rift --host http://localhost:11434 --model gemma4:26b

# Headless one-shot
rift --prompt "Fix the failing test in src/lib.rs"
```

```sh
# WarpDrive: race models on a task in isolated git worktrees, then merge the winner
rift swarm "Refactor the auth middleware" --models gemma4:26b,qwen3:27b
rift merge 0-gemma4-26b --cleanup
```

Env vars: `RIFT_HOST`, `RIFT_MODEL`. Flags: `--num-ctx` (default 32768), `--max-iterations`, `-c/--continue` (resume last session).

## Slash commands (inside the TUI)

| command | what it does |
|---|---|
| `/model [name]` | list models on the server, or switch (capability-checked, num_ctx clamped) |
| `/clear` | wipe the conversation |
| `/compact` | force history compaction now |
| `/tokens` | context budget, usage estimate, estimator calibration |
| `/sessions [n]` | list saved sessions, or resume the nth |
| `/tools` · `/mcp` · `/permissions` | what the model can call, MCP server status, shell deny list |
| `/swarm <task> [--models a,b] [--explore]` | WarpDrive race without leaving the chat |
| `/merge <name> [--cleanup]` | apply a swarm candidate's patch |
| `/undo` | revert the last turn's write/edit changes |
| `/diff` | colored git diff of the working tree |
| `/init` | generate a RIFT.md project guide for agents |
| `/host [url]` | show or switch the Ollama server |
| `/think [on\|off\|auto]` | thinking mode (capability-checked) |
| `/export` | save the transcript as markdown |

## Config (`.rift.json` in the project, or `~/.config/rift/config.json`)

```json
{
  "mcp": {
    "fetch": {"command": "uvx", "args": ["mcp-server-fetch"]}
  },
  "permissions": {"bash_deny": ["docker push *"]}
}
```

MCP server tools are exposed to the model as `<server>_<tool>`. A built-in deny list (sudo, `rm -rf /`, mkfs, …) always applies to shell commands.

## Workspace layout

- `crates/rift-ollama` — native Ollama client: NDJSON streaming, tool calls, thinking, capability detection, truncation detection.
- `crates/rift-core` — agent engine: tool registry (read/write/edit/bash/ls/grep/glob), agent loop, local-model hardening.
- `crates/rift-tui` — `rift` binary: ratatui frontend + headless mode.

See `docs/PROJECT.md` for status and roadmap, `docs/RESEARCH.md` for the protocol/architecture research this is built on.
