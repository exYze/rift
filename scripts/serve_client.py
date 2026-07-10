#!/usr/bin/env python3
"""Minimal reference client for the rift serve protocol (docs/SERVE.md).

Interactive: type a prompt, watch the event stream; answer asks inline.
Also the smallest possible integration example — spawn, hello, prompt,
read events line by line, ignore what you don't know.

  python3 scripts/serve_client.py [--model M] [--rift path/to/rift] [--edit-review]
"""
import argparse
import json
import subprocess
import sys
import threading

DIM, RESET, BOLD, YELLOW = "\033[2m", "\033[0m", "\033[1m", "\033[33m"


def reader(proc, state):
    """Print each event as it arrives; remember open asks/reviews."""
    for line in proc.stdout:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = ev.get("event")
        if kind == "ready":
            print(f"{DIM}ready: {ev.get('model')} · protocol v{ev.get('protocol_version', 1)} "
                  f"· rift {ev.get('version')} · session {ev.get('session')}{RESET}")
        elif kind == "capabilities":
            print(f"{DIM}capabilities: edit_review={ev.get('edit_review')}{RESET}")
        elif kind == "content":
            print(ev.get("text", ""), end="", flush=True)
        elif kind == "thinking":
            print(f"{DIM}{ev.get('text', '')}{RESET}", end="", flush=True)
        elif kind == "tool_start":
            print(f"\n{DIM}⚙ {ev.get('name')} {json.dumps(ev.get('args', {}))[:120]}{RESET}")
        elif kind == "tool_result":
            mark = "✓" if ev.get("ok") else "✗"
            print(f"{DIM}{mark} {ev.get('name')}: {str(ev.get('preview', ''))[:120]}{RESET}")
        elif kind == "ask":
            state["ask"] = ev["id"]
            choices = ev.get("choices") or []
            print(f"\n{YELLOW}? {ev.get('question')}{RESET}")
            for i, c in enumerate(choices, 1):
                print(f"  {i}. {c}")
            print(f"{DIM}(answer with: :a <text or choice number>){RESET}")
        elif kind == "edit_review":
            state["review"] = ev["id"]
            print(f"\n{YELLOW}edit_review #{ev['id']}: {ev.get('path')}{RESET}")
            print(f"{DIM}(decide with :y / :n){RESET}")
        elif kind == "edit_review_closed":
            state.pop("review", None)
        elif kind == "done":
            s = ev.get("stats", {})
            print(f"\n{DIM}[done: {s.get('iterations')} iter, {s.get('prompt_tokens')} prompt tok, "
                  f"{s.get('output_tokens')} out tok]{RESET}")
            print(f"{BOLD}> {RESET}", end="", flush=True)
        elif kind in ("info", "warning"):
            print(f"\n{DIM}{kind}: {ev.get('text')}{RESET}")
        # Everything else (context, plan, history, subagent_*, task_*):
        # a real integration renders these; the reference client ignores
        # them, which is also legal per the protocol.
    print(f"\n{DIM}(rift exited){RESET}")


def main():
    ap = argparse.ArgumentParser(description="rift serve protocol reference client")
    ap.add_argument("--rift", default="rift", help="rift binary (default: from PATH)")
    ap.add_argument("--model", default=None)
    ap.add_argument("--edit-review", action="store_true",
                    help="opt into inline diff review (decide with :y / :n)")
    args, extra = ap.parse_known_args()

    cmd = [args.rift, "--serve"] + (["--model", args.model] if args.model else []) + extra
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    state = {}
    threading.Thread(target=reader, args=(proc, state), daemon=True).start()
    send({"cmd": "hello", "edit_review": args.edit_review})

    print(f"{BOLD}> {RESET}", end="", flush=True)
    try:
        for line in sys.stdin:
            line = line.rstrip("\n")
            if not line.strip():
                continue
            if line.startswith(":a "):
                send({"cmd": "answer", "id": state.pop("ask", 0), "text": line[3:]})
            elif line == ":y":
                send({"cmd": "edit_decision", "id": state.pop("review", 0), "apply": True})
            elif line == ":n":
                send({"cmd": "edit_decision", "id": state.pop("review", 0), "apply": False})
            elif line == ":cancel":
                send({"cmd": "cancel"})
            elif line == ":undo":
                send({"cmd": "undo"})
            elif line == ":quit":
                break
            else:
                send({"cmd": "prompt", "text": line})
    except KeyboardInterrupt:
        pass
    proc.stdin.close()
    proc.wait(timeout=10)


if __name__ == "__main__":
    main()
