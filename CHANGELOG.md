# Changelog

All notable changes to rift. Versions follow the roadmap phases in
[docs/ROADMAP.md](docs/ROADMAP.md); dates are release dates.

## v2.0.0 — 2026-07-10

The agent platform. Breaking changes, bundled once — everything here was
previewed behind flags in 1.10/1.11, so 2.0 is a promotion, not a
surprise. The 1.0 stability promise resets for 2.x: config and protocol
stay stable, breaking changes need a 3.0.

- **Plugin API stable, on by default** (the `experimental.plugins` flag is
  accepted but no longer needed). A plugin directory can now contribute:
  - *commands* — prompt templates, surfaced like skills (`/skill:<name>`)
  - *tools* — subprocess tools; PROJECT plugins get the same one-time
    startup trust prompt as project hooks/MCP, keyed to the exact manifest
    (any edit re-prompts); user plugins register freely
  - *hooks* — `"hooks": {"post_edit": [...]}`; user plugins apply as-is,
    project-plugin hooks join the existing per-command trust flow
  - *themes* — `themes/<name>.json` ({"base": "dark", "accent":
    "#39c5cf", …}); also loadable outside plugins from
    `~/.config/rift/themes/`; `/theme <name>` resolves built-ins first
  - *prompt targets* — `prompts/<family>.md` from USER plugins only: a
    cloned repo must never replace the system prompt
- **Config schema v2, migrated automatically**: a v1 user config is
  rewritten in place on first load (backup: `config.json.v1.bak`);
  read-only configs use the migrated form in memory. Project `.rift.json`
  legacy `bash_deny` globs keep loading (tighten-only — dropping them
  would be a security regression) with a nudge toward
  `rift config migrate --project`
- **Serve protocol v1 frozen**, with two additive extensions: `ready`
  lists `skills` (skills + plugin commands) so editor chats can offer
  completion, and a prompt of `/skill:<name> [task]` expands server-side
  exactly like the TUI
- **VS Code extension parity**: typing `/` in the sidebar chat now
  completes skills and plugin commands (same popup as `@file` mentions,
  descriptions included) and invoking them runs the real skill expansion

## v1.11.0 — 2026-07-10

Platform preview (roadmap v1.11): everything 2.0 stabilizes ships here
first, behind flags — the breaking release becomes a promotion, not a
surprise.

- **Experimental plugin API** (`"experimental": {"plugins": true}` in the
  config): a plugin is a directory with `plugin.json`, discovered from
  `.rift/plugins/` (project) and `~/.config/rift/plugins/` (user).
  *Commands* are prompt templates (`{args}` = invocation arguments) and
  surface exactly like skills — `/skill:<name>` in the palette, listed to
  the model. *Tools* run a subprocess (args JSON on stdin, stdout is the
  result, nonzero exit = error, 60s timeout) — user plugins only in the
  preview: a cloned repo must not register commands to execute, so
  project-plugin tools are skipped with a pointed warning until 2.0's
  trust flow
- **Config schema v2 + migrator**: `rift config migrate` rewrites the
  deprecated `permissions.bash_allow`/`bash_deny` globs as `Bash(...)`
  rules, stamps `"version": 2`, and backs the original up as
  `config.json.v1.bak`; `--dry-run` previews, `--project` targets the
  project `.rift.json`. Runs before the normal config load, so a config
  broken enough to need migrating can't block the migrator
- **Deprecation warnings**: loading a config that still uses
  `bash_allow`/`bash_deny` warns they stop loading in 2.0 and points at
  the migrator — nothing 2.0 breaks goes unannounced

## v1.10.0 — 2026-07-10

Serve protocol v1 (roadmap v1.10): the `--serve` surface becomes a
contract third parties can build on.

- **docs/SERVE.md** documents every command and event — fields, semantics
  (one turn at a time, one id space, orphaned reviews closed, dropped
  replies deny), and the versioning rules: additive changes never break
  v1, consumers ignore what they don't know, and removals/renames bump
  the protocol version (a 2.0-class change)
- **`protocol_version`** now rides in the `ready` event, and `hello` is
  acked with a `capabilities` event carrying the negotiated set — a
  consumer can confirm exactly what it's speaking to
- **Conformance tests** pin every event's exact wire shape in serve.rs;
  a failing test means a breaking protocol change and says so
- **Reference client**: `scripts/serve_client.py` — an interactive minimal
  integration (spawn, hello, prompt, stream events, answer asks, decide
  reviews) for Neovim/JetBrains plugin authors; SERVE.md's integration
  guide walks through it
- The VS Code extension (the reference consumer) checks
  `protocol_version` and warns on a mismatch instead of failing weirdly

## v1.9.0 — 2026-07-10

Distribution, finished (roadmap v1.9) — plus the long-standing quick wins.

- **winget**: `scripts/make_winget.sh vX.Y.Z` generates the manifest trio
  from a release's published checksums; the release workflow auto-submits
  version updates to microsoft/winget-pkgs via wingetcreate once the
  `WINGET_TOKEN` secret exists (first-time submission is manual — see
  packaging/winget/README.md)
- **crates.io**: rift-provider/ollama/openai/anthropic/core now carry full
  publish metadata and the release workflow publishes them in dependency
  order once `CARGO_REGISTRY_TOKEN` exists — `rift-ollama` and friends
  usable as standalone dependencies
