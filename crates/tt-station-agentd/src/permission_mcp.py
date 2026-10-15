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
from codex_bringup import ask_person, inside, risk  # noqa: E402

READ_ONLY = {"Read", "Glob", "Grep", "LS", "WebSearch", "TodoWrite", "Task", "NotebookRead", "Skill", "BashOutput", "KillShell", "KillBash", "ExitPlanMode"}
EDITS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
# Where a bring-up reads documentation. Fetching any other address waits for
# a person: a URL can carry what the agent read (its query string is a way out).
DOC_HOSTS = ("huggingface.co", "github.com", "raw.githubusercontent.com", "docs.tenstorrent.com", "arxiv.org", "pytorch.org", "docs.vllm.ai")


def doc_page(url) -> bool:
    from urllib.parse import urlsplit
    try:
        u = urlsplit(str(url or ""))
    except ValueError:
        return False
    host = (u.hostname or "").lower()
    return u.scheme == "https" and not u.query and any(host == h or host.endswith("." + h) for h in DOC_HOSTS)


def decide(tool: str, args: dict) -> dict:
    policy = os.environ.get("TT_STATION_POLICY", "smart")
    repo = os.path.realpath(os.environ.get("TT_STATION_REPO", os.getcwd()))
    why: str | None
    if tool in READ_ONLY:
        why = None
    elif tool == "WebFetch":
        why = None if doc_page(args.get("url")) else f"fetches {str(args.get('url'))[:200]}"
    elif tool == "Bash":
        why = risk(args.get("command"), args.get("cwd") or repo, repo)
    elif tool in EDITS:
        path = os.path.realpath(args.get("file_path") or args.get("notebook_path") or repo)
        why = None if inside(path, repo) else f"writes outside the repo ({path})"
    else:
        # A tool this list doesn't know (an MCP server's, a new one): a person decides.
        why = f"uses {tool}, which TT Station doesn't know"
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
