#!/usr/bin/env python3
"""HumanEval pass@1: baseline one-shot vs rift agentic, same model.

Baseline = the standard benchmark protocol (one completion, no tools, no
feedback). Rift = the same model driven by rift's agent loop in a temp dir
where it can run the tests and fix its own mistakes. The delta is the value
of the harness, not the model.

Usage:
  python3 humaneval.py [--mode baseline|rift|both] [--limit N] [--offset K]
"""
import argparse
import gzip
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(ROOT, "HumanEval.jsonl.gz")
DATA_URL = "https://github.com/openai/human-eval/raw/master/data/HumanEval.jsonl.gz"
RIFT = os.path.join(ROOT, "..", "target", "release", "rift")
HOST = os.environ.get("RIFT_HOST", "http://100.102.217.61:11434")
MODEL = os.environ.get("RIFT_MODEL", "gemma4:26b")
RESULTS = os.path.join(ROOT, "humaneval-results.jsonl")
RUN_TIMEOUT = 12  # seconds per test execution


def load_problems():
    if not os.path.exists(DATA):
        print(f"downloading {DATA_URL}")
        urllib.request.urlretrieve(DATA_URL, DATA)
    out = []
    with gzip.open(DATA, "rt") as f:
        for line in f:
            if line.strip():
                out.append(json.loads(line))
    return out


def run_program(code, cwd=None):
    """Execute a candidate program; True iff it exits 0 within the timeout."""
    try:
        r = subprocess.run([sys.executable, "-c", code], capture_output=True,
                           timeout=RUN_TIMEOUT, cwd=cwd, text=True)
        return r.returncode == 0, (r.stderr or "")[-300:]
    except subprocess.TimeoutExpired:
        return False, "(timeout)"
    except Exception as e:  # noqa: BLE001
        return False, str(e)


def extract_code(text):
    """Best-effort: fenced block first, else raw text."""
    blocks = re.findall(r"```(?:python)?\n(.*?)```", text, re.DOTALL)
    if blocks:
        # Prefer the block that defines a function.
        for b in blocks:
            if "def " in b:
                return b
        return blocks[0]
    return text


def chat_once(prompt):
    body = json.dumps({
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
        "think": False,
        "options": {"temperature": 0.2, "num_ctx": 8192, "num_predict": 1200},
    }).encode()
    req = urllib.request.Request(f"{HOST}/api/chat", data=body,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        return json.loads(resp.read())["message"]["content"]


def baseline_one(prob):
    """Standard one-shot protocol: complete the function, no tools, no retry."""
    prompt = (
        "Complete the following Python function. Reply with ONLY a ```python code "
        "block containing the complete, working function (including the signature). "
        "No explanations.\n\n```python\n" + prob["prompt"] + "```"
    )
    t0 = time.time()
    try:
        reply = chat_once(prompt)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "secs": round(time.time() - t0, 1), "err": str(e)[:200]}
    code = extract_code(reply)
    # If the model returned only a body, prepend the official prompt.
    candidate = code if f"def {prob['entry_point']}" in code else prob["prompt"] + code
    program = candidate + "\n\n" + prob["test"] + f"\ncheck({prob['entry_point']})\n"
    ok, err = run_program(program)
    return {"ok": ok, "secs": round(time.time() - t0, 1), "err": err if not ok else ""}


def rift_one(prob):
    """Agentic protocol: same model, but it can run the tests and self-fix."""
    tmp = tempfile.mkdtemp(prefix="he-rift-")
    with open(os.path.join(tmp, "solution.py"), "w") as f:
        f.write(prob["prompt"] + "    pass  # TODO: implement\n")
    with open(os.path.join(tmp, "run_tests.py"), "w") as f:
        f.write(
            f"from solution import {prob['entry_point']}\n\n"
            + prob["test"]
            + f"\ncheck({prob['entry_point']})\nprint('ALL TESTS PASS')\n"
        )
    subprocess.run(["git", "init", "-q"], cwd=tmp, check=True)
    prompt = (
        f"Implement the function `{prob['entry_point']}` in solution.py according to its "
        "docstring. Then run `python3 run_tests.py` with the bash tool to verify, and keep "
        "fixing until every test passes."
    )
    t0 = time.time()
    try:
        subprocess.run(
            [RIFT, "--host", HOST, "--model", MODEL, "--prompt", prompt,
             "--max-iterations", "12", "--num-ctx", "8192"],
            cwd=tmp, capture_output=True, timeout=420, stdin=subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired:
        pass
    secs = round(time.time() - t0, 1)
    with open(os.path.join(tmp, "solution.py")) as f:
        solution = f.read()
    program = solution + "\n\n" + prob["test"] + f"\ncheck({prob['entry_point']})\n"
    ok, err = run_program(program, cwd=tmp)
    return {"ok": ok, "secs": secs, "err": err if not ok else ""}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", default="both", choices=["baseline", "rift", "both"])
    ap.add_argument("--limit", type=int, default=164)
    ap.add_argument("--offset", type=int, default=0)
    args = ap.parse_args()

    problems = load_problems()[args.offset:args.offset + args.limit]
    modes = ["baseline", "rift"] if args.mode == "both" else [args.mode]

    done = set()
    if os.path.exists(RESULTS):
        with open(RESULTS) as f:
            for line in f:
                e = json.loads(line)
                done.add((e["mode"], e["task_id"]))

    for mode in modes:
        for prob in problems:
            key = (mode, prob["task_id"])
            if key in done:
                continue
            fn = baseline_one if mode == "baseline" else rift_one
            r = fn(prob)
            rec = {"mode": mode, "task_id": prob["task_id"], **r}
            with open(RESULTS, "a") as f:
                f.write(json.dumps(rec) + "\n")
            print(f"{mode:<9} {prob['task_id']:<14} ok={r['ok']} {r['secs']}s", flush=True)

    # Summary
    with open(RESULTS) as f:
        rows = [json.loads(l) for l in f if l.strip()]
    for mode in ("baseline", "rift"):
        rs = [r for r in rows if r["mode"] == mode]
        if rs:
            ok = sum(r["ok"] for r in rs)
            print(f"{mode}: {ok}/{len(rs)} pass@1 = {ok/len(rs)*100:.1f}%")


if __name__ == "__main__":
    main()
