#!/usr/bin/env python3
"""GhostWriter benchmark harness.

Runs each agent over the task suite in bench/tasks/. Every task is copied to
a fresh temp git repo; the agent gets the task prompt headless; success is
decided by the task's verify.sh; token counts come from the recording proxy
(proxy.py), measured identically for every agent from the wire.

Usage:
  python3 bench.py [agent ...]        # default: ghostwriter opencode
  (start proxy.py first)
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
TOKEN_LOG = "/tmp/gw-bench-tokens.jsonl"
PROXY = "http://127.0.0.1:11435"
GW = os.path.join(ROOT, "..", "target", "release", "gw")
MODEL = "gemma4:26b"
TIMEOUT = 600

OPENCODE_CONFIG = {
    "$schema": "https://opencode.ai/config.json",
    "provider": {
        "benchprox": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Bench proxy",
            "options": {"baseURL": f"{PROXY}/v1"},
            "models": {MODEL: {"name": MODEL}},
        }
    },
    "permission": {"edit": "allow", "bash": "allow"},
}


def agent_cmd(agent, prompt):
    if agent == "ghostwriter":
        return [GW, "--host", PROXY, "--model", MODEL, "--prompt", prompt]
    if agent == "opencode":
        return ["opencode", "run", "-m", f"benchprox/{MODEL}", prompt]
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


def run_task(agent, task):
    src = os.path.join(ROOT, "tasks", task)
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
            json.dump(OPENCODE_CONFIG, f)

    # opencode's first invocation in a brand-new project dir exits silently
    # (project initialization); warm it up untimed, before the token mark.
    if agent == "opencode":
        try:
            subprocess.run(agent_cmd(agent, "warmup: reply ok"), cwd=tmp,
                           capture_output=True, timeout=120, env=env,
                           stdin=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            pass

    mark = line_count(TOKEN_LOG)
    t0 = time.time()
    timed_out = False
    try:
        proc = subprocess.run(
            agent_cmd(agent, prompt), cwd=tmp, capture_output=True, text=True,
            timeout=TIMEOUT, env=env, stdin=subprocess.DEVNULL,
        )
        rc = proc.returncode
        tail = (proc.stdout + proc.stderr)[-400:]
    except subprocess.TimeoutExpired:
        rc, tail, timed_out = -1, "(timeout)", True
    secs = round(time.time() - t0, 1)

    ok = subprocess.run(["bash", os.path.join(src, "verify.sh")], cwd=tmp,
                        capture_output=True).returncode == 0
    res = {"agent": agent, "task": task, "ok": ok, "secs": secs, "rc": rc,
           "timeout": timed_out, **tokens_since(TOKEN_LOG, mark), "dir": tmp}
    res["tail"] = tail
    return res


def main():
    agents = sys.argv[1:] or ["ghostwriter", "opencode"]
    tasks = sorted(d for d in os.listdir(os.path.join(ROOT, "tasks"))
                   if os.path.isdir(os.path.join(ROOT, "tasks", d)))
    results = []
    for agent in agents:
        for task in tasks:
            print(f"=== {agent} / {task} ...", flush=True)
            r = run_task(agent, task)
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

    print("\nagent        task          ok   secs  prompt   out  calls")
    for r in results:
        print(f"{r['agent']:<12} {r['task']:<13} {str(r['ok']):<5}{r['secs']:>6} "
              f"{r['prompt_tok']:>7} {r['output_tok']:>5} {r['llm_calls']:>5}")
    for agent in agents:
        rs = [r for r in results if r["agent"] == agent]
        ok = sum(r["ok"] for r in rs)
        print(f"{agent}: {ok}/{len(rs)} ok, total prompt={sum(r['prompt_tok'] for r in rs)}, "
              f"out={sum(r['output_tok'] for r in rs)}, secs={round(sum(r['secs'] for r in rs),1)}")


if __name__ == "__main__":
    main()
