# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""lease_run.py: a bring-up under a whole-box gozer lease, waiting while the
chips are busy. `gozer` is a shell stub first on PATH (never the real one);
the 20-second wait between asks is patched out.

    cd crates/tt-station-agentd/src && python3 -m unittest test_lease_run -v
"""

from __future__ import annotations

import io
import json
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lease_run  # noqa: E402

# Logs each call; `run` queues (exit 10, printing a ticket) while the `queued`
# file counts down, then runs the command after `--`.
FAKE_GOZER = """#!/bin/sh
D=$(dirname "$0")
echo "$*" >> "$D/calls"
case "$1" in
  run)
    n=$(cat "$D/queued" 2>/dev/null || echo 0)
    if [ "$n" -gt 0 ]; then
      echo $((n - 1)) > "$D/queued"
      echo "queued: ticket T$n" >&2
      echo "gozer: chips busy" >&2
      exit 10
    fi
    while [ "$1" != "--" ]; do shift; done
    shift
    exec "$@" ;;
  status) cat "$D/status" 2>/dev/null ;;
esac
exit 0
"""


class LeaseRun(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="tt-lease-run-")
        gozer = os.path.join(self.dir, "gozer")
        with open(gozer, "w") as f:
            f.write(FAKE_GOZER)
        os.chmod(gozer, 0o755)
        self.status = os.path.join(self.dir, "lease-status.json")
        patcher = mock.patch.dict(os.environ, {"PATH": self.dir + os.pathsep + os.environ.get("PATH", "")})
        patcher.start()
        self.addCleanup(patcher.stop)

    def calls(self):
        try:
            with open(os.path.join(self.dir, "calls")) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def run_main(self, *cmd):
        argv = ["lease_run.py", self.status, "tt-station:bringup:qwen", "port Qwen3", "--", *cmd]
        with mock.patch.object(sys, "argv", argv):
            return lease_run.main()

    def test_a_granted_lease_runs_the_command_and_returns_its_code(self):
        with open(self.status, "w") as f:
            f.write("{}")  # left over from an earlier wait
        self.assertEqual(self.run_main("sh", "-c", "exit 7"), 7)
        self.assertEqual(
            self.calls(),
            ["run --chips 4 --who tt-station:bringup:qwen --reason port Qwen3 -- sh -c exit 7"],
        )
        self.assertFalse(os.path.exists(self.status), "the waiting note is cleared")

    def test_busy_chips_are_asked_for_again_with_the_ticket_dropped_and_the_wait_said(self):
        with open(os.path.join(self.dir, "queued"), "w") as f:
            f.write("2")
        with open(os.path.join(self.dir, "status"), "w") as f:
            f.write("BOARD  STATE         WHO\n0100  HELD-FOREIGN  claude:ttm-optimize\n0101  HELD  bob:serve\n0102  FREE\n")
        seen = []

        def no_wait(secs):
            self.assertEqual(secs, 20)
            with open(self.status) as f:
                seen.append(json.load(f))

        err = io.StringIO()
        with mock.patch.object(lease_run.time, "sleep", no_wait), mock.patch.object(sys, "stderr", err):
            self.assertEqual(self.run_main("true"), 0)
        self.assertEqual(len(seen), 2)
        for s in seen:
            self.assertTrue(s["waiting_for_chips"])
            self.assertEqual(s["held_by"], "bob:serve, claude:ttm-optimize")
            self.assertIsInstance(s["since"], float)
        self.assertEqual(seen[0]["since"], seen[1]["since"], "since the first ask")
        cancels = [c for c in self.calls() if c.startswith("cancel")]
        self.assertEqual(cancels, ["cancel T2", "cancel T1"])
        self.assertEqual(sum(c.startswith("run ") for c in self.calls()), 3)
        out = err.getvalue()
        self.assertNotIn("queued: ticket", out, "the ticket line is consumed")
        self.assertIn("gozer: chips busy", out)
        self.assertIn("the chips are busy (bob:serve, claude:ttm-optimize)", out)
        self.assertFalse(os.path.exists(self.status))

    def test_holders_is_empty_without_gozer_and_short_with_many(self):
        with mock.patch.dict(os.environ, {"PATH": tempfile.mkdtemp()}):
            self.assertEqual(lease_run.holders(), "")
        with open(os.path.join(self.dir, "status"), "w") as f:
            f.write("".join(f"b{i} HELD tenant-{i:03d}-{'x' * 20}\n" for i in range(30)))
        self.assertEqual(len(lease_run.holders()), 200)


if __name__ == "__main__":
    unittest.main()
