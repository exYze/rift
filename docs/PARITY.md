# Rift vs opencode — feature parity

A working comparison against opencode (v1.18.x, July 2026) covering its CLI,
TUI, and desktop app. Kept honest in both directions: where rift is ahead,
where it matches, what it deliberately doesn't do, and what's being closed.

Rift's founding goal — models run measurably faster inside this harness —
is benchmarked in [BENCHMARKS.md](BENCHMARKS.md): on the latest 150-run
suite, **57% fewer prompt tokens and 12.7× faster wall time** at equal-or-
better task success (150/150 vs 149/150).

## At parity (or ahead)

| area | rift | opencode | notes |
|---|---|---|---|
| Sessions | resume/continue, fork, rewind (files+conversation), undo, named saves, picker, autosave | resume/fork, snapshots+revert, undo/redo | rift's `/rewind` restores files *and* conversation |
| Context management | two-stage compaction, calibrated token estimator, hydrate-on-demand reads, explicit num_ctx | auto-compaction (`compaction.auto/prune`) | rift's is the benchmarked differentiator |
| Subagents | concurrent `agent` tool, model roles, personas (`.rift/agents/*.md`), background tasks | task tool, agent definitions, subagent depth | comparable; rift adds cross-model roles |
| Parallel exploration | **WarpDrive**: worktree racing, judge, side-by-side merge TUI | not shipped (worktrees not wired into desktop; community plugins) | rift ahead |
| MCP | stdio + remote HTTP, project/user config, trust flow, `/mcp` | stdio + remote, OAuth, `mcp` CLI | opencode adds OAuth; rift roadmap |
| Permissions | allow/ask/deny `Tool(pattern)` rules, approval mode, bash wrapper (WSL/Docker/firejail), deny floor | allow/ask/deny per tool with patterns, doom-loop ask | comparable |
| Custom commands / skills | plugin commands, skills (`SKILL.md`), `/skill:<name>` | markdown commands, Agent Skills (Claude-compatible dirs) | comparable |
| Plugins | `plugin.json`: commands, subprocess tools, hooks | JS plugins (Bun), rich hook surface | different philosophies: subprocess vs JS-in-process |
| Themes | 13 built-in + custom JSON | ~13 built-in + custom JSON | parity |
| Images/attachments | `@photo.png`, `/paste`, `--attach` | attachments, resize config | parity |
| Headless / scripting | `-p` one-shot, `--output-format json`, `--trace` JSONL | `run` with formats, stdin, sessions | parity for CI use |
| Providers | Ollama native, OpenAI-compat, Anthropic native; `provider/model` routing | many via npm SDKs + Zen gateway | rift is local-first by design |
| Editor integration | VS Code extension (sidebar chat, inline per-hunk diff review, terminal) | VS Code-family extension, JetBrains/Zed via ACP | ACP on roadmap |
| Update | `rift update`, startup check, winget/scoop/brew | `upgrade`, autoupdate config | parity |

## Gaps closed (this branch)

| feature | opencode has | rift now |
|---|---|---|
| **Desktop app** | Electron app (migrated *from* Tauri), tabs-primary UI, sessions sidebar, diff review, onboarding | ✅ **Tauri 2** app over the serve protocol — no Node runtime, one small binary; tabs, sessions sidebar, in-app per-hunk diff review (`desktop/`) |
| **LSP diagnostics** | 30+ auto-detected servers, diagnostics feed the agent | ✅ zero-dep LSP client (rust-analyzer, pyright, typescript-language-server, gopls, clangd auto-detected); post-edit diagnostics appended to tool results, token-capped |
| **GitHub integration** | GitHub App, `/opencode` comment triggers, PR reviews | ✅ `rift github install` writes a self-hosted Actions workflow: `/rift` comment triggers headless rift on your runner/model, pushes a branch, opens a PR — no hosted app needed (docs/GITHUB.md) |
| **Share** | cloud share links (opncd.ai) | ✅ `/share`: self-contained HTML transcript export; optional GitHub gist upload via `gh`. No rift cloud service — local-first |

## Deliberate non-goals

- **Zen / Go** (paid model gateways) — rift's premise is your own hardware
  and your own keys.
- **Cloud share service** — sharing exports a file you own, not a hosted link.
- **Electron** — the desktop app stays Tauri/Rust; WebView2/WebKit ship with
  the OS.
- **npm-based plugin runtime** — rift plugins stay subprocess-based
  (any language, no Node dependency).

## Known remaining gaps (roadmap candidates)

- ACP (Agent Client Protocol) server mode for Zed/JetBrains/Neovim.
- HTTP server + web UI (`opencode web`, mDNS, remote attach). The serve
  protocol is transport-agnostic; an HTTP/SSE bridge is a contained follow-up.
- Built-in formatter registry (rift covers this today via `post_edit` hooks).
- MCP OAuth flows; provider `auth login` OAuth.
- `stats`/`db` CLI analogs (rift has `/stats`, `/tokens`, JSONL traces).
