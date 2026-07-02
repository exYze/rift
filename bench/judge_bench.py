#!/usr/bin/env python3
"""Judge-accuracy benchmark: how good is the swarm auto-judge?

For each task, race two models with `rift swarm --judge` in a fresh git
repo, then establish ground truth the judge never sees: apply each
candidate's patch to a clean copy and run the task's verify.sh. Compare
the judge's pick against reality.

The interesting cases are the DISCRIMINATIVE ones — exactly one candidate
passes verify. There the judge has a right answer; picking it is signal,
50% is coin-flip. When both pass, any pick is fine; when neither passes,
the correct verdict is "none".

Usage:
  python3 judge_bench.py --host http://SERVER:11434 \\
      --models gemma4:26b,ornith:35b --judge qwen3.6:35b \\
      [--tasks t1_offbyone,t11_fizz] [--timeout 900]
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile

ROOT = os.path.dirname(os.path.abspath(__file__))
RIFT = os.path.join(ROOT, "..", "target", "release", "rift")


def fresh_repo(task_src, prefix):
    tmp = tempfile.mkdtemp(prefix=prefix)
    for f in os.listdir(task_src):
        if f in ("prompt.txt", "verify.sh") or f.startswith("__"):
            continue
        if os.path.isfile(os.path.join(task_src, f)):
            shutil.copy(os.path.join(task_src, f), tmp)
    subprocess.run(["git", "init", "-q"], cwd=tmp, check=True)
    subprocess.run(["git", "add", "-A"], cwd=tmp, check=True)
    subprocess.run(
        ["git", "-c", "user.email=b@b", "-c", "user.name=b", "commit", "-qm", "init"],
        cwd=tmp, check=True, capture_output=True,
    )
    return tmp


def verify_patch(task_src, patch_path):
    """Ground truth: does this candidate's patch pass the task's verify.sh?"""
    tmp = fresh_repo(task_src, "judge-verify-")
    try:
        if patch_path and os.path.exists(patch_path):
            rc = subprocess.run(["git", "apply", "--3way", patch_path], cwd=tmp,
                                capture_output=True).returncode
            if rc != 0:
                return False
        elif patch_path is None:
            pass  # no changes — verify the pristine (broken) code
        ok = subprocess.run(["bash", os.path.join(task_src, "verify.sh")], cwd=tmp,
                            capture_output=True).returncode == 0
        return ok
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def run_task(task, host, models, judge, timeout, tasks_dir="tasks"):
    src = os.path.join(ROOT, tasks_dir, task)
    with open(os.path.join(src, "prompt.txt")) as f:
        prompt = f.read().strip()
    repo = fresh_repo(src, f"judge-{task}-")

    cmd = [RIFT, "--host", host, "swarm", prompt,
           "--models", models, "--judge", judge, "--no-tui"]
    try:
        proc = subprocess.run(cmd, cwd=repo, capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)
        out = proc.stdout + proc.stderr
    except subprocess.TimeoutExpired:
        return {"task": task, "error": "timeout"}

    m = re.search(r"^JUDGE: winner=(\S+)", out, re.MULTILINE)
    pick = m.group(1) if m else "(no verdict line)"

    # Ground truth per candidate: patches live under .rift/patches/<name>.patch;
    # a candidate with no patch made no changes and cannot pass a broken fixture.
    candidates = {}
    for i, model in enumerate(models.split(",")):
        safe = "".join(c if c.isalnum() or c in ".-" else "-" for c in model.strip())
        name = f"{i}-{safe}"
        patch = os.path.join(repo, ".rift", "patches", f"{name}.patch")
        candidates[name] = verify_patch(src, patch if os.path.exists(patch) else None)

    passing = [n for n, ok in candidates.items() if ok]
    if len(passing) == 1:
        kind = "discriminative"
        correct = pick == passing[0]
    elif len(passing) == len(candidates):
        kind = "all_pass"
        correct = pick in candidates  # any real pick is acceptable
    else:
        kind = "none_pass"
        correct = pick == "none"

    shutil.rmtree(repo, ignore_errors=True)
    return {"task": task, "candidates": candidates, "judge_pick": pick,
            "kind": kind, "judge_correct": correct}


def main():
    ap = argparse.ArgumentParser(description="Swarm judge accuracy benchmark")
    ap.add_argument("--host", default="http://localhost:11434")
    ap.add_argument("--models", required=True, help="two models to race, comma-separated")
    ap.add_argument("--judge", required=True, help="referee model")
    ap.add_argument("--tasks", default=None, help="comma-separated subset (default: all)")
    ap.add_argument("--dir", default="tasks",
                    help="task directory under bench/ (e.g. tasks2 for the hard tier)")
    ap.add_argument("--timeout", type=int, default=900, help="seconds per task")
    args = ap.parse_args()

    tasks = sorted(d for d in os.listdir(os.path.join(ROOT, args.dir))
                   if os.path.isdir(os.path.join(ROOT, args.dir, d)))
    if args.tasks:
        wanted = [t.strip() for t in args.tasks.split(",") if t.strip()]
        missing = [t for t in wanted if t not in tasks]
        if missing:
            raise SystemExit(f"unknown tasks: {', '.join(missing)}")
        tasks = wanted

    results = []
    for task in tasks:
        print(f"=== {task} ...", flush=True)
        r = run_task(task, args.host, args.models, args.judge, args.timeout, args.dir)
        results.append(r)
        if "error" in r:
            print(f"    ERROR: {r['error']}", flush=True)
        else:
            truth = " ".join(f"{n}={'pass' if ok else 'FAIL'}" for n, ok in r["candidates"].items())
            print(f"    {truth} | judge={r['judge_pick']} | {r['kind']} | "
                  f"{'CORRECT' if r['judge_correct'] else 'WRONG'}", flush=True)

    out = os.path.join(ROOT, "judge-results.json")
    existing = []
    if os.path.exists(out):
        with open(out) as f:
            existing = json.load(f)
    with open(out, "w") as f:
        json.dump(existing + [{"judge": args.judge, "models": args.models,
                               "results": results}], f, indent=2)

    scored = [r for r in results if "error" not in r]
    disc = [r for r in scored if r["kind"] == "discriminative"]
    nonep = [r for r in scored if r["kind"] == "none_pass"]
    allp = [r for r in scored if r["kind"] == "all_pass"]
    print(f"\njudge: {args.judge} on {args.models}")
    print(f"tasks scored: {len(scored)} ({len(results) - len(scored)} errored)")
    if disc:
        ok = sum(r["judge_correct"] for r in disc)
        print(f"discriminative (one candidate passes): {ok}/{len(disc)} correct "
              f"— the headline number; 50% = coin flip")
    if nonep:
        ok = sum(r["judge_correct"] for r in nonep)
        print(f"none-pass (judge should say none):     {ok}/{len(nonep)} correct")
    if allp:
        print(f"all-pass (any pick acceptable):        {len(allp)} tasks")


if __name__ == "__main__":
    main()
