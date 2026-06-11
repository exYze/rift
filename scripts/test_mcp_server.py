#!/usr/bin/env python3
"""Dependency-free MCP stdio server used as a test fixture and example.

Exposes one tool, get_secret, which returns a fixed codeword — enough to
verify the full client handshake + tools/list + tools/call path.
"""
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        send({
            "jsonrpc": "2.0", "id": msg_id,
            "result": {
                "protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "testsrv", "version": "0.1"},
            },
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0", "id": msg_id,
            "result": {"tools": [{
                "name": "get_secret",
                "description": "Returns the secret codeword for verification",
                "inputSchema": {"type": "object", "properties": {}},
            }]},
        })
    elif method == "tools/call":
        name = msg.get("params", {}).get("name")
        if name == "get_secret":
            send({
                "jsonrpc": "2.0", "id": msg_id,
                "result": {"content": [{"type": "text", "text": "the codeword is XYZZY-42"}], "isError": False},
            })
        else:
            send({
                "jsonrpc": "2.0", "id": msg_id,
                "result": {"content": [{"type": "text", "text": f"unknown tool {name}"}], "isError": True},
            })
    elif msg_id is not None:
        send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "method not found"}})
