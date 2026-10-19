"""Minimal LSP probe server (TEST FIXTURE ONLY, chunks 92-93).

Speaks enough LSP over Content-Length-framed stdio JSON-RPC for the
chunk-92/93 client: `initialize` (capabilities + serverInfo), the L1b
op set (hover / definition / references / diagnostic-pull / rename /
codeAction / documentSymbol, all canned with request-echo where it
proves the round-trip), `shutdown`, plus JSON-RPC `-32601` for unknown
methods. `initialized`/`exit` notifications are absorbed silently; after
each rename reply the server also pushes `textDocument/publishDiagnostics`
and `workspace/willRenameFiles` notifications (the client's observe-only
incoming queue is proven against these). Stdlib only, no deps.

Not product code: the repo stays single-language Rust; this file exists so
tests can spawn a REAL child through the production spawn path instead of
only loopback duplexes (the chunk-56 MCP precedent, same shape).
"""

import json
import sys


def read_frame():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline().decode("utf-8")
        if not line:
            return None
        line = line.strip()
        if line == "":
            break
        if ":" in line:
            name, value = line.split(":", 1)
            headers[name.strip().lower()] = value.strip()
    try:
        length = int(headers.get("content-length", ""))
    except ValueError:
        return None
    body = sys.stdin.buffer.read(length)
    if len(body) < length:
        return None
    return json.loads(body.decode("utf-8"))


def write_frame(msg):
    body = json.dumps(msg).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


_ZERO = {
    "start": {"line": 0, "character": 0},
    "end": {"line": 0, "character": 0},
}


def notify(method, params):
    write_frame({"jsonrpc": "2.0", "method": method, "params": params})


def reply(req_id, result):
    write_frame({"jsonrpc": "2.0", "id": req_id, "result": result})


def failure(req_id, code, message):
    write_frame(
        {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": code, "message": message},
        }
    )


def main():
    while True:
        msg = read_frame()
        if msg is None:
            break
        method = msg.get("method")
        req_id = msg.get("id")
        if method == "initialize":
            reply(
                req_id,
                {
                    "capabilities": {"hoverProvider": True},
                    "serverInfo": {"name": "polymorph-probe-lsp", "version": "0.1.0"},
                },
            )
        elif method == "textDocument/hover":
            reply(
                req_id,
                {"contents": {"kind": "markdown", "value": "probe-hover"}},
            )
        elif method == "textDocument/definition":
            uri = (msg.get("params") or {}).get("textDocument", {}).get("uri", "")
            reply(req_id, [{"uri": uri, "range": _ZERO}])
        elif method == "textDocument/references":
            uri = (msg.get("params") or {}).get("textDocument", {}).get("uri", "")
            reply(
                req_id,
                [
                    {"uri": uri, "range": _ZERO},
                    {"uri": uri, "range": _ZERO},
                ],
            )
        elif method == "textDocument/diagnostic":
            reply(
                req_id,
                {
                    "kind": "full",
                    "items": [
                        {
                            "message": "probe-diagnostic",
                            "severity": 2,
                            "range": _ZERO,
                        }
                    ],
                },
            )
        elif method == "textDocument/rename":
            params = msg.get("params") or {}
            uri = params.get("textDocument", {}).get("uri", "")
            new_name = params.get("newName", "")
            reply(
                req_id,
                {
                    "changes": {
                        uri: [{"range": _ZERO, "newText": new_name}],
                    }
                },
            )
            # Unsolicited pushes for the observe-only incoming queue.
            notify(
                "textDocument/publishDiagnostics",
                {"uri": uri, "diagnostics": [{"message": "probe-diagnostic"}]},
            )
            notify(
                "workspace/willRenameFiles",
                {"files": [{"oldUri": uri, "newUri": uri + ".renamed"}]},
            )
        elif method == "textDocument/codeAction":
            reply(req_id, [{"title": "probe-action", "kind": "quickfix"}])
        elif method == "textDocument/documentSymbol":
            reply(
                req_id,
                [
                    {
                        "name": "probe-symbol",
                        "kind": 12,
                        "range": _ZERO,
                        "selectionRange": _ZERO,
                    }
                ],
            )
        elif method == "shutdown":
            reply(req_id, None)
        elif method in ("initialized", "exit"):
            if method == "exit":
                break
        elif req_id is not None:
            failure(req_id, -32601, "Method not found: %s" % method)


if __name__ == "__main__":
    main()
