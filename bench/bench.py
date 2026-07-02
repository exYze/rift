#!/usr/bin/env python3
"""Rift benchmark harness.

Runs each agent over the task suite in bench/tasks/. Every task is copied to
a fresh temp git repo; the agent gets the task prompt headless; success is
decided by the task's verify.sh; token counts come from the recording proxy
(proxy.py), measured identically for every agent from the wire.

Usage:
  python3 bench.py [agent ...]                   # default: rift opencode
  python3 bench.py --models qwen3:27b,gemma4:26b rift   # model matrix
  (start proxy.py first)

With --models the whole suite runs once per model and the summary diffs
pass rate / prompt tokens / wall time per model — the fitness function for
per-model prompt and schema experiments (docs/ROADMAP.md, "Model targets").
Rift runs always record per-turn JSONL traces under bench/traces/.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
TOKEN_LOG = "/tmp/rift-bench-tokens.jsonl"
PROXY = "http://127.0.0.1:11435"
RIFT = os.path.join(ROOT, "..", "target", "release", "rift")
MODEL = "gemma4:26b"
TIMEOUT = 600


def opencode_config(model):
    return {
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "benchprox": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Bench proxy",
                "options": {"baseURL": f"{PROXY}/v1"},
                "models": {model: {"name": model}},
            }
        },
        "permission": {"edit": "allow", "bash": "allow"},
    }


def trace_path(model):
    safe = re.sub(r"[^A-Za-z0-9._-]", "-", model)
    return os.path.join(ROOT, "traces", f"{safe}.jsonl")


def agent_cmd(agent, prompt, model):
    if agent == "rift":
        return [RIFT, "--host", PROXY, "--model", model,
                "--trace", trace_path(model), "--prompt", prompt]
    if agent == "opencode":
        return ["opencode", "run", "-m", f"benchprox/{model}", prompt]
    raise SystemExit(f"unknown agent {agent}")


def line_count(path):
    try:
        with open(path) as f:
            return sum(1 for _ in f)
    except FileNotFoundError:
        return 0


def tokens_since(path, mark):
    prompt = output = calls = 0
    try:
        with open(path) as f:
            for i, line in enumerate(f):
                if i < mark:
                    continue
                e = json.loads(line)
                prompt += e.get("prompt", 0)
                output += e.get("output", 0)
                calls += 1
    except FileNotFoundError:
        pass
    return {"prompt_tok": prompt, "output_tok": output, "llm_calls": calls}


def run_task(agent, task, model, tasks_dir="tasks"):
    src = os.path.join(ROOT, tasks_dir, task)
    tmp = tempfile.mkdtemp(prefix=f"bench-{agent}-{task}-")
    for f in os.listdir(src):
        if f in ("prompt.txt", "verify.sh") or f.startswith("__"):
            continue
        if os.path.isfile(os.path.join(src, f)):
            shutil.copy(os.path.join(src, f), tmp)
    subprocess.run(["git", "init", "-q"], cwd=tmp, check=True)
    subprocess.run(["git", "add", "-A"], cwd=tmp, check=True)
    subprocess.run(
        ["git", "-c", "user.email=b@b", "-c", "user.name=b", "commit", "-qm", "init"],
        cwd=tmp, check=True, capture_output=True,
    )

    with open(os.path.join(src, "prompt.txt")) as f:
        prompt = f.read().strip()

    env = dict(os.environ, OPENCODE_DISABLE_AUTOUPDATE="1", PWD=tmp)
    if agent == "opencode":
        # Full isolation: throwaway HOME per task (fresh config/db/session
        # state — opencode otherwise bleeds project state across runs), with
        # the provider config discovered normally from the project cwd.
        home = os.path.join(tmp, ".bench-home")
        os.makedirs(home, exist_ok=True)
        for var in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
            env[var] = home
        with open(os.path.join(tmp, "opencode.json"), "w") as f:
            json.dump(opencode_config(model), f)

    # opencode's first invocation in a brand-new project dir exits silently
    # (project initialization); warm it up untimed, before the token mark.
    if agent == "opencode":
        try:
            subprocess.run(agent_cmd(agent, "warmup: reply ok", model), cwd=tmp,
                           capture_output=True, timeout=120, env=env,
                           stdin=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            pass

    mark = line_count(TOKEN_LOG)
    t0 = time.time()
    timed_out = False
    try:
        proc = subprocess.run(
            agent_cmd(agent, prompt, model), cwd=tmp, capture_output=True, text=True,
            timeout=TIMEOUT, env=env, stdin=subprocess.DEVNULL,
        )
        rc = proc.returncode
        tail = (proc.stdout + proc.stderr)[-400:]
    except subprocess.TimeoutExpired:
        rc, tail, timed_out = -1, "(timeout)", True
    secs = round(time.time() - t0, 1)

    ok = subprocess.run(["bash", os.path.join(src, "verify.sh")], cwd=tmp,
                        capture_output=True).returncode == 0
    res = {"agent": agent, "model": model, "task": task, "ok": ok, "secs": secs,
           "rc": rc, "timeout": timed_out, **tokens_since(TOKEN_LOG, mark), "dir": tmp}
    res["tail"] = tail
    return res


def summarize(results, agents, models):
    print("\nagent        model            task          ok   secs  prompt   out  calls")
    for r in results:
        print(f"{r['agent']:<12} {r['model']:<16} {r['task']:<13} {str(r['ok']):<5}{r['secs']:>6} "
              f"{r['prompt_tok']:>7} {r['output_tok']:>5} {r['llm_calls']:>5}")
    # Per (agent, model) totals — with multiple models this is the matrix
    # that model-specific prompt/schema experiments diff against.
    print("\nagent        model              ok    prompt      out    secs")
    for agent in agents:
        for model in models:
            rs = [r for r in results if r["agent"] == agent and r["model"] == model]
            if not rs:
                continue
            ok = sum(r["ok"] for r in rs)
            print(f"{agent:<12} {model:<16} {ok:>2}/{len(rs):<3} "
                  f"{sum(r['prompt_tok'] for r in rs):>9} {sum(r['output_tok'] for r in rs):>8} "
                  f"{round(sum(r['secs'] for r in rs), 1):>7}")


def main():
    ap = argparse.ArgumentParser(description="Rift benchmark harness")
    ap.add_argument("agents", nargs="*", default=None,
                    help="agents to run (default: rift opencode)")
    ap.add_argument("--models", default=MODEL,
                    help="comma-separated models; the suite runs once per model")
    ap.add_argument("--tasks", default=None,
                    help="comma-separated task names to run (default: all) — "
                         "for quick prompt/schema experiments before a full run")
    ap.add_argument("--dir", default="tasks",
                    help="task directory under bench/ (e.g. tasks2 for the hard tier)")
    args = ap.parse_args()
    agents = args.agents or ["rift", "opencode"]
    models = [m.strip() for m in args.models.split(",") if m.strip()]

    os.makedirs(os.path.join(ROOT, "traces"), exist_ok=True)
    tasks = sorted(d for d in os.listdir(os.path.join(ROOT, args.dir))
                   if os.path.isdir(os.path.join(ROOT, args.dir, d)))
    if args.tasks:
        wanted = [t.strip() for t in args.tasks.split(",") if t.strip()]
        missing = [t for t in wanted if t not in tasks]
        if missing:
            raise SystemExit(f"unknown tasks: {', '.join(missing)}")
        tasks = wanted
    results = []
    for model in models:
        for agent in agents:
            for task in tasks:
                print(f"=== {agent} / {model} / {task} ...", flush=True)
                r = run_task(agent, task, model, args.dir)
                print(f"    ok={r['ok']} {r['secs']}s prompt={r['prompt_tok']} "
                      f"out={r['output_tok']} calls={r['llm_calls']}", flush=True)
                results.append(r)

    out = os.path.join(ROOT, "results.json")
    existing = []
    if os.path.exists(out):
        with open(out) as f:
            existing = json.load(f)
    with open(out, "w") as f:
        json.dump(existing + results, f, indent=2)

    summarize(results, agents, models)


if __name__ == "__main__":
    main()
