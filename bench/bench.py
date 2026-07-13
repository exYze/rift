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

Windows notes (each of these cost a full suite run to learn):
- run from Git Bash, not PowerShell — verify.sh needs Git's bash, and
  PowerShell resolves `bash` to WSL's (find_bash() below compensates)
- npm's `opencode` shim isn't CreateProcess-launchable; point OPENCODE_BIN
  at the real exe (node_modules/opencode-*/bin/opencode.exe)
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
# Shared with proxy.py (same env var / same default) — a drive-relative
# "/tmp" diverges between processes on Windows.
TOKEN_LOG = os.environ.get("RIFT_BENCH_TOKENS") or os.path.join(
    tempfile.gettempdir(), "rift-bench-tokens.jsonl"
)
# Overridden by --proxy. A /v1 suffix makes rift speak the OpenAI
# protocol through the proxy (vLLM/LM Studio upstreams); a bare host
# keeps the native Ollama protocol.
PROXY = "http://127.0.0.1:11435"
RIFT = os.environ.get(
    "RIFT_BIN",
    os.path.join(ROOT, "..", "target", "release", "rift.exe" if os.name == "nt" else "rift"),
)
OPENCODE = os.environ.get("OPENCODE_BIN", "opencode")
MODEL = "gemma4:26b"
TIMEOUT = 600
KEEP_DIRS = False  # set by --keep-dirs; default is to clean task dirs up


def find_bash():
    """The bash that runs verify.sh. On Windows, `bash` on PATH is often
    WSL's (System32) — which can't run the task scripts — so prefer the one
    Git for Windows ships next to git itself."""
    if os.name != "nt":
        return "bash"
    git = shutil.which("git")
    if git:
        cand = os.path.join(os.path.dirname(os.path.dirname(git)), "usr", "bin", "bash.exe")
        if os.path.isfile(cand):
            return cand
    return shutil.which("bash") or "bash"


BASH = find_bash()


def clean_dir(path):
    """Best-effort rmtree that shrugs off Windows read-only files (git
    objects) — 300 leftover task dirs once filled a whole drive."""
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                os.chmod(os.path.join(root, f), 0o700)
            except OSError:
                pass
    shutil.rmtree(path, ignore_errors=True)


def opencode_config(model):
    return {
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "benchprox": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Bench proxy",
                "options": {"baseURL": PROXY.rstrip("/") if PROXY.rstrip("/").endswith("/v1") else PROXY.rstrip("/") + "/v1"},
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
        return [OPENCODE, "run", "-m", f"benchprox/{model}", prompt]
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
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    # concurrent proxy appends can interleave a blank/partial
                    # line; skip it rather than lose the whole suite
                    continue
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
        # Explicit utf-8 on the pipes: Windows defaults text mode to cp1252,
        # and one un-decodable byte kills the reader thread — which then
        # deadlocks the child on a full pipe until the timeout.
        proc = subprocess.run(
            agent_cmd(agent, prompt, model), cwd=tmp, capture_output=True, text=True,
            encoding="utf-8", errors="replace",
            timeout=TIMEOUT, env=env, stdin=subprocess.DEVNULL,
        )
        rc = proc.returncode
        tail = (proc.stdout + proc.stderr)[-400:]
    except subprocess.TimeoutExpired:
        rc, tail, timed_out = -1, "(timeout)", True
    secs = round(time.time() - t0, 1)

    ok = subprocess.run([BASH, os.path.join(src, "verify.sh")], cwd=tmp,
                        capture_output=True).returncode == 0
    res = {"agent": agent, "model": model, "task": task, "ok": ok, "secs": secs,
           "rc": rc, "timeout": timed_out, **tokens_since(TOKEN_LOG, mark), "dir": tmp}
    res["tail"] = tail
    if not KEEP_DIRS:
        clean_dir(tmp)  # opencode's per-task HOME once filled a whole drive
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
            # Multi-run variance: per-run pass counts + flaky tasks (passed
            # in some runs, failed in others) — the run-to-run noise floor
            # any claimed improvement has to clear.
            runs = sorted({r.get("run", 0) for r in rs})
            if len(runs) > 1:
                per_run = [sum(r["ok"] for r in rs if r.get("run", 0) == run) for run in runs]
                by_task = {}
                for r in rs:
                    by_task.setdefault(r["task"], []).append(r["ok"])
                flaky = sorted(t for t, oks in by_task.items() if len(set(oks)) > 1)
                print(f"{'':<12} {'':<16} per-run: {per_run} "
                      f"(min {min(per_run)}, max {max(per_run)}); "
                      f"flaky: {', '.join(flaky) if flaky else 'none'}")


def main():
    global PROXY
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
    ap.add_argument("--runs", type=int, default=1,
                    help="repetitions per task — nondeterministic models need "
                         "multi-run variance, not single-run point estimates")
    ap.add_argument("--proxy", default=PROXY,
                    help="recording-proxy URL agents talk to (start proxy.py "
                         "first). Append /v1 when the upstream is an "
                         "OpenAI-style server (vLLM, LM Studio): rift then "
                         "speaks the OpenAI protocol through the proxy")
    ap.add_argument("--schema", choices=["rich", "lean"], default="rich",
                    help="tool-schema variant for rift runs (RIFT_TOOL_SCHEMA): "
                         "lean = first-sentence descriptions, no per-param docs — "
                         "the schema A/B for the model matrix")
    ap.add_argument("--keep-dirs", action="store_true",
                    help="keep each task's temp dir for post-mortems (default: "
                         "cleaned after verify — 300 kept dirs once filled a drive)")
    args = ap.parse_args()
    global KEEP_DIRS
    KEEP_DIRS = args.keep_dirs
    PROXY = args.proxy.rstrip("/")
    os.environ["RIFT_TOOL_SCHEMA"] = args.schema
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
    out = os.path.join(ROOT, "results.json")
    existing = []
    if os.path.exists(out):
        with open(out) as f:
            existing = json.load(f)
    results = []
    for model in models:
        for agent in agents:
            for run in range(args.runs):
                for task in tasks:
                    tag = f" run {run + 1}/{args.runs}" if args.runs > 1 else ""
                    print(f"=== {agent} / {model} / {task}{tag} ...", flush=True)
                    r = run_task(agent, task, model, args.dir)
                    r["run"] = run
                    r["schema"] = args.schema
                    print(f"    ok={r['ok']} {r['secs']}s prompt={r['prompt_tok']} "
                          f"out={r['output_tok']} calls={r['llm_calls']}", flush=True)
                    results.append(r)
                    # flush after every task: a crash mid-suite must not
                    # lose the results already collected
                    with open(out, "w") as f:
                        json.dump(existing + results, f, indent=2)

    summarize(results, agents, models)


if __name__ == "__main__":
    main()
