# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""code_session.py: a coding session's terminal on a Unix socket (a tiny
tmux). Run for real with `sh` in its pseudo-terminal: keys in, output out,
resizing, a late viewer's catch-up, the exit code beside the log, and a
command that doesn't exist.

    cd crates/tt-station-agentd/src && python3 -m unittest test_code_session -v
"""

from __future__ import annotations

import base64
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest

SESSION = os.path.join(os.path.dirname(os.path.abspath(__file__)), "code_session.py")


def wait_for(cond, what, secs=10.0):
    end = time.time() + secs
    while time.time() < end:
        if cond():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {what}")


def keys(text: str) -> bytes:
    return b"i " + base64.b64encode(text.encode()) + b"\n"


class Viewer:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(0.2)
        self.sock.connect(path)
        self.seen = b""
        self.closed = False

    def send(self, data: bytes):
        self.sock.sendall(data)

    def pump(self):
        try:
            data = self.sock.recv(65536)
            if not data:
                self.closed = True
            self.seen += data
        except socket.timeout:
            pass

    def until(self, needle: bytes, secs=10.0):
        end = time.time() + secs
        while needle not in self.seen and not self.closed and time.time() < end:
            self.pump()
        if needle not in self.seen:
            raise AssertionError(f"never saw {needle!r}; got {self.seen!r}")

    def until_closed(self, secs=10.0):
        end = time.time() + secs
        while not self.closed and time.time() < end:
            self.pump()
        assert self.closed, self.seen


class Session(unittest.TestCase):
    def start(self, *cmd, cols=80, rows=24, wait=True):
        self.dir = tempfile.mkdtemp(prefix="tt-sess-")
        self.sock = os.path.join(self.dir, "s.sock")
        self.log = os.path.join(self.dir, "term.log")
        self.proc = subprocess.Popen(
            [sys.executable, SESSION, self.sock, self.log, str(cols), str(rows), "--", *cmd]
        )
        self.addCleanup(lambda: self.proc.poll() is None and (self.proc.kill(), self.proc.wait()))
        if wait:
            wait_for(lambda: os.path.exists(self.sock), "the socket")

    def viewer(self):
        v = Viewer(self.sock)
        self.addCleanup(v.sock.close)
        return v

    def exit_code(self):
        wait_for(lambda: os.path.exists(self.log + ".exit"), "the exit file")
        with open(self.log + ".exit") as f:
            return f.read()

    def test_keys_go_in_output_comes_out_and_the_exit_code_is_kept(self):
        self.start("sh", "-c", 'read x; echo "you said $x"; exit 4')
        self.assertEqual(os.stat(self.sock).st_mode & 0o777, 0o600)
        v = self.viewer()
        v.send(keys("hello\n"))
        v.until(b"you said hello")
        v.until(b"[session ended, exit 4]")
        v.until_closed()
        self.assertEqual(self.exit_code(), "4")
        self.assertEqual(self.proc.wait(timeout=10), 0)
        self.assertFalse(os.path.exists(self.sock))
        with open(self.log, "rb") as f:
            self.assertIn(b"you said hello", f.read())

    def test_a_resize_reaches_the_terminal(self):
        self.start("sh", "-c", "read x; stty size; read y", cols=80, rows=24)
        v = self.viewer()
        # Split across sends: a line is only acted on once it's whole.
        v.send(b"r 132 ")
        v.send(b"41\n" + keys("\n"))
        v.until(b"41 132")
        v.send(keys("\n"))
        v.until_closed()
        self.assertEqual(self.exit_code(), "0")

    def test_a_late_viewer_gets_the_recent_output_first(self):
        self.start("sh", "-c", "echo early-bird; read x")
        wait_for(lambda: os.path.exists(self.log) and b"early-bird" in open(self.log, "rb").read(), "output")
        v = self.viewer()
        v.until(b"early-bird", secs=2)
        # Lines it doesn't understand are ignored; the session carries on.
        v.send(b"zzz\nr wide tall\ni !!!not-base64\n")
        other = self.viewer()
        other.until(b"early-bird", secs=2)
        other.send(keys("\n"))
        v.until_closed()
        other.until_closed()
        self.assertEqual(self.exit_code(), "0")

    def test_a_command_that_does_not_exist_says_so(self):
        # It may be gone before its socket can be seen.
        self.start("/nonexistent/tt-station-agent-cli", wait=False)
        self.assertEqual(self.exit_code(), "127")
        with open(self.log, "rb") as f:
            self.assertIn(b"couldn't start /nonexistent/tt-station-agent-cli", f.read())


if __name__ == "__main__":
    unittest.main()
