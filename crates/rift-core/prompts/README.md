# Prompt targets

Each model family is a compiler target (docs/ROADMAP.md, v0.8.5 "Model
targets"). Open models differ dramatically in what prompting they want —
one hardcoded prompt for every model leaves performance on the table.

Files here are embedded into the binary at compile time (`include_str!` in
`src/prompts.rs` — the one-binary promise holds). Format:

```markdown
---
family: qwen
match: qwen, qwen3
---
You are Rift, ... {cwd} ... {shell}
```

- `family` — the target's name (defaults to the file stem).
- `match` — comma-separated lowercase substrings tried against the model
  name (`qwen3.6:27b` matches `qwen`). First matching target wins; no match
  falls back to `default`. `default.md` has no `match` — it IS the fallback.
- `{cwd}` and `{shell}` are the only placeholders.

## Adding or changing a family — the evolution gate

Prompts are code. A new family file, or a change to an existing one, merges
only if it **beats the incumbent on the bench matrix**:

1. Experiment without recompiling: put your candidate in
   `~/.config/rift/prompts/<family>.md` — user-level overrides are matched
   before embedded targets (there is deliberately no project-level override:
   a cloned repo must never be able to replace the system prompt).
2. Run the matrix: `python3 bench/bench.py --models <model,...> rift`
   with the incumbent, then with the candidate.
3. Compare pass rate, prompt tokens, wall time, and the failure counters in
   `bench/traces/` (textual recoveries, alias hits, doom loops — a prompt
   that halves recoveries is a win even at equal pass rate).
4. If the candidate wins, move it into this directory, register it in
   `EMBEDDED` in `src/prompts.rs`, and record both runs' numbers in the PR.

Keep every target short — each token counts against `num_ctx` on local
models.