- **`--version` update nudge**: `rift --version` reports a newer release
  when the 24h update-check cache knows one — cache-only, never a network
  call, so offline machines never stall. Headless runs print the same
  nudge to stderr after the turn (stdout pipelines unaffected)
- **`/copy` palette completion**: typing `/copy ` offers `all` and `log`
  with descriptions, prefix-filtered like command completion
- **Palette mouse-wheel**: the wheel moves the selection in the command /
  @file popup instead of scrolling the pane beneath it
- **Session file size cap**: autosaves stay under 10MB — whole turns are
  trimmed from the front (system prompt kept, tool-call pairs never
  split). Live context is untouched; compaction still owns that

## v1.8.0 — 2026-07-10

Model targets: each open-model family treated as a compiler target, with
the bench matrix as the fitness function (roadmap v1.8).

- **Family prompt targets**: `qwen`, `deepseek`, `glm`, and `mistral`
  prompt files embedded alongside `gemma` — each tuned to its family's
  documented failure modes (qwen: narration and double-verification;
  deepseek: reasoning spill and over-exploration; glm: chat-only answers
  and reply language; mistral: exact-match edit retries without whole-file
  rewrites). All provisional until they clear the evolution gate on a
  matrix run; models with no family target keep `default` exactly as before
- **Prompt evolution gate**: `scripts/prompt_gate.py --family F
  --candidate f.md --models m1,m2` benchmarks the embedded incumbent, then
  the candidate as a user-level override (no rebuild), and diffs pass
  rate, prompt tokens, wall time, and the per-turn failure counters from
  traces — printing a PR-ready verdict. A candidate merges only if it wins
  on every model
- **Tool-schema A/B**: `RIFT_TOOL_SCHEMA=lean` serves every tool with a
  first-sentence description and no per-parameter docs — structure
  (types, required, enums) is untouched. `bench.py --schema lean|rich`
  drives it and tags each result row with the variant, so schema cost can
  be measured per model family instead of assumed
- BENCHMARKS.md records the methodology and the planned validation matrix

## v1.7.3 — 2026-07-10

- **`"editor"` config key**: set the `/config edit` editor in rift's own
  config (`"editor": "code -w"`, flags allowed) instead of wrestling with
  `$EDITOR` on Windows. Precedence: config → `$EDITOR` → `$VISUAL` → the
  terminal-editor PATH probe. Loads from the user config only — a cloned
  repo's `.rift.json` must never choose what command runs — and
  hot-reloads with the rest of the config
- **`/config edit` says what it's opening**: the resolved editor and where
  it came from ("opening config in vim… (default — set \"editor\" in the
  user config or $EDITOR to change)"), so the PATH-probe pick is never a
  surprise
- **Broken JSON no longer strands the config**: saving a config that fails
  to parse now keeps the previous settings live and reports the exact
  error with line and column, pointing back at /config edit — instead of
  a bare reload error with the settings in limbo
- **Merge-to-release**: merging a version bump to master now tags and
  publishes the release automatically — the release workflow spots a
  Cargo.toml version with no matching tag and cuts `v<version>` itself.
  Manually pushed `v*` tags still work exactly as before. The pipeline is
  resilient to transient CI flakes: shell network steps retry, and a
  failed build/package leg (e.g. a DNS blip on artifact upload) re-runs
  its failed jobs automatically, capped at three attempts

## v1.7.2 — 2026-07-09

- **Default editor opens in the terminal**: with no `$EDITOR`/`$VISUAL`
  set, Windows no longer defaults `/config edit` to notepad popping the
  file open in a separate window. The default now probes PATH for a
  terminal editor — `edit` (Microsoft's terminal editor, in-box on
  Windows 11), then nano, vim, nvim, vi, hx, micro — and opens it in the
  terminal via the usual TTY handover. Notepad remains only as the last
  resort when no terminal editor exists; an explicitly set `$EDITOR` is
  respected as before

## v1.7.1 — 2026-07-08

- **Robust Windows shell quoting**: the bash tool ran commands through
  `cmd.exe /C` with Rust's default (MSVCRT-targeted) argument quoting, which
  cmd.exe re-parsed under its own rules and mangled — a `python -c "import
  x"` argument reached the shell as a broken `"import` and every quoted
  command failed. The command line is now built with `raw_arg` and `/S`, so
  cmd strips exactly the outer quote pair and runs everything inside
  untouched, preserving any quoting for any command shape (foreground,
  background, and post-edit hooks; composes with the sandbox wrapper)
- **Stuck-turn guard**: the doom-loop guard refused identical repeats but
  let the turn grind on to `max_iterations` (25 wasted steps), re-warning on
  every repeat. A running count of tool calls that failed or were refused
  without any success between now nudges the model to change tack at 4 and
  ends the turn cleanly at 7 — catching both an exact-repeat loop and a
  model varying a broken call slightly, while leaving legitimate iterative
  debugging alone (a success resets the streak). The repeat warning fires
  once per signature, not per repeat
- **Config editing keeps the TUI visible**: `/config edit` no longer blanks
  the terminal while a GUI editor (notepad, VS Code, …) holds the file in
  its own window. The TUI stays up, dimmed behind a "close the file to
  continue" modal, and hot-reloads when the editor window closes. Terminal
  editors (vim/nano) and anything unrecognized keep the full-TTY handover

