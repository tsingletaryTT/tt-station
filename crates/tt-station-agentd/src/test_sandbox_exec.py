# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""sandbox_exec.py: which rules it plans, and (on a kernel with Landlock) that
a command inside it can't read a hidden folder or write outside its paths."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sandbox_exec as sx  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


class Plan(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        r = self.root = os.path.realpath(self.t.name)
        for d in ["home/.config/tt-station/bringup/run1", "home/.config/other", "home/.ssh", "home/code", "opt"]:
            os.makedirs(os.path.join(r, d))
        for f in ["home/.bashrc", "home/.config/tt-station/local-secret", "home/.ssh/id_ed25519"]:
            open(os.path.join(r, f), "w").close()
        os.symlink(os.path.join(r, "home/.config/tt-station"), os.path.join(r, "home/link-to-station"))

    def tearDown(self):
        self.t.cleanup()

    def p(self, rel):
        return os.path.join(self.root, rel)

    def test_everything_is_readable_but_the_hidden_paths(self):
        rules = dict(sx.read_rules([self.p("home/.config/tt-station"), self.p("home/.ssh")], self.root))
        self.assertEqual(rules[self.p("opt")], "read")
        self.assertEqual(rules[self.p("home/code")], "read")
        self.assertEqual(rules[self.p("home/.bashrc")], "read")
        self.assertEqual(rules[self.p("home/.config/other")], "read")
        # On the way to a hidden path: names only.
        for d in ["", "home", "home/.config"]:
            self.assertEqual(rules[self.p(d).rstrip("/")], "list", d)
        for hidden in ["home/.config/tt-station", "home/.ssh", "home/.config/tt-station/local-secret"]:
            self.assertNotIn(self.p(hidden), rules)
        # A symlink gets no rule of its own (what it points at decides).
        self.assertNotIn(self.p("home/link-to-station"), rules)

    def test_nothing_hidden_reads_everything(self):
        self.assertEqual(sx.read_rules([], self.root), [(self.root, "read")])

    def test_writable_paths_and_driver_files(self):
        rules = sx.plan(
            [self.p("home/.config/tt-station")],
            [self.p("home/.config/tt-station/bringup/run1"), self.p("missing")],
            [self.p("home/.config/tt-station/local-secret")],
        )
        self.assertIn((self.p("home/.config/tt-station/bringup/run1"), "rw"), rules)
        self.assertNotIn((self.p("missing"), "rw"), rules, "paths that don't exist are skipped")
        self.assertIn((self.p("home/.config/tt-station/local-secret"), "read"), rules)

    def test_rights_track_the_kernels_landlock(self):
        self.assertFalse(sx.handled_for(1) & sx.REFER)
        self.assertTrue(sx.handled_for(2) & sx.REFER)
        self.assertTrue(sx.handled_for(5) & sx.IOCTL_DEV)

    def test_usage(self):
        with self.assertRaises(SystemExit):
            sx.parse(["--deny", "/x"])
        lists, require, cmd = sx.parse(["--deny", "/a", "--rw", "/b", "--require", "--", "true"])
        self.assertEqual((lists["--deny"], lists["--rw"], require, cmd), (["/a"], ["/b"], True, ["true"]))


@unittest.skipUnless(sx.abi_version() >= 1, "no Landlock on this kernel")
class Enforced(unittest.TestCase):
    def test_a_confined_command_cant_read_hidden_or_write_elsewhere(self):
        with tempfile.TemporaryDirectory() as t:
            t = os.path.realpath(t)
            secret, work, other = (os.path.join(t, n) for n in ("secret", "work", "other"))
            for d in (secret, work, other):
                os.makedirs(d)
            with open(os.path.join(secret, "key"), "w") as f:
                f.write("hunter2")
            with open(os.path.join(secret, "driver.py"), "w") as f:
                f.write("print('driver')")
            script = (
                f"cat {secret}/key && echo LEAKED; "
                f"cat {secret}/driver.py; "
                f"touch {work}/ok && echo wrote-work; "
                f"touch {other}/no && echo LEAKED-WRITE; "
                f"ln {secret}/key {work}/hl && echo LEAKED-LINK; "
                f"kill -0 {os.getpid()} && echo LEAKED-SIGNAL; "
                "true"
            )
            out = subprocess.run(
                [sys.executable, os.path.join(HERE, "sandbox_exec.py"), "--require",
                 "--deny", secret, "--rw", work, "--rw", "/dev", "--ro-file", f"{secret}/driver.py",
                 "--", "sh", "-c", script],
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(out.returncode, 0, out.stderr)
            self.assertNotIn("LEAKED", out.stdout)
            self.assertIn("driver", out.stdout)
            self.assertIn("wrote-work", out.stdout)
            self.assertIn("Landlock ABI", out.stderr)


if __name__ == "__main__":
    unittest.main()
