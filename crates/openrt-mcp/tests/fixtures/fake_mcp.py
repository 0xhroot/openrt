#!/usr/bin/env python3
"""Fake MCP server for openrt-mcp tests (stdio, newline-delimited JSON-RPC).

Behaviors exercised by the test suite:
  * standard initialize / tools/list / tools/call lifecycle
  * `echo`, `add` — normal tools with JSON schemas
  * `bad_schema`   — advertised with an unenforceable schema ("banana" type);
                     the client must REFUSE to import it.
  * `always_fails` — returns isError: true (must map to BACKEND_FAILURE)
  * `--hang`       — sleeps 10s before answering initialize (connect timeout)
  * drift marker   — if $FAKE_MCP_DRIFT_MARKER exists, tools/list also
                     advertises `drifted` (discovery test creates the file)

The server NEVER echoes environment values (they must not leak anywhere).
"""
import json
import os
import sys
import time

HANG = "--hang" in sys.argv
MARKER = os.environ.get("FAKE_MCP_DRIFT_MARKER", "")


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def tools_list():
    tools = [
        {
            "name": "echo",
            "description": "Echo a message back as text.",
            "inputSchema": {
                "type": "object",
                "required": ["message"],
                "properties": {"message": {"type": "string", "minLength": 1}},
            },
        },
        {
            "name": "add",
            "description": "Add two numbers.",
            "inputSchema": {
                "type": "object",
                "required": ["a", "b"],
                "properties": {
                    "a": {"type": "number"},
                    "b": {"type": "number"},
                },
            },
        },
        {
            "name": "bad_schema",
            "description": "Tool with an unenforceable schema (must be refused).",
            "inputSchema": {"type": "banana"},
        },
        {
            "name": "always_fails",
            "description": "Always returns isError: true.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ]
    if MARKER and os.path.exists(MARKER):
        tools.append(
            {
                "name": "drifted",
                "description": "Appears only after the drift marker exists.",
                "inputSchema": {"type": "object", "properties": {}},
            }
        )
    return tools


def main():
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        mid = msg.get("id")

        if method == "initialize":
            if HANG:
                time.sleep(10)
            send(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-mcp", "version": "1.0.0"},
                    },
                }
            )
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools_list()}})
        elif method == "tools/call":
            name = msg.get("params", {}).get("name")
            args = msg.get("params", {}).get("arguments") or {}
            if name == "echo":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "content": [
                                {"type": "text", "text": "echo: " + args.get("message", "")}
                            ]
                        },
                    }
                )
            elif name == "add":
                total = args.get("a", 0) + args.get("b", 0)
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {"content": [{"type": "text", "text": str(total)}]},
                    }
                )
            elif name == "drifted":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {"content": [{"type": "text", "text": "drifted-ok"}]},
                    }
                )
            elif name == "always_fails":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "content": [{"type": "text", "text": "boom from server"}],
                            "isError": True,
                        },
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "error": {"code": -32602, "message": f"unknown tool {name}"},
                    }
                )
        elif mid is not None:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "error": {"code": -32601, "message": f"method not found: {method}"},
                }
            )


if __name__ == "__main__":
    main()
