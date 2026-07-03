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

## HumanEval — gemma4:26b, baseline protocol vs rift agentic (2026-06-12)

All 164 problems, two protocols, same model and server (`bench/humaneval.py`):
the **baseline** follows the standard benchmark protocol (one completion, no
tools, no feedback — what official numbers report); **rift** gives the same
model its agent loop in a scratch repo where it can run the official tests and
fix its own mistakes.

| protocol | pass@1 | failed |
|---|---:|---|
| baseline one-shot | 162/164 = **98.8%** | #92, #145 |
| rift agentic | 162/164 = **98.8%** | #76, #145 |

Verdict: HumanEval is saturated at this model's level — there is no headroom
for a harness to demonstrate uplift (the ceiling is 1.2 points away). The
mechanism still showed itself: rift solved #92, which one-shot generation
fails, by running the tests and iterating. Demonstrating a 10–15% uplift
requires a benchmark with a non-saturated baseline (HumanEval+/EvalPlus or
harder agentic suites) — planned next.

## 3-model matrix (2026-07-02, rift v0.7.1, DGX Spark server)

First run of the model matrix (`bench.py --models a,b,c rift`): the same
50-task suite, rift only, three local models on a DGX Spark Ollama server,
wire-measured through the proxy. New in v0.7.1: every run also records a
per-turn JSONL trace (tool calls, hardening/failure counters, outcome) to
`bench/traces/` — the failure analysis below comes straight from them.

| metric (50 tasks) | gemma4:26b | ornith:35b | qwen3.6:35b |
|---|---:|---:|---:|
| tasks solved (as recorded) | 40/50 | 49/50 | 49/50 |
| tasks solved (corrected\*) | 40/50 | **50/50** | **50/50** |
| prompt tokens (wire) | 1,499,962 | **517,110** | 531,376 |
| output tokens | 73,019 | 32,564 | 32,444 |
| wall time | 28.1 min | 14.2 min | **11.5 min** |
| apply nudges / greedy retries | 36 / 13 | 0 / 0 | 0 / 0 |
| doom loops / tool errors | 15 / 13 | 1 / 1 | 0 / 11 |

