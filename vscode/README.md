# Rift for VS Code

Runs [rift](https://github.com/exYze/rift) — a fast terminal coding agent for
local Ollama models — inside VS Code's integrated terminal, with editor-side
integration on top.

Because the extension embeds the real rift binary, the **entire** feature set
works exactly as it does standalone: approval prompts, sessions
(`/quit`-safe autosave, `--continue`), skills, MCP servers, sub-agents,
project memory, `/fork`, swarm, web search, the live diff pane — all of it.

## Features

- **Rift: Open** (`Ctrl+Cmd+R` / `Ctrl+Alt+R`) — open rift in the integrated
  terminal at the workspace root; reuses the running session if there is one.
  Also available from the `rift` status-bar button.
- **Rift: New Session / Continue Last Session** — fresh start or `--continue`
  the most recent session.
- **Rift: Add Selection to Prompt** (`Ctrl+Cmd+K` / `Ctrl+Alt+K`, also in the
  editor right-click menu) — types an `@file` mention (plus the selected line
  range) into rift's input, so the file content is attached to your next
  prompt.
- **Rift: Add File to Prompt** — same, from the explorer right-click menu or
  command palette.

## Requirements

The `rift` binary must be installed ([install instructions](https://github.com/exYze/rift#install))
and reachable — either on `PATH` or via the `rift.executablePath` setting.

## Settings

| Setting | Default | Effect |
| --- | --- | --- |
| `rift.executablePath` | `rift` | Path to the rift binary |
| `rift.host` | *(empty)* | Ollama server URL, passed as `--host` |
| `rift.model` | *(empty)* | Model, passed as `--model` |
| `rift.extraArgs` | `[]` | Extra CLI args on every launch |

Empty settings defer to rift's own defaults and `.rift.json` config, so an
existing rift setup needs no configuration here at all.

## Building from source

```sh
cd vscode
npx --yes @vscode/vsce package   # produces rift-vscode-<version>.vsix
code --install-extension rift-vscode-*.vsix
```
