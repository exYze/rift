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

Unknown commands produce a `warning` event; malformed JSON lines likewise.

## Events (rift → stdout)

Turn flow:

| event | fields | notes |
|---|---|---|
| `iteration` | `n` | Agent-loop iteration counter within the turn. |
| `thinking` | `text` | Streamed reasoning (thinking models only). |
| `content` | `text` | Streamed assistant reply text. |
| `tool_start` | `name`, `args` | A tool call begins. `args` is the raw arguments object. |
| `tool_result` | `name`, `ok`, `preview` | Tool finished; `preview` is a short result excerpt. |
| `plan` | `items`: [{`text`,`done`}] | The agent's live checklist; replaces the previous plan. |
| `done` | `stats`: {`iterations`, `prompt_tokens`, `billed_prompt_tokens`, `output_tokens`, `duration_ms`, `tokens_per_sec`} | **Always ends a turn**, success or error. |

Interaction:

| event | fields | notes |
|---|---|---|
| `ask` | `id`, `question`, `detail`: [string], `choices`: [string] | Answer via the `answer` command. Empty `choices` = free text. `detail` carries context lines (e.g. a pending diff for approval prompts). Asks can arrive right after startup, before any turn — untrusted project-plugin manifests are offered this way (`choices: ["trust","skip"]`); answering `trust` registers the plugin's tools live and persists the approval. Ignoring the ask fails safe (skipped). |
| `edit_review` | `id`, `tool`, `path`, `old`, `new` | Only after `hello` with `edit_review:true`. Emitted BEFORE the write touches disk; decide via `edit_decision`. |
| `edit_review_closed` | `id` | The review is orphaned (turn ended/cancelled) — close its diff; a late decision would be a silent no-op. |

Session and status:

| event | fields | notes |
|---|---|---|
| `ready` | `model`, `session`, `cwd`, `version`, `protocol_version`, `num_ctx`, `skills`: [{`name`,`description`}] | First event after spawn. `skills` (added in 2.0, additive) lists what `/skill:<name>` prompts can invoke — skills and plugin commands — so consumers can offer completion. |
| `capabilities` | `protocol_version`, `edit_review` | Acknowledges `hello` with the effective capability set. |
| `context` | `used`, `limit` | Context-window occupancy; sent at startup and after each turn's idle compaction. |
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
