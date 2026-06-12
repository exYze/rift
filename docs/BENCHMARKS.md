# Rift vs opencode — token efficiency benchmark

**Date:** 2026-06-11 · **Model:** gemma4:26b (Q4_K_M) · **Server:** a private single-GPU Ollama server on the local network
**Baseline:** opencode v1.14.48 (installed via Homebrew) · **Rift:** v0.1.0 release build (pre-rename, then called GhostWriter)

## Headline

| metric (5-task suite) | Rift | opencode | delta |
|---|---:|---:|---|
| tasks solved | **5/5** | 5/5 | tied |
| prompt tokens (total) | **54,431** | 277,110 | **−80.4%** (5.1× fewer) |
| output tokens (total) | **4,480** | 7,049 | −36% |
| wall time (total) | **128.8s** | 300.4s | **−57%** (2.3× faster) |
| LLM calls | 33 | 32 | ~equal |

Equal task success, **80% fewer prompt tokens and 2.3× faster** — far past the ≥10% efficiency target. The win is structural: Rift's system prompt + tool schemas cost ~1.5k tokens per call and old tool outputs are pruned by the Compactor; opencode re-sends a ~9–10k-token preamble on every call.

## Per-task results

| task | rift ok | rift secs | rift prompt | oc ok | oc secs | oc prompt |
|---|---|---:|---:|---|---:|---:|
| t1_offbyone (fix arithmetic bug) | ✓ | 12.0 | 8,700 | ✓ | 38.0 | 40,492 |
| t2_default (honor default arg) | ✓ | 13.3 | 9,814 | ✓ | 37.0 | 40,676 |
| t3_feature (implement from docstring) | ✓ | 13.2 | 7,536 | ✓ | 47.7 | 50,750 |
| t4_multifile (fix across two files) | ✓ | 54.7 | 14,655 | ✓ | 71.8 | 61,965 |
| t5_rename (cross-file refactor) | ✓ | 35.6 | 13,726 | ✓ | 105.9 | 83,227 |

## Methodology

- **Suite:** 5 small Python repos with planted bugs/features (`bench/tasks/`), each verified by a `verify.sh` that fails before the fix and passes after. Fixtures confirmed broken before every run.
- **Isolation:** each run gets a fresh temp git repo (copy of the fixture). For opencode, additionally a throwaway `$HOME` per task so no global config/session state leaks (see harness notes).
- **Token counting:** a recording reverse proxy (`bench/proxy.py`) sits between the agent and Ollama and logs `prompt_eval_count`/`eval_count` (native API) and `usage.*` (OpenAI-compat) from the wire — both tools measured identically, no self-reported stats.
- **Both agents:** same model, same server, same prompts, headless one-shot mode, 600s timeout. opencode's first-run project initialization is excluded via an untimed warmup call before the token mark.
- **Reproduce:** `python3 bench/proxy.py & python3 bench/bench.py rift opencode`

## Re-validation (2026-06-11, v0.3.5 harness hardening)

After adding the model-failure harness (timeout output salvage, dev-server probe,
edit/read failure hints), the suite was re-run to guard the efficiency claim:
**5/5 ok, 69,562 prompt tokens (−74.9% vs opencode's 277,110), 95.8s wall** (3.1×
faster than opencode's 300s). The prompt-token delta vs the original 54k run is
within the ±30% run-to-run variance noted below. The harness fixes target failure
modes this small suite doesn't exercise (dev servers, fuzzy edit misses); their
payoff shows on real-world agentic sessions.

## 50-task suite (2026-06-12, rift v0.4.1 hardened vs opencode v1.14.48)

![50-task results](assets/benchmark-50.svg)

The suite grew from 5 to 50 tasks (`bench/tasks/`): planted arithmetic/logic/string
bugs, collection handling, edge cases, implement-from-docstring, multi-file fixes,
and refactors — every task verified by a script that fails pre-fix (self-checked).

| metric (50 tasks) | rift | opencode | delta |
|---|---:|---:|---|
| tasks solved | **44/50** | 42/50 | +2 |
| prompt tokens (wire) | **1,250,659** | 2,926,446 | **−57.3%** |
| output tokens | **68,420** | 103,708 | −34% |
| wall time | **26.8 min** | 90.8 min | **3.4× faster** |
| LLM calls | 501 | 435 | +15% |

What the run surfaced (and fixed — kept for honesty): at gemma's server-default
temperature the same task alternated between a clean agentic run and a "chat-only"
answer (the model pastes the fix into its reply and never edits the file). Three
harness layers now address it — temperature pinned to 0.2 by default, a one-shot
"apply your changes with the tools" nudge, and (headless/swarm work items only) a
full-turn greedy retry at temperature 0. Those layers took rift from 35/50 to
44/50 on this suite with zero per-task tuning; opencode's number is from the same
session, same wire measurement. rift's 6 remaining failures are genuine model
mistakes (wrong logic that passes the model's own reading but not the hidden
tests); opencode's 8 include one 600s timeout.

## Caveats (honest ones)

- Small suite, single run per task, nondeterministic models: treat as directional, not a rigorous eval. Observed run-to-run variance on the same task was ~±30% tokens for opencode (40k–71k on t1).
- Tasks are small and single-purpose; they don't measure long-session behavior (where Rift's compaction should widen the gap, but that's unmeasured here).
- Wall time on a shared single-GPU server includes prompt-processing time, which itself scales with prompt size — the speed win is partly a consequence of the token win.
- opencode went 5/5 here once configured correctly; the widely-reported "bad outputs with Ollama" failures are mostly configuration/`num_ctx` pitfalls that Rift eliminates by design rather than model capability.

## Harness bugs found while benchmarking (kept for honesty)

1. opencode trusts the `$PWD` env var over the real cwd for project-root resolution. Python's `subprocess(cwd=…)` doesn't update `PWD`, so early opencode runs rooted themselves in the *harness* directory and edited the benchmark fixtures. Fixed by setting `PWD` explicitly; fixtures restored from canonical content and re-verified broken before the final run.
2. opencode silently exits 0 (no output, no error) on `ProviderModelNotFoundError`; a custom provider colliding with the user's global `ollama` provider config triggered it. Fixed with a uniquely-named provider + per-task `$HOME` isolation.
3. Early invalid runs were discarded; only runs with verified-broken fixtures and confirmed temp-dir-only edits are reported above.
