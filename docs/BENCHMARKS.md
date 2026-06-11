# GhostWriter vs opencode — token efficiency benchmark

**Date:** 2026-06-11 · **Model:** gemma4:26b (Q4_K_M) · **Server:** Ollama at localhost:11434 (single GPU)
**Baseline:** opencode v1.14.48 (installed via Homebrew) · **GhostWriter:** v0.1.0 release build

## Headline

| metric (5-task suite) | GhostWriter | opencode | delta |
|---|---:|---:|---|
| tasks solved | **5/5** | 5/5 | tied |
| prompt tokens (total) | **54,431** | 277,110 | **−80.4%** (5.1× fewer) |
| output tokens (total) | **4,480** | 7,049 | −36% |
| wall time (total) | **128.8s** | 300.4s | **−57%** (2.3× faster) |
| LLM calls | 33 | 32 | ~equal |

Equal task success, **80% fewer prompt tokens and 2.3× faster** — far past the ≥10% efficiency target. The win is structural: GhostWriter's system prompt + tool schemas cost ~1.5k tokens per call and old tool outputs are pruned by the Compactor; opencode re-sends a ~9–10k-token preamble on every call.

## Per-task results

| task | gw ok | gw secs | gw prompt | oc ok | oc secs | oc prompt |
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
- **Reproduce:** `python3 bench/proxy.py & python3 bench/bench.py ghostwriter opencode`

## Re-validation (2026-06-11, v0.3.5 harness hardening)

After adding the model-failure harness (timeout output salvage, dev-server probe,
edit/read failure hints), the suite was re-run to guard the efficiency claim:
**5/5 ok, 69,562 prompt tokens (−74.9% vs opencode's 277,110), 95.8s wall** (3.1×
faster than opencode's 300s). The prompt-token delta vs the original 54k run is
within the ±30% run-to-run variance noted below. The harness fixes target failure
modes this small suite doesn't exercise (dev servers, fuzzy edit misses); their
payoff shows on real-world agentic sessions.

## Caveats (honest ones)

- Small suite, single run per task, nondeterministic models: treat as directional, not a rigorous eval. Observed run-to-run variance on the same task was ~±30% tokens for opencode (40k–71k on t1).
- Tasks are small and single-purpose; they don't measure long-session behavior (where GhostWriter's compaction should widen the gap, but that's unmeasured here).
- Wall time on a shared single-GPU server includes prompt-processing time, which itself scales with prompt size — the speed win is partly a consequence of the token win.
- opencode went 5/5 here once configured correctly; the widely-reported "bad outputs with Ollama" failures are mostly configuration/`num_ctx` pitfalls that GhostWriter eliminates by design rather than model capability.

## Harness bugs found while benchmarking (kept for honesty)

1. opencode trusts the `$PWD` env var over the real cwd for project-root resolution. Python's `subprocess(cwd=…)` doesn't update `PWD`, so early opencode runs rooted themselves in the *harness* directory and edited the benchmark fixtures. Fixed by setting `PWD` explicitly; fixtures restored from canonical content and re-verified broken before the final run.
2. opencode silently exits 0 (no output, no error) on `ProviderModelNotFoundError`; a custom provider colliding with the user's global `ollama` provider config triggered it. Fixed with a uniquely-named provider + per-task `$HOME` isolation.
3. Early invalid runs were discarded; only runs with verified-broken fixtures and confirmed temp-dir-only edits are reported above.