## v1.7.0 — 2026-07-08

- **Granular permission rules**: `permissions.allow` / `ask` / `deny` lists
  of `Tool(pattern)` rules — `Bash(git push *)`, `Edit(src/**)`,
  `Read(~/.ssh/**)`, `Fetch(*://*.internal/*)` — with precedence
  deny > ask > allow > approval mode. Deny holds everywhere: YOLO mode,
  headless runs, inside grep/glob walks, `ls` of a covered directory, and
  across fetch redirects (re-checked per hop). Ask rules force a prompt
  even in /yolo (and deny headless); allow rules load from the user config
  only — a project `.rift.json` can add deny/ask but never allow. Matching
  is hardened: paths canonicalize first (symlinks, `..`, `\\?\` verbatim
  prefixes, case-insensitive on Windows/macOS), bash rules see the same
  `${IFS}`-folded chained segments as the deny list, URLs normalize
  scheme/host/default-port. Edit approval prompts offer a persistent
  "always allow `Edit(<dir>/**)`" scoped to the file's work area;
  `/permissions add|remove <allow|ask|deny> <rule>` edits rules live; the
  legacy `bash_allow`/`bash_deny` globs still load as `Bash(...)` rules
- **Inline diff review in VS Code — per-hunk accept/reject**: every
  write/edit the agent proposes opens as a native VS Code diff *before it
  touches disk*. Accept or reject each hunk via CodeLens (rejecting one
  visibly removes its change), apply with the ✓ title-bar button, and
  track pending reviews as chat cards (Apply / Open diff / Reject). Only
  the accepted hunks are written, the model is told when a proposal was
  partially applied, and `/undo` still restores the true prior state.
  Serve protocol: consumers opt in with `{"cmd":"hello","edit_review":true}`,
  then receive `edit_review {path, old, new}` events and reply with
  `edit_decision`; cancelled/finished turns emit `edit_review_closed` so a
  stale Apply can't claim success. Consumers that never say hello (older
  extensions, scripts) keep the classic in-chat approval prompts
- **Context-window gauge**: the TUI status bar shows `ctx 42% 13k/32k`
  next to the model chip (green under 60%, amber to 84%, red above) and
  the VS Code chat header shows a matching color-coded `ctx 42%` pill with
  token counts on hover — the calibrated estimate of what the conversation
  occupies (system prompt + history + tool schemas) vs the working
  `num_ctx`, refreshed at startup and after every turn, command, and
  compaction. New serve event `context {used, limit}`; `ready` now
  carries `num_ctx`

## v1.6.4 — 2026-07-07

- **Multi-agent visibility in the VS Code chat**: when rift fans work out to
  sub-agents (the `agent` tool) or background tasks, the chat shows a live
  card per agent — pulsing status, model, task label, and the agent's own
  scrolling activity feed — instead of flat interleaved log lines, plus a
  running-agent count in the header status (`⧉ 2 running`). Cancelled turns
  sweep their agent lanes closed
- **Structured sub-agent events**: new `AgentEvent::SubAgentStarted /
  SubAgentActivity / SubAgentFinished` variants replace preformatted Info
  strings, emitted over `--serve` as `subagent_started` / `subagent` /
  `subagent_finished`. The TUI, swarm UI, and headless output render
  byte-identical lines to before

## v1.6.3 — 2026-07-07

- **Instant hover tooltips in the VS Code chat**: every control shows a
  styled, theme-aware hover box immediately (native `title` tooltips were
  delayed and easy to miss on small icon buttons), with fuller descriptions
  of what each button actually does; icon buttons got bigger hit areas
- **Full-view settings panel**: the ⚙ settings are now an overlay covering
  the chat with one labeled field per option — binary path, server URL,
  context window (`--num-ctx`), temperature (`--temp`), max iterations
  (`--max-iterations`), reasoning effort — each with an explanatory
  tooltip. New VS Code settings: `rift.numCtx`, `rift.temperature`,
  `rift.maxIterations`
- **Removed `rift.extraArgs`**: the raw pass-through is superseded by the
  dedicated fields above

## v1.6.2 — 2026-07-07

- **Syntax highlighting in the VS Code chat**: fenced code blocks render
  with highlight.js (vendored, pinned 11.11.1 — webview CSP allows no CDNs),
  matching VS Code's default dark/light token colors and following the
  active theme. Labeled fences highlight directly; unlabeled ones
  auto-detect against a shortlist of common languages
- **Code block buttons**: hovering a block shows **copy** and **insert** —
  insert places the code at the cursor in the active editor, replacing the
  selection if there is one
- **CI: bump actions to their Node 24 majors** — checkout@v7, setup-node@v6,
  upload-artifact@v7, download-artifact@v8, action-gh-release@v3 — clearing
  the Node 20 deprecation warnings

## v1.6.1 — 2026-07-07

- **@-mention file picker in the VS Code chat**: typing `@` opens a popup of
  workspace files and folders (keyboard navigation, live filtering, picking
  a folder drills into it), fed by the extension host with junk directories
  excluded — no more typing paths by hand
- **Readable tool output in the VS Code chat**: a tool call's start and
  result now fold into a single row (`✓ bash command=… → 100`), and clicking
  it expands the full arguments and output in a scrollable block. Thinking
  blocks auto-scroll while streaming (no more mid-line clipping), click to
  expand, and thinking after a tool call starts a new block below it instead
  of appending above out of order
- **Undo from the chat**: new `{"cmd":"undo"}` in the `--serve` protocol
  reverting the last turn's write/edit changes via the same journal as the
  TUI's `/undo`, surfaced as an ↶ button in the chat header (bash-made
  changes stay untracked, same as the TUI)
- **install.sh: remove the old binary before installing** — overwriting it
  in place kept the old inode, and macOS SIGKILLs binaries whose cached
  code signature no longer matches

## v1.6.0 — 2026-07-07

- **VS Code sidebar chat**: a full chat UI backed by a new `rift --serve`
  JSON-lines protocol over stdio — streaming replies, tool activity,
  inline approval prompts (diff preview + allow/deny), the plan checklist,
  and new/continue session controls. Lives in its own activity-bar
  container, so it drags to the secondary sidebar and sits beside the
  editor. In-chat **model dropdown** discovers every model rift can reach
  (default host plus configured providers, as `provider/model`) and
  switches mid-conversation via `--continue`; a ⚙ settings panel edits the
  binary path, server URL, and reasoning-effort (dropdown, not a raw flag),
  with a button to open rift's own config for providers/permissions
- **`rift --serve`**: editor-integration mode — one JSON event per line on
  stdout, one command per line on stdin, approvals and `ask_user`
  questions surfaced as `ask` events answered by id. The safety model is
  identical to the TUI (approval on by default)
- **Harden the bash deny list against expansion tricks**: `bash_denied`
  now folds whitespace-producing expansions (`${IFS}`, `$IFS`, `$@`, `$*`)
  to spaces and splits on `$` before matching, so `rm${IFS}-rf${IFS}/` and
  `sudo${IFS}whoami` no longer slip past the built-in patterns. Remains
  best-effort (approval mode is the real gate), but the documented bypass
  is closed

## v1.5.3 — 2026-07-07

- **VS Code extension** (`vscode/`): rift in VS Code's integrated terminal
  with the complete feature set (it runs the real binary), plus editor
  glue — launch/continue commands with keybindings, a status-bar button,
  and right-click "Add File/Selection to Prompt" that types `@file`
  mentions into rift's input. Configurable binary path, host, model, and
  extra args; empty settings defer to `.rift.json`. Releases now attach
  `rift-vscode.vsix` — install with `code --install-extension`

## v1.5.2 — 2026-07-07

- **Responsive startup banner**: the RIFT logo is now drawn to fit the
  pane on every re-wrap — a larger pixel-block render at 2× on wide
  terminals (84+ cols) or 1× (42+), the compact box-drawing version
  below that, and plain text on slivers. Centered, resizes live, and
  never garbles on narrow terminals

## v1.5.1 — 2026-07-07

- **Startup banner**: a RIFT ascii-art logo (accent-colored, never
  re-wrapped on narrow terminals) opens the transcript, with a
  `v<version> · <model>` tagline beneath. The "session resumed" marker
  now keys off actual seeded history instead of transcript emptiness

## v1.5.0 — 2026-07-07

- **`web_search` tool + `/search`**: web search through a self-hosted
  SearXNG instance (`search_url` in config, or `/search <url>` — probed
  via the JSON API before adoption and persisted to the user config;
  `/search off` disables). Local-first: queries go to YOUR metasearch
  box, not a third-party API. Sub-agents inherit it
- **`/deep-research <question>`**: a research workflow through the normal
  agent loop — decompose into search angles, `web_search` each, delegate
  source-reading to concurrent sub-agents (fetch + verbatim quotes +
  dates), cross-check claims across sources (corroborated vs
  single-sourced), and synthesize a cited markdown report with a
  numbered source list. Verified live end-to-end
- **Project memory**: `.rift/memory.md` loads into the system prompt every
  session — durable facts that compound over time. Grown by `/remember
  <fact>` and by the model's new `remember` tool (short entries enforced,
  exact duplicates refused, 16KB cap with a prune hint, prompt budget
  capped separately from the guides)
- **`/fork`**: duplicate the conversation into a NEW session file and open
  a second rift window resuming the copy — parallel exploration with full
  context, original untouched. New console on Windows, Terminal via
  osascript on macOS, $TERMINAL/x-terminal-emulator/gnome-terminal/
  konsole/xterm on Linux
- **`fetch` tool**: built-in web fetch — GET a URL, strip HTML to
  readable text (script/style/comment-aware), 20KB cap, 20s timeout.
  Docs pages and READMEs without an MCP server; search still belongs to
  MCP (`/mcp add`)
- **Sandbox wrapper**: `permissions.bash_wrapper` routes every bash
  command (foreground, background, sub-agents) through a containment
  tool — `"wsl -e sh -c '{cmd}'"`, Docker, firejail, bwrap. Real
  isolation from tools built for it, not a homemade half-sandbox; the
  deny list and approval still inspect the raw command. User config
  only; shown in /permissions

## v1.4.0 — 2026-07-06

- **Remote MCP servers (streamable HTTP)**: config entries and
  `/mcp add <name> <url>` now take a `url` instead of a command — one
  POST per JSON-RPC message, plain-JSON and SSE-framed responses both
  handled, `Mcp-Session-Id` captured and echoed, custom `headers` for
  auth tokens. Trust identity covers url + headers
- **Interactive background tasks**: the `task` tool (and `/tasks send` /
  `/tasks eof`) can write lines to a running task's stdin and close it —
  REPLs, y/n prompts, and read-to-EOF filters are now drivable
- **`/paste`**: attach a clipboard image to your next message
  (PowerShell / pngpaste / wl-paste+xclip per platform)
- **`--output-format json`**: headless runs print one machine-readable
  result object (reply, tool calls, token/duration stats, estimated
  cost, session path) on stdout with progress on stderr — verified live
- **Anthropic prompt caching**: cache_control breakpoints on the system
  prompt and the conversation tail, so agent-loop iterations re-read the
  prefix from cache instead of re-billing full input price

## v1.3.0 — 2026-07-06

- **Hooks — automatic post-edit verification**: `"hooks": {"post_edit":
  ["cargo check --quiet"]}` runs after every successful write/edit; a
  failing hook's output (capped, ANSI-stripped, with exit code) is
  appended to the tool result so the model fixes broken builds/tests in
  the same turn. Successes log a `hook ✓` line. Project-config hooks
  need one-time trust at startup (they auto-execute; a cloned repo must
  not get that for free); sub-agents run the same hooks
- **Checkpoints — `/rewind [n]`**: restore the write/edit changes AND
  the conversation of the last n turns together (session file rewritten,
  transcript reseeded, up to 20 turns back). The undo journal now keeps
  20 turns instead of 3. Like Claude Code checkpoints, bash-made changes
  are outside the journal; compaction clears rewind marks
- **Agent personas**: `.rift/agents/<name>.md` (project) or
  `~/.config/rift/agents/` (user-wide) define custom sub-agent types —
  frontmatter `name`/`description`/`model` (role or full name)/`tools`
  (whitelist) plus a prompt body layered onto the base system prompt.
  Tasks select one with `agent: "<name>"`; configured personas are
  advertised to the model. Verified live end-to-end
- **Diff preview in approvals**: write/edit approval prompts now render
  a diff-colored preview of the pending change (trimmed to the changed
  region, capped at 40 lines) above the allow/deny question — you review
  what you're approving, not a byte count

## v1.2.0 — 2026-07-06

- **Image attachments for vision models**: `@photo.png` in a prompt (or
  `--attach <path>` headless) attaches the image as base64 — ask about
  screenshots, diagrams, error dialogs. One neutral form (data URLs on
  the message), translated per provider: Ollama's bare-base64 `images`
  array, OpenAI `image_url` content parts, Anthropic image source
  blocks; wire shapes pinned by tests and verified live against a
  vision-capable Ollama model. Text mentions keep the outline treatment;
  images cap at 10 MB; attachments persist in the session. Headless
  `--attach` also appends text files to the prompt
- **`/mcp add [--global] <name> <command> [args…]`**: connect an existing
  (off-the-shelf) stdio MCP server from inside the TUI — e.g.
  `/mcp add fetch uvx mcp-server-fetch`. The server is spawned and its
  tools verified BEFORE anything persists, registered live (no restart),
  and saved to the project `.rift.json` (pre-trusted — typing the entry
  is the consent the trust gate collects) or the user config with
  `--global`. Complements config-file entries and `/mcp new`
- Fix: after Esc-cancelling a turn, the model resumed the interrupted
  task on the next (even unrelated) message — the abandoned instruction
  was still the last word in history. Cancelled turns now mark the agent
  interrupted, and the next input carries a note that the task was
  deliberately abandoned and must not be resumed unasked
- Fix: resumed sessions rendered expanded command prompts (/mcp new,
  /init, …) as page-long user-colored walls, flattening the transcript —
  live sessions only ever showed the line the user typed. Seeded user
  messages now collapse to their first 8 lines + a count, and the
  Esc-interrupt note is stripped from display

## v1.1.1 — 2026-07-06

- **`/host` autodetects the server type**: probes the model list on both
  protocols (URL shape picks the order — `…/v1` tries OpenAI-compatible
  first) and adopts whichever answers, so `/host http://box:8000/v1`
  switches straight to a vLLM/LM Studio/llama.cpp server. The default
  host is protocol-aware everywhere now: bare `/model` switches, startup
  (`host` in config may be a `/v1` URL), `/restart`, and the context
  budget adoption all follow the detected kind. Ad-hoc hosts run keyless
  — keyed endpoints still belong in `providers`
- Fix: `/restart` relaunched against the STARTUP host — a `/host` switch
  never reached the restart spec (only the model survived). The UI now
  tracks host switches, so restart resumes on the current server

## v1.1.0 — 2026-07-06

- **Model roles — optional multi-model workflows**: a config `models` map
  names roles ({"smart": "vllm/…", "fast": "gemma4:26b"}), and each
  agent-tool task can set `model` to a role name or full model string —
  so one session plans/reviews on a strong model and delegates
  implementation to a cheap one (cross-provider: an Ollama child under a
  vLLM session works). Children on a different model get fresh
  think/effort defaults (capability checks don't transfer); routing
  failures surface as tool errors before anything runs. The system
  prompt advertises configured roles, and `/model`'s picker lists them
  first. No `models` map = exactly the old single-model behavior

## v1.0.5 — 2026-07-06

- Fix: picking a theme from the `/theme` picker failed with "unknown
  command '/theme'" — the picker forwarded its selection to the agent-side
  command dispatcher, but the theme is UI state. Both the picker and the
  typed form now share one UI-side switch (regression-tested)
- **Reasoning-effort levels**: `/think` now takes
  `minimal|low|medium|high|xhigh|max` alongside on/off/auto (a level
  implies thinking on), plus `--effort` and `"effort"` in config. One
  neutral knob, translated per provider: Ollama's graded string `think`,
  OpenAI/DeepSeek `reasoning_effort` + `thinking:{type}` toggle (per the
  DeepSeek V4 thinking-mode API), Anthropic-format
  `output_config.effort`. The OpenAI form also carries vLLM's
  `chat_template_kwargs` variant ({"thinking": bool, "reasoning_effort":
  …} per the DeepSeek-V4 vLLM recipe) — verified live against a vLLM
  DeepSeek-V4-Flash: thinking off/high/max produced 0/49/260 reasoning
  chunks. DeepSeek requirements honored: sampling params drop in
  thinking mode and `reasoning_content` is passed back during tool
  loops. Servers that reject any of the params get one retry without
  them, so an effort set on a model that can't grade is a no-op, not a
  failure

## v1.0.4 — 2026-07-06

- Fix: headless (`-p`) runs hung forever after printing their final line —
  the background-task registry held a clone of the event channel sender,
  so the output printer's wait for channel-close never ended and every
  headless run since v1.0.0 left a zombie rift process. The registry now
  detaches its channel before the final wait (regression-tested)

## v1.0.3 — 2026-07-06

- **10 new color themes**: `dracula`, `nord`, `gruvbox`, `solarized-dark`,
  `solarized-light`, `tokyo-night`, `catppuccin`, `rose-pine`, `matrix`,
  `synthwave` — truecolor palettes that paint their own text, background,
  and border colors (the classic `dark`/`light`/`mono` stay
  terminal-native). The `Theme` struct gained `fg`/`bg`/`border`; bare
  `/theme` now opens an interactive picker, and each theme's syntect
  mapping is pinned by test

## v1.0.2 — 2026-07-06

- **Claude Code-style permissions**: interactive sessions now **ask before
  write/edit/bash by default**, and each bash prompt offers allow once /
  **always allow `<pattern>`** (e.g. `git push *`, persisted to
  `permissions.bash_allow` in the user config — never prompts again) /
  allow all bash this session / deny. Allow-listed commands run silently;
  chained commands need every segment allowed; the deny list still always
  wins. A project `.rift.json` can only tighten (its `bash_allow` is
  ignored, its `approve: true` is honored, `approve: false` is not).
  Opt out with `"approve": false` in the user config
- **`/yolo [off]`**: stop asking before write/edit/bash entirely (the
  built-in + configured deny list is still enforced); `/yolo off` restores
  prompts. `/permissions` now shows allowed and banned patterns side by
  side
- **`/btw <question>`** (modeled on Claude Code's /btw): a quick side
  question that sees the whole conversation but has no tools, never enters
  the main history (the agent never sees the exchange), and runs on a
  UI-side task — so it works even while the agent is mid-turn. Follow-up
  `/btw`s continue a small side thread (kept to the last 10 exchanges,
  `/btw clear` resets it); answers render as dimmed `(btw)` blocks,
  buffered so a streaming main turn is never garbled. Context comes from
  the session autosave (history up to the last completed turn); the side
  question always uses the session's current model

## v1.0.1 — 2026-07-06

Verified against DeepSeek V4 Flash on vLLM; fixes for OpenAI-compatible
providers generally:

- **Context-length discovery**: `show()` now reads the model's context
  window from `/v1/models` (vLLM `max_model_len`, OpenRouter
  `context_length`, LM Studio `max_context_length`, llama.cpp
  `meta.n_ctx_train`). When the user hasn't set `--num-ctx`, provider-routed
  models adopt the reported context as rift's working budget (capped at
  128k so huge hosted contexts don't bloat every request); `/model`
  switches retune the budget the same way. Ollama keeps its conservative
  default — there num_ctx sizes the server's KV cache
- **Real throughput stats**: the OpenAI-compatible path now measures
  stream timing (prefill = to first chunk, decode = rest), so tok/s shows
  real numbers instead of 0.0 in the status line and `/stats`

## v1.0.0 — 2026-07-06

- **Concurrent sub-agents**: the model gets an `agent` tool — 1–4
  self-contained tasks run as concurrent child agents, each with its own
  context window, tool set, plan, and undo journal (permission policy and
  the ask channel are shared; nesting is blocked). Foreground calls wait
  and return every child's final report; `background=true` launches them
  as background tasks that keep working across turns. `/model` and
  `/host` switches carry over to children automatically
- **Background tasks**: `bash run_in_background=true` starts a command as
  a session-wide background task and returns its id immediately — it
  keeps running while the conversation continues. Output accumulates in
  a capped buffer; the new `task` tool lists/inspects/kills tasks; the
  status bar shows a live "N bg tasks running" count; and when a task
  finishes on its own, a `[task notification]` auto-turn hands the result
  back to the model (kills are silent, Esc suppresses pending
  notifications, at most 8 tasks run at once). `/tasks [kill <id>]` is
  the user-facing view of the same registry. Background tasks terminate
  with the rift process — no orphans

## v0.9.5 — 2026-07-03

- **`/goal <condition>`**: keep working until the model verifies the goal —
  turns auto-continue (up to 25) until a verified line-anchored `GOAL MET`,
  `/goal clear`/Esc stops it, and the run cap stops with a resume hint.
  A failed turn pauses the goal instead of spinning
- **`/loop [30s|5m|2h] <prompt or /command>`**: re-run a body on a fixed
  interval (rescheduled from fire time — no catch-up bursts) or
  back-to-back without one; `/loop stop` or Esc ends it, and a
  back-to-back loop halts if a run fails. Esc during a running auto-turn
  cancels the turn and the automation
- **Generator scope**: `/skills new` and `/mcp new` take `--global` (`-g`)
  to install user-wide (`~/.config/rift/`, every project, absolute paths,
  no trust prompt) instead of the project-scoped default (`.rift/`,
  trust-gated)

## v0.9.4 — 2026-07-03

- **Self-extension**: `/skills new <desc>` — the agent writes its own skill
  file from your description; `/mcp new <desc>` — the agent writes,
  self-tests, and registers a local stdio MCP server (stdlib-only Python),
  trust-gated like any project-config server. `/restart` loads either
  without losing your chat
- MCP client accepts the common bare-content-array `tools/call` result
  shape instead of silently reading an empty string

## v0.9.3 — 2026-07-03

- Fix: `/restart` before the first turn crashed the relaunch ("EOF while
  parsing a value") — session files are reserved empty at startup and
  resume now handles that (and missing/corrupt files, which back up to
  `.json.corrupt`) gracefully instead of failing startup. Also fixes the
  latent `rift -c` crash after quitting an unused session

## v0.9.2 — 2026-07-03

- **`/restart`**: relaunch rift and resume the exact session in place —
  the post-`/update` path that keeps your chat. True exec on Unix (same
  terminal, same PID), spawn-and-wait on Windows; `/update`'s success
  message now points at it
- The status line and `/restart` carry the addressable model name
  (provider prefix intact), so `anthropic/…` models survive a restart

## v0.9.1 — 2026-07-03

- **Markdown rendering in the transcript**: `## headings` render bold in
  the accent color, bullets become `•`, `inline code` and **bold** get
  span colors — raw markdown from the model no longer displays as plain
  text. Fence-tag coverage pinned by test (markdown/md/json/yaml/diff/…)
- Prompts now ask models to put document/file-content answers in a
  language-tagged fenced code block, so they render in the highlighted box
- Explicit content requests ("do not use any tools", "in a code block")
  no longer trip the apply-nudge, which could make the model discard the
  requested content on its retry

## v0.9.0 — 2026-07-02 · v0.8 phase complete: context engine v2

- **Hydrate-on-demand reads**: an unbounded `read` of a 500+ line source
  file returns its line-numbered outline instead of a 2000-line dump; the
  model then fetches exact ranges (offset/limit always verbatim). Measured
  on the hard tier: **−18.4% prompt tokens** suite-wide, −66% on the
  buried-bug big-module task, pass rate held (docs/BENCHMARKS.md)
- **Repo-map centrality ranking**: files ranked by import in-degree first,
  recency as tiebreak — central modules make the map even when untouched;
  falls back to pure recency when no imports resolve. Imports cached
  alongside outlines
- Traces record each tool call's file/pattern target (data for future
  retrieval tuning)
- `bench.py --runs N` (per-run pass counts + flaky-task list) and
  `RIFT_BIN` override for old-vs-new binary A/Bs

## v0.8.1 — 2026-07-02

- **Persistent outline cache**: repo_map outlines cached per repo root
  keyed by (mtime, size) — a hit skips the file read and the tree-sitter
  parse; best-effort JSON under the user data dir
- **Idle-time compaction**: the context-budget check also runs right after
  each turn, so pruning/summarizing happens while you read the reply
  instead of mid-turn while you wait
- **Hard-tier benchmark suite** (`bench/tasks2/`, `--dir tasks2`): 10
  harder tasks — multi-file symptom-not-location bugs, fix-the-failing-test
  with tamper guards, needle-in-a-haystack long-session modules, a
  cross-file signature refactor
- **First judge-accuracy results**: qwen3.6:35b judging gemma4:26b vs
  ornith:35b races went 14/14 on discriminative cases (see
  docs/BENCHMARKS.md)
- Community docs: CHANGELOG, CONTRIBUTING, issue templates
- Demo GIF in the README (reproducible VHS tape committed)
- Packaging: Homebrew formula generator + release-workflow tap
  automation, scoop manifest with autoupdate

## v0.8.0 — 2026-07-02 · v0.7 phase complete: cloud + swarm

- **Cross-provider WarpDrive**: one swarm race can mix providers — each
  candidate's model string (`gemma4:26b` vs `anthropic/claude-sonnet-4-6`)
  resolves through a provider factory to its own client
- **Swarm auto-judge** (`--judge <model>`): a referee model scores every
  candidate's diff and recommends a winner; TUI shows the verdict in the
  winner's log, `--no-tui` prints a machine-parseable `JUDGE: winner=` line
- **Cost display** for metered providers: `/stats` and the headless summary
  show estimated $; billed input tracked as summed per-call prompt tokens;
  built-in Anthropic rates plus a config `pricing` map
- `bench/judge_bench.py`: measures judge accuracy against verify-script
  ground truth (discriminative-case accuracy is the headline number)
- Swarm diffs exclude interpreter cache junk (`__pycache__`, `*.pyc`) that
  unfairly penalized candidates in judged races

## v0.7.2 — 2026-07-02

- First per-model prompt target shipped provisionally: `gemma.md` puts the
  tool-application contract first (from trace analysis: 8 of gemma4:26b's
  10 bench failures were chat-only answers)
- The apply-nudge now keys on *mutating* tool use (write/edit/bash), closing
  the explored-but-never-edited escape
- Fixed `t12_cents` bench task (was unpassable: float-representation trap)
- 3-model matrix results published in README/BENCHMARKS

## v0.7.1 — 2026-07-02

- **Turn traces + failure counters**: opt-in `--trace <file>` / `RIFT_TRACE`
  JSONL records model, tokens, tool calls, hardening/failure counters, and
  outcome per turn; `/stats` shows a recoveries line
- **Prompt-target machinery**: per-model-family system prompts compiled into
  the binary (`crates/rift-core/prompts/<family>.md`), selected by model
  name, user-overridable via `~/.config/rift/prompts/`
- **Bench model matrix**: `bench.py --models a,b,c` runs the suite per model
  and diffs pass rate / tokens / wall time; `--tasks` subset filter

## v0.7.0 — 2026-07-01 · cloud providers

- **Native Anthropic provider**: the Messages API spoken natively (SSE
  content blocks, tool_use/tool_result, thinking with signature round-trip);
  `anthropic/<model>` works with just `ANTHROPIC_API_KEY` in the env
- `openai/<model>` built-in provider over the existing OpenAI protocol
- Fenced code blocks render as labeled boxes in the TUI

## v0.6.4 — 2026-07-01

- Per-provider hardening test suite: deterministic mock-server tests in CI
  (SSE/NDJSON framing, mid-stream errors, tool-call accumulation,
  truncation detection) plus env-gated live suites against real servers

## v0.6.3 — 2026-07-01

- Completed the v0.5 UX scope: `@file` mentions with palette completion,
  syntax highlighting (syntect, feature-gated), live diff pane (Ctrl+D),
  built-in themes (`dark`/`light`/`mono`)

## v0.6.2 — 2026-07-01

- Config merge semantics: project `.rift.json` overlays the user config;
  permissions only tighten (deny-list union, approve stays on)
- `/mcp trust` management; transport retries

## v0.6.1 — 2026-07-01

- Hardening sweep: data safety, provider robustness, MCP trust gating for
  project-defined servers

## v0.6.0 — 2026-07-01 · provider abstraction

- `Provider` trait extracted — Ollama becomes one implementation
- **OpenAI-compatible provider**: vLLM, LM Studio, llama.cpp server,
  LiteLLM, OpenRouter; per-provider base URLs and keys in config
- Token accounting normalized across providers

## v0.5.1 — 2026-06-30

- Ctrl+C no longer exits the TUI (use `/quit`)

## v0.5.0 — 2026-06-30 · command + UX expansion

- New slash commands: `/retry`, `/stats`, `/system`, `/temp`, `/ctx`,
  `/worktrees`, `/save`, named `/sessions`, `/quit`
- Startup resilience: unreachable server or missing model opens the TUI
  anyway with a recovery hint

## v0.4.x — 2026-06-11 → 2026-06-28 · plan view + trust

- Plan tool with live activity-pane checklist; `ask_user` elicitation;
  interactive `/model` and `/sessions` pickers
- Approval mode (`--approve`): y/n gate on write/edit/bash with per-session
  always-allow
- Agent Skills standard (`.rift/skills/` SKILL.md), `/skill:<name>`
- RIFT.md / AGENTS.md / CLAUDE.md auto-loaded into the system prompt
- Full input-line editing (cursor movement, word jumps, bracketed paste)
- Cross-platform: Windows support (cmd.exe shell, USERPROFILE fallback,
  install.ps1), CI build/test matrix on macOS/Linux/Windows
- 50-task benchmark vs opencode: 44/50 vs 42/50, −57% prompt tokens,
  3.4× faster (see docs/BENCHMARKS.md)

## v0.3.x — 2026-06-11 · polish

- `rift update` self-updater with startup check and `/update`
- `/copy` command and Ctrl+T native-selection toggle
- Paste truncation, ANSI corruption, and false-hang fixes
- Model-failure harness: timeout salvage, server probe, edit hints

## v0.2.0 — 2026-06-11

- Slash-command palette popup

## v0.1.0 — 2026-06-11 · initial release

- Native Ollama `/api/chat` terminal coding agent: pre-wrapped flicker-free
  TUI, AST-based context compaction, textual tool-call recovery, doom-loop
  guard, WarpDrive worktree races, MCP, 16 slash commands
- Release pipeline: CI, cross-platform builds, one-line install script
