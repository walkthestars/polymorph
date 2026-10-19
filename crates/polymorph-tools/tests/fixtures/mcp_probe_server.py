"""Minimal MCP probe server (TEST FIXTURE ONLY, chunk 56).

Speaks enough MCP over newline-delimited stdio JSON-RPC for rmcp's client:
`initialize` (echoes the client's protocolVersion back — negotiation-safe),
`ping`, `tools/list` (probe_echo + probe_fail), `tools/call`, plus
JSON-RPC errors for unknown tools/methods. Stdlib only, no deps.

Not product code: the repo stays single-language Rust; this file exists so
tests can spawn a REAL child through `TokioChildProcess` instead of only
loopback duplexes.
"""

import json
import sys


TOOLS = [
    {
        "name": "probe_echo",
        "description": "probe echo tool",
        "inputSchema": {"type": "object"},
    },
    {
        "name": "probe_fail",
        "description": "probe failing tool",
        "inputSchema": {"type": "object"},
    },
]


def reply(req_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result}) + "\n")
    sys.stdout.flush()


def failure(req_id, code, message):
    sys.stdout.write(
        json.dumps(
            {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}
        )
        + "\n"
    )
    sys.stdout.flush()


def handle(msg):
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params", {}) or {}
    if method == "initialize":
        reply(
            req_id,
            {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "polymorph-probe", "version": "0.1.0"},
            },
        )
    elif method == "ping":
        reply(req_id, {})
    elif method == "tools/list":
        reply(req_id, {"tools": TOOLS})
    elif method == "tools/call":
        name = params.get("name", "")
        if name == "probe_echo":
            reply(
                req_id,
                {"content": [{"type": "text", "text": "echo"}], "isError": False},
            )
        elif name == "probe_fail":
            reply(
                req_id,
                {"content": [{"type": "text", "text": "boom"}], "isError": True},
            )
        else:
            failure(req_id, -32602, "unknown tool: %s" % name)
    elif method is None:
        pass  # response to our own request — never sent; ignore defensively
    elif method.startswith("notifications/"):
        pass  # no reply
    else:
        if req_id is not None:
            failure(req_id, -32601, "method not found: %s" % method)


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            handle(json.loads(line))
        except Exception as exc:  # never die on a bad frame; tests assert behavior
            sys.stderr.write("probe: bad frame: %s\n" % exc)
            sys.stderr.flush()


if __name__ == "__main__":
    main()
