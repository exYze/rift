#!/usr/bin/env python3
"""Token-recording reverse proxy for Ollama.

Sits between any coding agent and the Ollama server, records the
prompt/output token counts every response reports (native NDJSON
`prompt_eval_count`/`eval_count` and OpenAI-compat `usage.prompt_tokens`/
`completion_tokens`) to a JSONL file. This measures every tool identically,
from the wire — no trust in self-reported stats.

Usage: proxy.py [port] [upstream] [logfile]
"""
import json
import re
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 11435
UPSTREAM = sys.argv[2] if len(sys.argv) > 2 else "http://localhost:11434"
LOGFILE = sys.argv[3] if len(sys.argv) > 3 else "/tmp/gw-bench-tokens.jsonl"

PROMPT_RE = re.compile(rb'"(?:prompt_eval_count|prompt_tokens)":\s*(\d+)')
OUTPUT_RE = re.compile(rb'"(?:eval_count|completion_tokens)":\s*(\d+)')


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # connection-close framing keeps it simple

    def log_message(self, *args):
        pass

    def _forward(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        req = urllib.request.Request(UPSTREAM + self.path, data=body, method=method)
        for h in ("Content-Type", "Authorization", "Accept"):
            v = self.headers.get(h)
            if v:
                req.add_header(h, v)
        try:
            resp = urllib.request.urlopen(req, timeout=900)
            status, payload, ctype = resp.status, resp.read(), resp.headers.get("Content-Type")
        except urllib.error.HTTPError as e:
            status, payload, ctype = e.code, e.read(), e.headers.get("Content-Type")
        except Exception as e:  # upstream unreachable
            status, payload, ctype = 502, json.dumps({"error": str(e)}).encode(), "application/json"

        self.send_response(status)
        if ctype:
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except BrokenPipeError:
            pass

        prompt_counts = PROMPT_RE.findall(payload)
        output_counts = OUTPUT_RE.findall(payload)
        if prompt_counts or output_counts:
            # one LLM call per request: the final chunk carries the totals
            entry = {
                "path": self.path,
                "prompt": int(prompt_counts[-1]) if prompt_counts else 0,
                "output": int(output_counts[-1]) if output_counts else 0,
            }
            with open(LOGFILE, "a") as f:
                f.write(json.dumps(entry) + "\n")

    def do_POST(self):
        self._forward("POST")

    def do_GET(self):
        self._forward("GET")


if __name__ == "__main__":
    print(f"proxy :{PORT} -> {UPSTREAM}, log {LOGFILE}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
