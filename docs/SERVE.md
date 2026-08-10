# The serve protocol (v1)

`rift --serve` speaks a line-oriented JSON protocol over stdio for editor
integrations: **one event object per stdout line, one command object per
stdin line**, stderr reserved for human-readable diagnostics. The process
exits when stdin closes. This document is the contract — the VS Code
extension is the reference consumer, and `scripts/serve_client.py` is a
minimal reference client for new integrations.

## Versioning

The `ready` event carries `protocol_version` (currently **1**). The rules:

- **Additive changes are not breaking.** New events, and new fields on
  existing events, may appear in any rift release. Consumers MUST ignore
  events and fields they don't recognize.
- **Breaking changes bump `protocol_version`** — removing or renaming an
  event/field, or changing a field's type or semantics. Within protocol
  v1, everything documented here keeps working.
- Capabilities are opt-in via `hello` (below); a consumer that never sends
  `hello` gets the v1 baseline behavior.

## Lifecycle

```
spawn: rift --serve [--model M] [--continue | --resume FILE] ...
  ← {"event":"ready", "model":…, "session":…, "cwd":…, "version":…,
     "protocol_version":1, "num_ctx":…}
  ← {"event":"context", "used":…, "limit":…}
  ← {"event":"history", "messages":[{"role":"user"|"assistant","text":…},…]}
        (only when resuming a session with prior conversation)
  → {"cmd":"hello", "edit_review":true}        (optional, once, at spawn)
  ← {"event":"capabilities", "protocol_version":1, "edit_review":true}
  … turns …
stdin EOF → process exits (in-flight turn cancelled, session saved)
```

## Commands (stdin → rift)

| command | fields | notes |
|---|---|---|
| `hello` | `edit_review`: bool | Capability handshake. Idempotent; send once at spawn. Answered with a `capabilities` event. |
| `prompt` | `text`: string | Starts a turn. Rejected with a `warning` event while a turn is running. Text beginning `/skill:<name> [task]` expands that skill (or plugin command) exactly like the TUI; an unknown name is a `warning`, not a turn. |
| `answer` | `id`: number, `text`: string | Answers a pending `ask` by id. Unknown ids are ignored. |
| `edit_decision` | `id`: number, `apply`: bool, `content`?: string | Decides a pending `edit_review`. `apply:true` writes `content` (the accepted-hunk subset the reviewer assembled) or, when `content` is absent, the proposal verbatim. `apply:false` rejects. |
| `cancel` | — | Cancels the in-flight turn. Pending reviews are closed (`edit_review_closed`). |
| `undo` | — | Reverts the last turn's write/edit changes (bash changes aren't tracked). Rejected with a `warning` mid-turn. |
| `list_sessions` | — | Requests the saved-session index for a history/reopen picker. Answered with a `sessions` event. Read-only — safe to send any time, including mid-turn. Reopen a chosen session by relaunching with `--resume <path>`. |
| `list_models` | — | (added in 2.6.3) Requests every model rift can currently reach — the default host's list plus each configured provider's, as `provider/model` strings ready for `set_model`. Answered with a `models` event; unreachable servers are skipped after a short probe. Safe any time, including mid-turn. |
| `set_model` | `model`: string | (added in 2.6.3) Live model switch on the same conversation — the preflight-and-swap of the TUI's `/model` (tools-capability check, num_ctx clamp/adopt). Acked with `model_changed` (+ a fresh `context` event); failure is a `warning`. Rejected with a `warning` mid-turn. |
| `set_approval` | `approve`: bool | (added in 2.6.6) Approval mode — the TUI's `/approve` … `/yolo` over the wire. `approve:false` stops the prompts: write/edit/bash apply as the model calls them, and no `edit_review` is raised (there is nothing left to decide). NOT a permission bypass — `deny` rules still refuse and `ask` rules still prompt. Acked with `approval_changed`; a missing/non-boolean `approve` is a `warning`. Safe mid-turn (it is one shared flag, read per gated action); a prompt already on screen still needs its answer. |

Unknown commands produce a `warning` event; malformed JSON lines likewise.
Feature-detect via `commands` in `ready` rather than probing — an unknown
command's `warning` surfaces in the user's chat.

## Events (rift → stdout)

Turn flow:

| event | fields | notes |
|---|---|---|
| `iteration` | `n` | Agent-loop iteration counter within the turn. |
| `thinking` | `text` | Streamed reasoning (thinking models only). |
| `content` | `text` | Streamed assistant reply text. |
| `tool_start` | `name`, `args` | A tool call begins. `args` is the raw arguments object. |
| `tool_result` | `name`, `ok`, `preview` | Tool finished; `preview` is a short result excerpt. |
| `edit_diff` | `path`, `added`, `removed`, `diff`: [string] | A write/edit was applied (added in 2.6, additive): `diff` is a capped ± preview of the change (`@@ line N @@` context markers, `-old`, `+new` lines), `added`/`removed` precomputed for slim headers. Emitted right after the corresponding `tool_result`. |
| `plan` | `items`: [{`text`,`done`}] | The agent's live checklist; replaces the previous plan. |
| `done` | `stats`: {`iterations`, `prompt_tokens`, `billed_prompt_tokens`, `output_tokens`, `duration_ms`, `tokens_per_sec`} | **Always ends a turn**, success or error. |

