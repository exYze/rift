# Rift

A fast, flicker-free terminal coding agent built in Rust for **local models via Ollama's native API** — no Node, no Python, one 9.5MB binary.

![rift demo — fixing a bug with gemma4:26b on a local Ollama server](docs/assets/demo.gif)

**Benchmarked vs opencode** on a 50-task suite (same model, same Ollama server, wire-measured tokens): **more tasks solved (44 vs 42), 57% fewer prompt tokens, 3.4× faster** — see [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

**Model matrix** (July 2026, v0.7.1): the same 50-task suite across three local models — **ornith:35b 50/50 and qwen3.6:35b 50/50** at ~520k prompt tokens each, gemma4:26b 40/50 — with per-turn traces and failure counters recorded for every run. The traces pinpointed gemma's chat-only failure mode and produced the first per-model prompt target ([details](docs/BENCHMARKS.md)).

![rift vs opencode — 50-task suite](docs/assets/benchmark-50.svg)

Rift is a ground-up Rust answer to opencode/Crush-style agents, designed around the three failure modes that plague them with local models:

1. **Broken scrolling/streaming UX** → pre-wrapped line buffer with bottom-anchored scrolling; streaming never fights your scroll position.
2. **Context blowout** → explicit `num_ctx` on every request, silent-truncation detection via `prompt_eval_count`, token-capped tool outputs, AST-based context compaction (Compactor subsystem).
3. **Local tool-calling fragility** → native `/api/chat` protocol (not the OpenAI-compat shim), textual-tool-call recovery, hallucinated-tool-name aliasing, error-as-tool-result self-correction, doom-loop guard.

Plus **WarpDrive**: parallel agent exploration in isolated git worktrees with side-by-side diff merge.

## Roadmap

![rift roadmap](docs/assets/roadmap.svg)

Full details and rationale in [docs/ROADMAP.md](docs/ROADMAP.md).

## Install

**Homebrew** (macOS / Linux):

```sh
brew tap exYze/tap && brew install rift
```

(Newer Homebrew asks once to trust third-party taps: `brew trust exyze/tap`.)

**macOS / Linux** (one line, no dependencies):

```sh
curl -fsSL https://raw.githubusercontent.com/exYze/rift/master/install.sh | sh
```

**Windows** (PowerShell — downloads, verifies the checksum, and adds to PATH):

```powershell
irm https://raw.githubusercontent.com/exYze/rift/master/install.ps1 | iex
```

**Windows via scoop** (installs straight from the manifest, auto-updates):

```powershell
scoop install https://raw.githubusercontent.com/exYze/rift/master/packaging/scoop/rift.json
```

**From source** (any platform with Rust):

```sh
cargo install --git https://github.com/exYze/rift rift-tui
```

Pre-built binaries: macOS (Apple Silicon + Intel), Linux (x64 + arm64, fully static — no glibc requirements), Windows x64. All on the [releases page](https://github.com/exYze/rift/releases) with SHA-256 checksums.

## Updating

```sh
rift update        # or /update inside the TUI
```

rift checks for new releases on startup (at most once per 24h, cached, silent when offline) and shows a one-line notice when one exists. Set `RIFT_NO_UPDATE_CHECK=1` to disable the check entirely — no other network calls are ever made except to your own Ollama server.

## Usage

```sh
# TUI — host/model come from your config (see Config below), or pass them explicitly
rift

# ...or override per run
rift --host http://localhost:11434 --model gemma4:26b

# Headless one-shot
rift --prompt "Fix the failing test in src/lib.rs"
```

```sh
# WarpDrive: race models on a task in isolated git worktrees, then merge the winner
rift swarm "Refactor the auth middleware" --models gemma4:26b,anthropic/claude-sonnet-4-6
rift swarm "Fix the failing test" --models gemma4:26b,qwen3.6:35b --judge ornith:35b
rift merge 0-gemma4-26b --cleanup
```

Env vars: `RIFT_HOST`, `RIFT_MODEL`. Flags: `--num-ctx` (default 32768), `--max-iterations`, `-c/--continue` (resume last session), `--trace <file>` (append one JSON line per turn — tokens, tool calls, failure counters — for offline analysis; also `RIFT_TRACE`).

## Slash commands (inside the TUI)

| command | what it does |
|---|---|
| `/model [name]` | interactive model picker (↑↓/Enter), or switch directly by name |
| `/clear` | wipe the conversation |
| `/config [edit]` | show or edit `.rift.json` in `$EDITOR` (permissions hot-reload) |
| `/approve [on\|off]` | toggle approval mode without touching the config |
| `/copy [all\|log]` | copy the last reply, whole transcript, or activity log to the clipboard |
| `/compact` | force history compaction now |
| `/tokens` | context budget, usage estimate, estimator calibration |
| `/sessions [n]` | interactive session picker, or resume the nth directly |
| `/skills` · `/skill:<name> [task]` | list packaged skills, or run one |
| `/skills new <desc>` · `/mcp new <desc>` | the agent builds its own extensions: writes a skill file, or writes + self-tests a local MCP server and registers it (trust-gated) — `/restart` loads them |
| `/plan [clear]` | the agent's task checklist (also pinned live in the activity pane) |
| `/tools` · `/mcp` · `/permissions` | what the model can call, MCP server status, deny list + approval state |
| `/swarm <task> [--models a,b] [--judge m] [--explore]` | WarpDrive race without leaving the chat — models may span providers; the optional judge scores the diffs and recommends a winner |
| `/merge <name> [--cleanup]` | apply a swarm candidate's patch |
| `/undo` | revert the last turn's write/edit changes |
| `/diff` | colored git diff of the working tree |
| `/init` | generate a RIFT.md project guide for agents |
| `/restart` | relaunch rift and resume this session — pick up a fresh `/update` without losing your chat |
| `/host [url]` | show or switch the Ollama server |
| `/think [on\|off\|auto]` | thinking mode (capability-checked) |
| `/export` | save the transcript as markdown |

## Config (`.rift.json` in the project, or `~/.config/rift/config.json`)

```json
{
  "host": "http://localhost:11434",
  "model": "gemma4:26b",
  "providers": {
    "openrouter": {
      "base_url": "https://openrouter.ai/api/v1",
      "api_key_env": "OPENROUTER_API_KEY"
    }
  },
  "mcp": {
    "fetch": {"command": "uvx", "args": ["mcp-server-fetch"]}
  },
  "permissions": {"bash_deny": ["docker push *"], "approve": false}
}
```

Copy [`.rift.json.example`](.rift.json.example) to `.rift.json` (project — it's gitignored, so a private host stays out of git) or `~/.config/rift/config.json` (user-wide), then edit. Set `host` and `model` once and you can start the TUI with a bare `rift` — no flags needed; they're the startup defaults (a `--host`/`--model` flag or `RIFT_HOST`/`RIFT_MODEL` env var still overrides them). Other optional keys mirror the flags: `num_ctx`, `temperature`, `max_iterations`. For metered providers, `/stats` shows an estimated cost — Anthropic model rates are built in; add a `"pricing"` map (`{"gpt-5": {"input": 1.25, "output": 10.0}}`, $ per million tokens, matched by model-name substring) for anything else. Set `"approve": true` (or launch with `--approve`) to pause for a y/n picker before every write/edit/shell action, with per-session "always allow". Project context files (`RIFT.md`, `AGENTS.md`, `CLAUDE.md`) at the repo root are loaded into the system prompt automatically (`/init` writes a RIFT.md for you). On multi-step tasks the agent maintains a visible task checklist, pinned at the top of the activity pane.

### Model providers

By default `model` names an Ollama model on `host`. To reach an **OpenAI-compatible** endpoint (OpenRouter, vLLM, LM Studio, llama.cpp, LiteLLM, or Ollama's own `/v1`), declare it under `providers` and address a model as `provider/model` — the part before the first `/` selects the provider, the rest is sent as the model name:

```
rift --model openrouter/qwen/qwen3-30b-a3b       # one-off
```
```json
{ "model": "openrouter/qwen/qwen3-30b-a3b", "providers": { "openrouter": { "base_url": "https://openrouter.ai/api/v1", "api_key_env": "OPENROUTER_API_KEY" } } }
```

Each provider takes a `base_url` (a `/v1` suffix is added if you omit it) and, if the endpoint needs auth, either `api_key_env` (name of an environment variable to read — keeps the secret out of the file) or a literal `api_key`. `/model provider/model` switches providers live within a session; a bare model name always routes to the Ollama `host`.

## Skills

Package reusable instructions as [Agent Skills](https://agentskills.io)-style `SKILL.md` files:

```
.rift/skills/<name>/SKILL.md        # project (commit them)
~/.config/rift/skills/<name>.md     # user-wide
```

```markdown
---
name: release-check
description: checklist to verify the project is ready for release
---
1. Run the test suite ...
```

Skills are listed to the model by name + description only (progressive disclosure — bodies stay out of context); the model loads one with its `skill` tool when relevant, or you invoke one directly with `/skill:<name> [task]` (they autocomplete in the `/` palette). `/skills` lists what's available.

MCP server tools are exposed to the model as `<server>_<tool>`. A built-in deny list (sudo, `rm -rf /`, mkfs, …) always applies to shell commands.

## Elicitation

In interactive sessions the model gets an `ask_user` tool: when a task is ambiguous it pauses and asks you a clarifying question instead of guessing — multiple-choice questions open the same ↑↓/Enter picker, free-text questions turn the input box into an answer field (Esc skips, and the model proceeds on its own judgment). Headless and swarm runs stay fully autonomous.

## Workspace layout

- `crates/rift-provider` — the `Provider` trait plus the neutral wire types (messages, tool calls, stream deltas) every backend maps to.
- `crates/rift-ollama` — native Ollama client: NDJSON streaming, tool calls, thinking, capability detection, truncation detection.
- `crates/rift-openai` — OpenAI-compatible client: SSE streaming, tool-call correlation by id, string-encoded arguments (vLLM, LM Studio, llama.cpp, OpenRouter, LiteLLM, Ollama `/v1`).
- `crates/rift-core` — agent engine: tool registry (read/write/edit/bash/ls/grep/glob), agent loop, local-model hardening.
- `crates/rift-tui` — `rift` binary: ratatui frontend + headless mode.

See `docs/PROJECT.md` for status and roadmap, `docs/RESEARCH.md` for the protocol/architecture research this is built on.
