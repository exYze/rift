# Rift Desktop

A native desktop shell for rift — tabs, a sessions sidebar, inline per-hunk
diff review, live model switching — built with **Tauri 2**, no Node runtime,
no bundler. The app is a consumer of the serve protocol
([docs/SERVE.md](../docs/SERVE.md)): every tab owns one `rift --serve`
process, exactly like the VS Code extension does, so all agent behavior
(tools, permissions, compaction, plugins, MCP) is the rift binary's — the
desktop app never reimplements it.

Where opencode moved its desktop app *to* Electron, rift's stays a small
Rust binary over the OS webview (WebView2 on Windows, WebKit on
macOS/Linux) — same philosophy as the ~14MB CLI.

## Layout

```
desktop/
  ui/            static frontend — no framework, no build step
    index.html   shell + per-tab template
    app.js       serve-event renderer (ported from the VS Code webview) + shell
    style.css    theme layer + ported transcript styles
  src-tauri/     the Tauri app
    src/main.rs  process manager, folder picker, file index, settings
```

## Features

- **Tabs**: each tab is an independent conversation (own `rift --serve`
  process), Ctrl+T new / Ctrl+W close / Ctrl+1-9 switch, per-tab busy dot.
- **Sessions sidebar**: every saved chat, newest first — click to reopen in
  the active tab. Recent projects for quick reopening.
- **Per-hunk diff review**: proposed write/edits arrive *before* they touch
  disk; accept or reject individual hunks in the review modal (rift's own
  `segments` hunking — the app never diffs).
- **Everything the serve protocol streams**: thinking, tool activity (boxed
  and capped), applied-diff cards, plan checklist, sub-agent lanes,
  ask prompts, context gauge, turn stats.
- **@file mentions** (workspace index from the Rust side) and `/skill:`
  completion, identical to the VS Code chat.
- **Model switching** that keeps the conversation (`set_model`), model list
  from every configured provider (`list_models`).
- Light/dark theme, settings for binary path / server URL / model /
  num_ctx / temperature / effort / max iterations. Providers, permissions,
  hooks and the rest live in rift's own config file, as always.

## Requirements

- The `rift` binary on PATH (or set its path in Settings).
- Windows: WebView2 (ships with Windows 11 / modern Windows 10).
  Linux: webkit2gtk 4.1. macOS: nothing extra.

## Build

```sh
cd desktop/src-tauri
cargo build --release        # produces the app binary
```

Dev loop: `cargo run` (the frontend is static files — edit `ui/` and press
F5/reload; no dev server, no npm). Set `RIFT_DESKTOP_SMOKE=1` to auto-exit
after 3s (CI smoke test).

Installers (NSIS/dmg/AppImage/deb) are produced by `cargo tauri bundle`
(via `cargo install tauri-cli`) or the `desktop-build` CI workflow.

## Protocol notes

The app speaks serve protocol v1 and feature-detects optional commands
(`list_models`, `set_model`) from the `ready` event's `commands` field, so
it degrades gracefully against older rift binaries. Closing a tab (or the
window) closes the child's stdin — rift saves the session and exits
cleanly, per the protocol contract.
