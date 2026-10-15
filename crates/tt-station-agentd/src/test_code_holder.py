# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""code_holder.py: holds a Code session's process so it outlives the agent.
Run for real against a small Python child standing in for Claude Code: the
journal, ATTACH replay, a reconnecting client, input forwarding, and #EXIT.

    cd crates/tt-station-agentd/src && python3 -m unittest test_code_holder -v
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest

HOLDER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "code_holder.py")

CHILD = """
import os, sys
print("one", os.environ.get("TT_HOLDER_TEST"), os.getcwd(), flush=True)
print("two", flush=True)
for line in sys.stdin:
    line = line.strip()
    if line == "quit":
        sys.exit(3)
    print("got " + line, flush=True)
"""


def wait_for(cond, what, secs=10.0):
    end = time.time() + secs
    while time.time() < end:
        if cond():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {what}")


class Holder(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="tt-holder-")
        self.cwd = os.path.join(self.dir, "work")
        os.mkdir(self.cwd)
        self.sock = os.path.join(self.dir, "s.sock")
        self.journal = os.path.join(self.dir, "journal")
        self.spec = os.path.join(self.dir, "spec.json")
        with open(self.spec, "w") as f:
            json.dump({
                "argv": [sys.executable, "-u", "-c", CHILD],
                "env": {"TT_HOLDER_TEST": "from-spec"},
                "cwd": self.cwd,
                "socket": self.sock,
                "journal": self.journal,
                "stderr": os.path.join(self.dir, "stderr"),
            }, f)
        self.proc = subprocess.Popen([sys.executable, HOLDER, self.spec])
        self.addCleanup(self.stop)
        wait_for(lambda: os.path.exists(self.sock), "the socket")
        wait_for(lambda: len(self.journal_lines()) >= 2, "the child's first lines")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()

    def journal_lines(self):
        try:
            with open(self.journal, "rb") as f:
                return f.read().decode().splitlines()
        except FileNotFoundError:
            return []

    def attach(self, first: bytes):
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(10)
        c.connect(self.sock)
        c.sendall(first)
        return c, c.makefile("rb")

    def test_the_spec_is_deleted_and_the_socket_is_private(self):
        self.assertFalse(os.path.exists(self.spec), "it can hold the session's key")
        self.assertEqual(os.stat(self.sock).st_mode & 0o777, 0o600)
        self.assertEqual(self.journal_lines()[0], f"one from-spec {os.path.realpath(self.cwd)}")

    def test_a_client_gets_the_journal_then_live_lines_and_its_input_reaches_the_child(self):
        c, r = self.attach(b"ATTACH 0\n")
        self.assertTrue(r.readline().startswith(b"one from-spec"))
        self.assertEqual(r.readline(), b"two\n")
        c.sendall(b"hello\n")
        self.assertEqual(r.readline(), b"got hello\n")
        c.sendall(b"quit\n")
        self.assertEqual(r.readline(), b"#EXIT 3\n")
        self.assertEqual(r.readline(), b"", "closed after the exit line")
        self.assertEqual(self.proc.wait(timeout=10), 0)
        self.assertEqual(self.journal_lines()[-2:], ["got hello", "#EXIT 3"])
        self.assertFalse(os.path.exists(self.sock))

    def test_attach_skips_the_lines_a_client_already_has(self):
        c, r = self.attach(b"ATTACH 1\n")
        self.assertEqual(r.readline(), b"two\n")
        c.sendall(b"x\n")
        self.assertEqual(r.readline(), b"got x\n")
        c.close()
        # Anything else as a first line replays everything.
        c, r = self.attach(b"HELLO\n")
        self.assertTrue(r.readline().startswith(b"one"))
        c.close()

    def test_a_new_client_takes_over_and_the_old_one_is_closed(self):
        old, old_r = self.attach(b"ATTACH 2\n")
        old.sendall(b"first\n")
        self.assertEqual(old_r.readline(), b"got first\n")
        new, new_r = self.attach(b"ATTACH 2\n")
        self.assertEqual(new_r.readline(), b"got first\n", "replayed from its line")
        self.assertEqual(old_r.readline(), b"", "the old client was closed")
        new.sendall(b"second\n")
        self.assertEqual(new_r.readline(), b"got second\n")
        # The child's output kept going to the journal throughout.
        wait_for(lambda: "got second" in self.journal_lines(), "the journal")


if __name__ == "__main__":
    unittest.main()
