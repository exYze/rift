#!/usr/bin/env python3
"""The prompt evolution gate (docs/ROADMAP.md, v1.8).

Prompts are code: a new family file, or a change to an existing one, merges
only if it beats the incumbent on the bench matrix. This script runs both
sides of that comparison and prints a PR-ready verdict:

  python3 scripts/prompt_gate.py --family qwen --candidate my-qwen.md \
      --models qwen3:32b[,more] [--dir tasks] [--runs 2] [--schema rich]

What it does, per model:
  1. Incumbent run: the embedded prompt (any user override for the family is
     moved aside first so the binary's own target is measured).
  2. Candidate run: the candidate file installed as the user-level override
     (~/.config/rift/prompts/<family>.md) — matched before embedded targets,
     no rebuild needed.
  3. Compares pass rate, prompt tokens, wall time, and the failure counters
     from bench/traces/ (textual recoveries, apply nudges, doom loops…) —
     a prompt that halves recoveries wins even at equal pass rate.

Exit code 0 = candidate wins or ties on pass rate with fewer model-failure
interventions; 1 = incumbent holds. Both runs' numbers are printed for the
PR description either way. Requires bench/proxy.py running and a release
rift binary (RIFT_BIN or target/release/rift), like bench.py itself.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "bench", "bench.py")
RESULTS = os.path.join(ROOT, "bench", "results.json")
TRACES = os.path.join(ROOT, "bench", "traces")

# The FailureCounters fields that signal model misbehavior (trace.rs
# model_failures() — compaction counters deliberately excluded).
FAILURE_KEYS = [
    "truncations", "textual_recoveries", "alias_hits", "doom_loop_trips",
    "unknown_tools", "tool_errors", "apply_nudges", "greedy_retries",
    "template_strips",
]


def override_path(family):
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return os.path.join(base, "rift", "prompts", f"{family}.md")


def results_mark():
    if not os.path.exists(RESULTS):
        return 0
    with open(RESULTS) as f:
        return len(json.load(f))


def trace_marks(models):
    marks = {}
    for m in models:
        safe = re.sub(r"[^A-Za-z0-9._-]", "-", m)
        p = os.path.join(TRACES, f"{safe}.jsonl")
        marks[m] = (p, sum(1 for _ in open(p)) if os.path.exists(p) else 0)
    return marks


def run_bench(models, dir_, runs, schema):
    cmd = [sys.executable, BENCH, "--models", ",".join(models),
           "--dir", dir_, "--runs", str(runs), "--schema", schema, "rift"]
    print(f"$ {' '.join(cmd)}", flush=True)
    subprocess.run(cmd, check=True)


def collect(models, res_mark, marks):
    """Summarize the results and traces appended since the marks."""
    with open(RESULTS) as f:
        new = json.load(f)[res_mark:]
    out = {}
    for m in models:
        rs = [r for r in new if r.get("model") == m]
        counters = dict.fromkeys(FAILURE_KEYS, 0)
        path, mark = marks[m]
        if os.path.exists(path):
            with open(path) as f:
                for i, line in enumerate(f):
                    if i < mark:
                        continue
                    fails = json.loads(line).get("failures", {})
                    for k in FAILURE_KEYS:
                        counters[k] += fails.get(k, 0)
        out[m] = {
            "passed": sum(1 for r in rs if r.get("ok")),
            "total": len(rs),
            "prompt_tok": sum(r.get("prompt_tok", 0) for r in rs),
            "secs": round(sum(r.get("secs", 0) for r in rs), 1),
            "failures": sum(counters.values()),
            "counters": {k: v for k, v in counters.items() if v},
        }
    return out


def main():
    ap = argparse.ArgumentParser(description="prompt evolution gate")
    ap.add_argument("--family", required=True, help="target family (file stem)")
    ap.add_argument("--candidate", required=True, help="candidate prompt .md")
    ap.add_argument("--models", required=True, help="comma-separated models")
    ap.add_argument("--dir", default="tasks", help="task tier (tasks, tasks2)")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--schema", choices=["rich", "lean"], default="rich")
    args = ap.parse_args()
    models = [m.strip() for m in args.models.split(",") if m.strip()]

    override = override_path(args.family)
    backup = override + ".gate-backup"
    os.makedirs(os.path.dirname(override), exist_ok=True)
    had_override = os.path.exists(override)
    if had_override:
        shutil.move(override, backup)

    try:
        print(f"--- incumbent: embedded '{args.family}' target ---", flush=True)
        res_mark, marks = results_mark(), trace_marks(models)
        run_bench(models, args.dir, args.runs, args.schema)
        incumbent = collect(models, res_mark, marks)

        print(f"--- candidate: {args.candidate} as user override ---", flush=True)
        shutil.copy(args.candidate, override)
        res_mark, marks = results_mark(), trace_marks(models)
        run_bench(models, args.dir, args.runs, args.schema)
        candidate = collect(models, res_mark, marks)
    finally:
        if os.path.exists(override):
            os.remove(override)
        if had_override:
            shutil.move(backup, override)

    wins = 0
    print(f"\n=== gate verdict: {args.family} ===")
    for m in models:
        i, c = incumbent[m], candidate[m]
        print(f"{m}:")
        print(f"  incumbent: {i['passed']}/{i['total']} pass, {i['prompt_tok']} prompt tok, "
              f"{i['secs']}s, {i['failures']} interventions {i['counters']}")
        print(f"  candidate: {c['passed']}/{c['total']} pass, {c['prompt_tok']} prompt tok, "
              f"{c['secs']}s, {c['failures']} interventions {c['counters']}")
        better = (c["passed"], -c["failures"], -c["prompt_tok"]) > (
            i["passed"], -i["failures"], -i["prompt_tok"])
        wins += better
        print(f"  -> {'CANDIDATE' if better else 'incumbent holds'}")

    if wins == len(models):
        print("\nGATE PASSED — candidate wins on every model. Move it into "
              "crates/rift-core/prompts/, register it in EMBEDDED, and paste "
              "the numbers above into the PR.")
        return 0
    print(f"\nGATE FAILED — candidate won {wins}/{len(models)} models; the "
          "incumbent stays.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
