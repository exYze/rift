# Rift for VS Code

Runs [rift](https://github.com/exYze/rift) — a fast terminal coding agent for
local Ollama models — inside VS Code's integrated terminal, with editor-side
integration on top.

Because the extension embeds the real rift binary, the **entire** feature set
works exactly as it does standalone: approval prompts, sessions
(`/quit`-safe autosave, `--continue`), skills, MCP servers, sub-agents,
project memory, `/fork`, swarm, web search, the live diff pane — all of it.

## Features

### Sidebar chat

Click the Rift icon in the activity bar (or `Ctrl+Cmd+R` / `Ctrl+Alt+R`, or
the `rift` status-bar button) to open the chat view — a full conversation UI
backed by `rift --serve`, with streaming replies, tool-call activity,
inline **approval prompts** (diff preview + allow/deny buttons), the model's
plan checklist, and session controls (new / continue-last).

Drag the view into the **secondary sidebar** (View → Appearance → Secondary
Side Bar, then drag the Rift icon there) to keep the chat, your file, and
the explorer all visible at once.

- **Rift: Add Selection to Prompt** (`Ctrl+Cmd+K` / `Ctrl+Alt+K`, also in the
  editor right-click menu) — inserts an `@file` mention (plus the selected
  line range) into the chat input, so the file content is attached to your
  next prompt.
- **Rift: Add File to Prompt** — same, from the explorer right-click menu.

### Integrated terminal

The full rift TUI is still one command away — **Rift: Open Terminal** (plus
New/Continue variants) runs it in the integrated terminal for everything the
chat doesn't surface (slash commands, /fork, swarm, the diff pane…).

## Requirements

The `rift` binary must be installed ([install instructions](https://github.com/exYze/rift#install))
and reachable — either on `PATH` or via the `rift.executablePath` setting.

## Settings

Everything is configurable from inside the chat: the **model dropdown**
under the input lists every model rift can currently reach — the default
host's models (Ollama or an OpenAI-style server like vLLM) plus every
provider configured in rift's own config, as `provider/model` entries.
Switching models restarts the session with `--continue`, so the
conversation carries over. The **⚙ gear** opens a settings panel for the
binary path, server URL, and extra args; "Edit rift config file…" opens
`~/.config/rift/config.json` for everything else (providers, permissions,
hooks).

The same values are exposed as VS Code settings:

| Setting | Default | Effect |
| --- | --- | --- |
| `rift.executablePath` | `rift` | Path to the rift binary |
| `rift.host` | *(empty)* | Server URL, passed as `--host` (Ollama, or `…/v1` for vLLM/LM Studio) |
| `rift.model` | *(empty)* | Model, passed as `--model` (supports `provider/model` prefixes) |
| `rift.effort` | *(empty)* | Reasoning effort for thinking models (`minimal`…`max`), passed as `--effort` |
| `rift.extraArgs` | `[]` | Extra CLI args on every launch |

Empty settings defer to rift's own defaults and `.rift.json` config, so an
existing rift setup needs no configuration here at all.

## Building from source

```sh
cd vscode
npx --yes @vscode/vsce package   # produces rift-vscode-<version>.vsix
code --install-extension rift-vscode-*.vsix
```
