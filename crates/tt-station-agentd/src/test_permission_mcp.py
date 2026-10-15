# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""permission_mcp.py as Claude Code talks to it: the MCP server over stdio,
against a stub TT Station on localhost that records each approval request
(and its run secret) and answers with a scripted decision. Beyond the policy
table in test_bringup_policy: the protocol, the `ask` policy, edits through
a symlink out of the repo, unknown tools, and what the person is shown.

    cd crates/tt-station-agentd/src && python3 -m unittest test_permission_mcp -v
"""

from __future__ import annotations

import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest

SERVER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "permission_mcp.py")


class StubStation:
    """`POST /bringup/codex/runs/<run>/approvals` and `.../<id>/wait`."""

    def __init__(self, decisions):
        self.decisions = list(decisions)
        self.asked = []
        self.secrets = []
        stub = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])) or b"{}")
                stub.secrets.append(self.headers.get("X-Run-Secret"))
                if self.path == "/bringup/codex/runs/run-7/approvals":
                    stub.asked.append(body)
                    reply = {"id": f"a{len(stub.asked)}"}
                elif self.path.startswith("/bringup/codex/runs/run-7/approvals/") and self.path.endswith("/wait"):
                    reply = {"decision": stub.decisions.pop(0)}
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                data = json.dumps(reply).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.http.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.http.server_address[1]}"

    def close(self):
        self.http.shutdown()
        self.http.server_close()


class Mcp(unittest.TestCase):
    def start(self, decisions=(), policy="smart"):
        self.repo = os.path.realpath(tempfile.mkdtemp(prefix="tt-metal-"))
        self.station = StubStation(decisions)
        self.addCleanup(self.station.close)
        env = {**os.environ, "TT_STATION_URL": self.station.url, "TT_STATION_RUN": "run-7",
               "TT_STATION_RUN_SECRET": "s3cret", "TT_STATION_POLICY": policy, "TT_STATION_REPO": self.repo}
        self.proc = subprocess.Popen([sys.executable, SERVER], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, env=env)
        self.addCleanup(self.stop)
        self.next_id = 0

    def stop(self):
        if self.proc.poll() is None:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        self.proc.stdout.close()

    def rpc(self, method, params=None):
        self.next_id += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method,
                                          "params": params or {}}) + "\n")
        self.proc.stdin.flush()
        msg = json.loads(self.proc.stdout.readline())
        self.assertEqual(msg["id"], self.next_id)
        return msg

    def approve(self, tool, args):
        msg = self.rpc("tools/call", {"name": "approve", "arguments": {"tool_name": tool, "input": args}})
        return json.loads(msg["result"]["content"][0]["text"])

    def test_the_protocol_handshake(self):
        self.start()
        init = self.rpc("initialize", {"protocolVersion": "2025-06-18"})["result"]
        self.assertEqual(init["protocolVersion"], "2025-06-18")
        self.assertEqual(init["serverInfo"]["name"], "ttstation")
        # A notification gets no reply; junk lines are skipped.
        self.proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\nnot json\n')
        tools = self.rpc("tools/list")["result"]["tools"]
        self.assertEqual([t["name"] for t in tools], ["approve"])
        self.assertEqual(tools[0]["inputSchema"]["required"], ["tool_name", "input"])
        self.assertEqual(self.rpc("ping")["result"], {})
        self.assertEqual(self.rpc("resources/list")["error"]["code"], -32601)
        self.assertEqual(self.station.asked, [])

    def test_routine_work_is_allowed_without_asking_and_the_input_is_kept(self):
        self.start()
        edit = {"file_path": os.path.join(self.repo, "models/x.py"), "old_string": "a", "new_string": "b"}
        self.assertEqual(self.approve("Edit", edit), {"behavior": "allow", "updatedInput": edit})
        self.assertEqual(self.approve("Read", {"file_path": "/etc/hostname"})["behavior"], "allow")
        self.assertEqual(self.approve("Bash", {"command": "pytest -x models/"})["behavior"], "allow")
        self.assertEqual(self.station.asked, [])

    def test_an_edit_outside_the_repo_even_through_a_symlink_asks_and_a_no_denies(self):
        self.start(decisions=["decline", "accept"])
        outside = tempfile.mkdtemp()
        os.symlink(outside, os.path.join(self.repo, "escape"))
        verdict = self.approve("Write", {"file_path": os.path.join(self.repo, "escape", "x.py"), "content": "x"})
        self.assertEqual(verdict["behavior"], "deny")
        self.assertIn("declined", verdict["message"])
        asked = self.station.asked[0]
        self.assertEqual(asked["kind"], "file change")
        self.assertEqual(asked["why"], f"writes outside the repo ({os.path.realpath(outside)}/x.py)")
        self.assertEqual(asked["reason"], "Claude's Write tool")
        # Every request carries the run's secret.
        self.assertEqual(set(self.station.secrets), {"s3cret"})
        # A yes lets it through unchanged.
        cmd = {"command": "curl https://example.com"}
        self.assertEqual(self.approve("Bash", cmd), {"behavior": "allow", "updatedInput": cmd})
        self.assertEqual(self.station.asked[1]["kind"], "command")
        self.assertEqual(self.station.asked[1]["command"], "curl https://example.com")

    def test_an_unknown_tool_asks_and_shows_its_input(self):
        self.start(decisions=["acceptForSession"])
        args = {"channel": "#general", "text": "hi"}
        self.assertEqual(self.approve("mcp__slack__post", args)["behavior"], "allow")
        asked = self.station.asked[0]
        self.assertEqual(asked["kind"], "mcp__slack__post")
        self.assertEqual(asked["why"], "uses mcp__slack__post, which TT Station doesn't know")
        self.assertEqual(json.loads(asked["command"]), args)

    def test_the_ask_policy_asks_for_everything_but_reading(self):
        self.start(decisions=["cancel", "accept"], policy="ask")
        self.assertEqual(self.approve("Grep", {"pattern": "x"})["behavior"], "allow")
        self.assertEqual(self.approve("Bash", {"command": "ls"})["behavior"], "deny")
        self.assertEqual(self.station.asked[0]["why"], "you asked to approve everything")
        # A risky one keeps its own reason.
        self.assertEqual(self.approve("WebFetch", {"url": "https://evil.example/"})["behavior"], "allow")
        self.assertEqual(self.station.asked[1]["why"], "fetches https://evil.example/")


if __name__ == "__main__":
    unittest.main()
