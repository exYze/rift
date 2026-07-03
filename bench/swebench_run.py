#!/usr/bin/env python3
"""Run rift against SWE-bench Verified and emit a predictions file for the
official swebench evaluation harness.

Per instance: check out the repo at base_commit (one cached clone per repo,
a detached worktree per instance), give rift the issue headless, capture
`git diff` as the prediction. Nothing from the gold patch or hints ever
reaches the agent.

  python3 swebench_run.py --host http://SERVER:11434 --model ornith:35b \\
      --instances 30 [--seed 0] [--timeout 900] [--ids id1,id2]

Predictions append to swebench/predictions_<model>.jsonl (resumable: done
instances are skipped). Evaluate with:

  python -m swebench.harness.run_evaluation \\
      --dataset_name princeton-nlp/SWE-bench_Verified \\
      --predictions_path swebench/predictions_<model>.jsonl \\
      --run_id rift-<model> --max_workers 4
"""
import argparse
import json
import os
import random
import re
import subprocess
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
RIFT = os.environ.get("RIFT_BIN", os.path.join(ROOT, "..", "target", "release", "rift"))
WORK = os.path.join(ROOT, "swebench")

PROMPT = """You are working at the root of the {repo} repository. Fix the bug described in this GitHub issue:

<issue>
{problem}
</issue>

Requirements:
- Find the root cause and make the minimal source change that fixes it.
- Do NOT modify any test files, and do not add new tests.
- Verify your change compiles/imports cleanly if practical (running the full test suite is too slow here — targeted checks only).
- When done, summarize the change briefly."""


def sh(args, cwd=None, timeout=None, check=True):
    p = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if check and p.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed: {p.stderr[-400:]}")
    return p


def repo_clone(repo):
    path = os.path.join(WORK, "repos", repo.replace("/", "__"))
    if not os.path.isdir(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        print(f"    cloning {repo} (once, cached)...", flush=True)
        sh(["git", "clone", "--quiet", f"https://github.com/{repo}.git", path], timeout=1800)
    return path


def make_worktree(clone, sha, name):
    wt = os.path.join(WORK, "worktrees", name)
    if os.path.isdir(wt):
        sh(["git", "-C", clone, "worktree", "remove", "--force", wt], check=False)
    os.makedirs(os.path.dirname(wt), exist_ok=True)
    try:
        sh(["git", "-C", clone, "worktree", "add", "--detach", wt, sha])
    except RuntimeError:
        # Commit not in the cached clone (repo moved on) — fetch and retry.
        sh(["git", "-C", clone, "fetch", "--quiet", "origin"], timeout=1800)
        sh(["git", "-C", clone, "worktree", "add", "--detach", wt, sha])
    return wt


def drop_worktree(clone, wt):
    sh(["git", "-C", clone, "worktree", "remove", "--force", wt], check=False)


def capture_patch(wt):
    """The prediction: staged diff vs base_commit, junk excluded, test files
    reverted (the prompt forbids touching them; enforcing it keeps the
    prediction honest rather than silently benefiting from test edits)."""
    sh(["git", "-C", wt, "add", "-A"])
    # Revert modifications to test files.
    ls = sh(["git", "-C", wt, "diff", "--cached", "--name-only"]).stdout.splitlines()
    tests = [f for f in ls if re.search(r"(^|/)(tests?|testing)/|(^|/)test_|_test\.py$", f)]
    if tests:
        sh(["git", "-C", wt, "reset", "--quiet", "--"] + tests, check=False)
        sh(["git", "-C", wt, "checkout", "--quiet", "--"] + tests, check=False)
    p = sh(["git", "-C", wt, "diff", "--cached", "--binary", "--",
            ".", ":(exclude)__pycache__", ":(exclude)*.pyc", ":(exclude).pytest_cache"])
    return p.stdout


def main():
    ap = argparse.ArgumentParser(description="rift on SWE-bench Verified")
    ap.add_argument("--host", default="http://localhost:11434")
    ap.add_argument("--model", required=True)
    ap.add_argument("--instances", type=int, default=30, help="subset size (seeded sample)")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--ids", default=None, help="explicit comma-separated instance_ids (overrides --instances)")
    ap.add_argument("--timeout", type=int, default=900, help="seconds per instance")
    ap.add_argument("--max-iterations", type=int, default=40)
    args = ap.parse_args()

    from datasets import load_dataset
    ds = load_dataset("princeton-nlp/SWE-bench_Verified", split="test")
    by_id = {i["instance_id"]: i for i in ds}
    if args.ids:
        picked = [by_id[x.strip()] for x in args.ids.split(",") if x.strip()]
    else:
        ids = sorted(by_id)
        random.Random(args.seed).shuffle(ids)
        picked = [by_id[x] for x in ids[: args.instances]]

    safe_model = re.sub(r"[^A-Za-z0-9._-]", "-", args.model)
    os.makedirs(WORK, exist_ok=True)
    pred_path = os.path.join(WORK, f"predictions_{safe_model}.jsonl")
    done = set()
    if os.path.exists(pred_path):
        with open(pred_path) as f:
            done = {json.loads(line)["instance_id"] for line in f if line.strip()}

    trace = os.path.join(ROOT, "traces", f"swe-{safe_model}.jsonl")
    os.makedirs(os.path.dirname(trace), exist_ok=True)

    for n, inst in enumerate(picked, 1):
        iid = inst["instance_id"]
        if iid in done:
            print(f"[{n}/{len(picked)}] {iid} — already done, skipping", flush=True)
            continue
        print(f"[{n}/{len(picked)}] {iid} ({inst['repo']})", flush=True)
        clone = repo_clone(inst["repo"])
        prompt = PROMPT.format(repo=inst["repo"], problem=inst["problem_statement"])
        t0 = time.time()
        patch, status = "", "ok"
        # One retry on a failed empty run: transient provider errors (server
        # waking, connection blips) otherwise burn the instance.
        for attempt in (1, 2):
            wt = make_worktree(clone, inst["base_commit"], iid)
            rc = 0
            try:
                proc = subprocess.run(
                    [RIFT, "--host", args.host, "--model", args.model,
                     "--max-iterations", str(args.max_iterations),
                     "--trace", trace, "--prompt", prompt],
                    cwd=wt, capture_output=True, text=True,
                    timeout=args.timeout, stdin=subprocess.DEVNULL,
                )
                rc = proc.returncode
            except subprocess.TimeoutExpired:
                status = "timeout"  # keep whatever diff exists — partial credit is real
            patch = capture_patch(wt)
            drop_worktree(clone, wt)
            if patch.strip() or (rc == 0 and status != "timeout"):
                if rc != 0:
                    status = f"rc={rc}"
                break
            status = f"retrying (rc={rc})"
            print(f"    attempt {attempt} failed with rc={rc} and no patch — retrying", flush=True)
        secs = round(time.time() - t0, 1)
        with open(pred_path, "a") as f:
            f.write(json.dumps({
                "instance_id": iid,
                "model_name_or_path": f"rift-{safe_model}",
                "model_patch": patch,
            }) + "\n")
        print(f"    {status} {secs}s patch={'yes' if patch.strip() else 'EMPTY'} "
              f"({len(patch)} bytes)", flush=True)

    print(f"\npredictions: {pred_path} ({len(picked)} requested)")


if __name__ == "__main__":
    main()
