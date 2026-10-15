# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""TT Station's permission server for Claude Code (an MCP server on stdio).

Claude Code runs a bring-up stage with `--permission-prompt-tool
mcp__ttstation__approve`; every tool use that would need permission comes
here. Routine work is allowed on the spot (policy `smart`); risky work waits
for a person in TT Station (see codex_bringup.risk / ask_person, shared).

Environment: TT_STATION_URL, TT_STATION_RUN, TT_STATION_RUN_SECRET,
TT_STATION_POLICY (ask | smart), TT_STATION_REPO.
"""

from __future__ import annotations

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from codex_bringup import ask_person, risk  # noqa: E402

READ_ONLY = {"Read", "Glob", "Grep", "LS", "WebSearch", "WebFetch", "TodoWrite", "Task", "NotebookRead", "Skill", "BashOutput"}
EDITS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}


def decide(tool: str, args: dict) -> dict:
    policy = os.environ.get("TT_STATION_POLICY", "smart")
    repo = os.path.realpath(os.environ.get("TT_STATION_REPO", os.getcwd()))
    why: str | None
    if tool in READ_ONLY:
        why = None
    elif tool == "Bash":
        why = risk(args.get("command"), args.get("cwd") or repo, repo)
    elif tool in EDITS:
        path = os.path.realpath(args.get("file_path") or args.get("notebook_path") or repo)
        why = None if path.startswith(repo) else f"writes outside the repo ({path})"
    else:
        why = None
    if policy == "ask" and tool not in READ_ONLY:
        why = why or "you asked to approve everything"
    if why is None:
        return {"behavior": "allow", "updatedInput": args}
    kind = "command" if tool == "Bash" else "file change" if tool in EDITS else tool
    command = args.get("command") or args.get("file_path") or json.dumps(args)[:400]
    decision = ask_person(kind, {"command": command, "cwd": args.get("cwd"), "reason": f"Claude's {tool} tool"}, why)
    if decision.startswith("accept"):
        return {"behavior": "allow", "updatedInput": args}
    return {"behavior": "deny", "message": "A person declined this in TT Station. Find another way, or stop and explain why you need it."}


def reply(id_, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id_}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def main() -> None:
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method, id_ = msg.get("method"), msg.get("id")
        if id_ is None:
            continue  # notifications
        if method == "initialize":
            reply(id_, {
                "protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ttstation", "version": "1"},
            })
        elif method == "tools/list":
            reply(id_, {"tools": [{
                "name": "approve",
                "description": "Ask TT Station whether a tool use may go ahead.",
                "inputSchema": {"type": "object", "properties": {
                    "tool_name": {"type": "string"}, "input": {"type": "object"}, "tool_use_id": {"type": "string"},
                }, "required": ["tool_name", "input"]},
            }]})
        elif method == "tools/call":
            args = msg.get("params", {}).get("arguments", {})
            verdict = decide(args.get("tool_name", ""), args.get("input") or {})
            reply(id_, {"content": [{"type": "text", "text": json.dumps(verdict)}]})
        elif method == "ping":
            reply(id_, {})
        else:
            reply(id_, error={"code": -32601, "message": f"unknown method {method}"})


if __name__ == "__main__":
    main()