\* `t12_cents` was unpassable as written: its verify asserted
`total([1.005, 2.0]) == 3.01`, reachable only via `Decimal` +
`ROUND_HALF_UP` semantics the prompt never asks for (1.005 is
1.00499… in binary floating point). Both 35B models had written the
canonical `round(sum(prices), 2)` and were marked failed. The verify was
fixed with binary-exact values and re-validated against the preserved run
dirs (pristine source still fails; both models' outputs pass).

What the traces showed: 8 of gemma4:26b's 10 failures were **chat-only
answers** — zero tool calls despite two apply-nudges and a temperature-0
retry — and the other two explored with read-only tools and never edited.
The recovery cycles are also where its 3× token bloat comes from. Two
changes came out of this run: the apply-nudge now keys on *mutating* tool
use (write/edit/bash) instead of any tool use, and gemma models get a
dedicated prompt target (`crates/rift-core/prompts/gemma.md`, provisional)
that puts the tool-application contract first. This run's 40/50 is the
baseline those changes are measured against.

Also surfaced: the suite is saturating for strong models (both 35Bs at
100% corrected) — the same lesson HumanEval taught above. Benchmark suite
v2 with a harder tier (multi-file, longer-horizon, compaction-exercising
tasks) is the planned fix (docs/ROADMAP.md, v0.8).

## Swarm judge accuracy (2026-07-02, rift v0.8.0, DGX Spark server)

First run of `bench/judge_bench.py`: on each of the 50 tasks, gemma4:26b
and ornith:35b raced in isolated worktrees, **qwen3.6:35b judged the two
diffs** (task + diffs + self-summaries; judged by the changes only), and
the harness then established ground truth the judge never saw — applying
each candidate's patch to a clean fixture and running verify.sh.

| bucket | result |
|---|---|
| discriminative (exactly one candidate passes) | **14/14 correct** |
| none-pass (neither passes; judge should say none) | 1/1 correct |
| all-pass (both pass; any pick acceptable) | 34/35 (one over-strict "none") |
| errors / unparsed verdicts | 0 |

The headline is the discriminative row: 14 cases where the judge had a
right answer and a coin flip scores 50%, and it went 14/14 (p ≈ 6×10⁻⁵
under the null). The failures it caught went both directions — sometimes
gemma was the broken one, sometimes ornith — and its picks weren't
position-locked (it selected the second-listed candidate repeatedly in
all-pass cases). Its single mistake all run was declaring "none" once
when both candidates were actually correct — the safe failure direction:
a false "none" means no auto-merge recommendation and the user reviews
manually; it never recommended a broken diff.

Along the way the judge also demonstrated the value of diff-level review:
in the live smoke test it docked a correct candidate for shipping
`__pycache__` noise in its patch — which exposed (and got fixed) a real
harness bug where run-artifacts leaked into swarm diffs.

Caveat: one judge, one racer pairing, one run — treat as a strong first
data point, not a leaderboard. `bench/judge_bench.py --dir tasks2` runs
the same protocol on the hard tier, where discriminative cases should be
more frequent.

## Hydrate-on-demand A/B (2026-07-02, hard tier, DGX Spark server)

The v0.8 context-engine changes (hydrate-on-demand reads + repo-map
centrality ranking) measured old-binary-vs-new on the hard-tier suite
(`bench/tasks2/`, 10 tasks), gemma4:26b, single run each, wire-measured:

| metric (10 hard tasks) | v0.8.1 (pre) | v0.8.2 (hydration) | delta |
|---|---:|---:|---|
| tasks solved | 10/10 | 10/10 | held |
| prompt tokens (wire) | 1,179,613 | 962,372 | **−18.4%** |
| output tokens | 47,586 | 44,423 | −6.6% |
| wall time | 16.3 min | 14.4 min | −11.4% |

Where it comes from: an unbounded `read` of a 500+ line source file now
returns the line-numbered outline instead of a 2000-line dump, and the
model then fetches exact ranges. On h09_bigmodule — one subtle bug in a
1,575-line module — that took the task from 265k prompt tokens / 148s to
89k / 90s (−66% tokens). Small-file tasks are unaffected by design;
per-task numbers off the main diagonal move within normal run-to-run
variance (single run per side — treat the suite total, not individual
tasks, as the signal).

Also notable: gemma4:26b went 10/10 on the hard tier in both runs — a
model that scored 40/50 on the *standard* suite before the v0.7.2
chat-only fixes (gemma prompt target + mutating-tool nudge). The hard
tier currently differentiates efficiency more than pass rate; pass-rate
headroom needs weaker models or harder tasks to show.

## Caveats (honest ones)

- Small suite, single run per task, nondeterministic models: treat as directional, not a rigorous eval. Observed run-to-run variance on the same task was ~±30% tokens for opencode (40k–71k on t1).
- Tasks are small and single-purpose; they don't measure long-session behavior (where Rift's compaction should widen the gap, but that's unmeasured here).
- Wall time on a shared single-GPU server includes prompt-processing time, which itself scales with prompt size — the speed win is partly a consequence of the token win.
- opencode went 5/5 here once configured correctly; the widely-reported "bad outputs with Ollama" failures are mostly configuration/`num_ctx` pitfalls that Rift eliminates by design rather than model capability.

## Harness bugs found while benchmarking (kept for honesty)

1. opencode trusts the `$PWD` env var over the real cwd for project-root resolution. Python's `subprocess(cwd=…)` doesn't update `PWD`, so early opencode runs rooted themselves in the *harness* directory and edited the benchmark fixtures. Fixed by setting `PWD` explicitly; fixtures restored from canonical content and re-verified broken before the final run.
2. opencode silently exits 0 (no output, no error) on `ProviderModelNotFoundError`; a custom provider colliding with the user's global `ollama` provider config triggered it. Fixed with a uniquely-named provider + per-task `$HOME` isolation.
3. Early invalid runs were discarded; only runs with verified-broken fixtures and confirmed temp-dir-only edits are reported above.