Interaction:

| event | fields | notes |
|---|---|---|
| `ask` | `id`, `question`, `detail`: [string], `choices`: [string] | Answer via the `answer` command. Empty `choices` = free text. `detail` carries context lines (e.g. a pending diff for approval prompts). Asks can arrive right after startup, before any turn — untrusted project-plugin manifests are offered this way (`choices: ["trust","skip"]`); answering `trust` registers the plugin's tools live and persists the approval. Ignoring the ask fails safe (skipped). |
| `edit_review` | `id`, `tool`, `path`, `old`, `new`, `segments` | Only after `hello` with `edit_review:true`. Emitted BEFORE the write touches disk; decide via `edit_decision`. `segments` (added in 2.6.3, additive) is rift's own hunking of old → new — alternating `{"same":true,"lines":[…]}` and `{"same":false,"old":[…],"new":[…]}` runs. Review each change segment as one hunk and reassemble accepted content by concatenating `lines`/chosen sides; no consumer-side diff needed. Degrades to a single whole-file change segment past the edit-distance cap. |
| `edit_review_closed` | `id` | The review is orphaned (turn ended/cancelled) — close its diff; a late decision would be a silent no-op. |

Session and status:

| event | fields | notes |
|---|---|---|
| `ready` | `model`, `session`, `cwd`, `version`, `protocol_version`, `num_ctx`, `approve`, `skills`: [{`name`,`description`}], `commands`: [string] | First event after spawn. `skills` (added in 2.0, additive) lists what `/skill:<name>` prompts can invoke — skills and plugin commands — so consumers can offer completion. `commands` (added in 2.6.3, additive) is the command set this build accepts — gate optional UI (model picker, live switch) on it. `approve` (added in 2.6.6, additive) is the approval mode rift started with (config + `--approve`), so a consumer's toggle opens in the right state. |
| `capabilities` | `protocol_version`, `edit_review`, `approve` | Acknowledges `hello` with the effective capability set. `approve` (added in 2.6.6) echoes the current approval mode. |
| `context` | `used`, `limit` | Context-window occupancy; sent at startup and after each turn's idle compaction. |
| `sessions` | `items`: [{`path`, `title`, `saved_at`, `cwd`, `model`, `turns`}] | Answer to `list_sessions` (added in 2.4, additive). Every saved session, newest first; `title` is the first user message, `saved_at` a Unix timestamp. Reopen one with `--resume <path>`. |
| `models` | `models`: [string], `current` | Answer to `list_models` (added in 2.6.3, additive). Every reachable model in `set_model`-ready form; `current` is the addressable name of the model in use. |
| `model_changed` | `model`, `num_ctx`, `note` | A `set_model` succeeded (added in 2.6.3, additive): same conversation, new model. `note` describes any context-budget adjustment (may be empty); a `context` event follows with the fresh gauge. |
| `approval_changed` | `approve` | A `set_approval` landed (added in 2.6.6, additive). `approve:false` = edits and commands now apply without asking. Worth surfacing in the transcript: it changes what happens to the user's files without further confirmation. |
| `history` | `messages`: [{`role`,`text`}] | Prior user/assistant exchanges of a resumed session (tool/system traffic excluded). |
| `info` / `warning` | `text` | Human-relevant notices. |
| `subagent_started` | `tag`, `model`, `label` | A concurrent sub-agent began. |
| `subagent` | `tag`, `text`, `warn` | Sub-agent activity line. |
| `subagent_finished` | `tag`, `steps` | Sub-agent completed. |
| `task_started` / `task_finished` | `id`, `label` (+ `ok`, `preview` on finish) | Background bash/agent tasks. |

## Semantics worth building against

- **One turn at a time.** `prompt` while busy → `warning`, not a queue.
- **One id space.** `ask` and `edit_review` ids come from the same counter;
  ids are never reused within a process.
- **Orphaned reviews are closed, not leaked.** On `done` or `cancel`, every
  undecided `edit_review` gets an `edit_review_closed`.
- **Dropped replies mean "deny".** Exiting (or ignoring an ask forever)
  fails safe.
- **Session autosave** happens after every turn; `session` in `ready` is
  the file path.

## Writing an integration (Neovim, JetBrains, …)

1. Spawn `rift --serve` with the flags you'd pass the TUI (`--model`,
   `--continue`, …). Read stdout line by line; parse each as JSON.
2. Send `hello` immediately if you implement inline diff review; otherwise
   skip it and approvals arrive as plain `ask` events.
3. Render `content` deltas as they stream; treat `done` as end-of-turn.
4. Ignore anything you don't recognize (that's what keeps you forward-
   compatible within v1).
5. Test against `scripts/serve_client.py` behavior and the conformance
   tests in `crates/rift-tui/src/serve.rs` — they pin these wire shapes.

Try it interactively:

```
python3 scripts/serve_client.py --model qwen3:32b
```
