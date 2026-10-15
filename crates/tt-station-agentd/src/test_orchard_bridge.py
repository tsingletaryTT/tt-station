# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""orchard_bridge.py: tt-station's bridge into a tt-orchard checkout. Run as
the agent runs it, against a stand-in `orchard` package in a temp checkout:
`info` (a broken check is a result, not a crash), `preflight` (credentials,
--base, an older tt-orchard without it, resuming, download size), errors as
JSON, and an unknown command.

    cd crates/tt-station-agentd/src && python3 -m unittest test_orchard_bridge -v
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

BRIDGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "orchard_bridge.py")

PACKAGE = {
    "__init__.py": '__version__ = "0.9.1"\n',
    "lab_setup.py": "",
    "cli.py": """
        from pathlib import Path
        def find_config(explicit, env, checkout, home):
            return Path(checkout) / "bringup.toml"
    """,
    "bringup_config.py": """
        import os
        from pathlib import Path
        from types import SimpleNamespace as NS
        def load(path):
            root = Path(os.environ["FAKE_RUNS_ROOT"])
            lab = NS(host="lab-box", root=Path("/srv/lab"), gozer=True, path=["/opt/bin"], python="py", test_python="tpy") \\
                if os.environ.get("FAKE_LAB") else None
            return NS(runs_root=root, coder=NS(target="local", port=8001, chips=2), required_chips=4,
                      package_format="tt-kernel", tt_model_root=None, lab=lab)
        def run_dir(cfg, model):
            return Path(cfg.runs_root) / model.replace("/", "--")
    """,
    "setup_machine.py": """
        import os
        from types import SimpleNamespace as NS
        def default_env(checkout):
            return {}
        class Options:
            pass
        def plan(env, opts):
            if os.environ.get("FAKE_SETUP_BREAKS"):
                raise RuntimeError("uv not found")
            return [NS(name="python", status="ok", detail="3.12"), NS(name="hf token", status="todo", detail="missing")]
    """,
    "preflight.py": """
        import json, os
        from types import SimpleNamespace as NS
        def hf_home_for(cfg):
            return "/data/hf"
        class Signals:
            def local_snapshot(self, model):
                return "/snap" if os.environ.get("FAKE_LOCAL") else None
        def default_signals(cfg):
            return Signals()
        def _record(**kw):
            with open(os.environ["FAKE_CALLS"], "a") as f:
                f.write(json.dumps(kw) + "\\n")
        def _checks():
            hub = NS(total_bytes=16_400_000_000, license="apache-2.0")
            return [NS(name="hub", status="ok", detail="found", reason=None, data=hub),
                    NS(name="base", status="warn", detail="2 bundles", reason="pick one",
                       data={"candidates": ["acme/a", "acme/b"]}),
                    NS(name="chips", status="fail", detail="busy", reason="held", data=None)]
        if os.environ.get("FAKE_OLD_ORCHARD"):
            def run_preflight(cfg, model, accept_credentials, signals, resuming):
                _record(model=model, accept=accept_credentials, resuming=resuming, base="(unsupported)")
                return _checks()
        else:
            def run_preflight(cfg, model, accept_credentials, signals, resuming, base_override=None):
                _record(model=model, accept=accept_credentials, resuming=resuming, base=base_override)
                return _checks()
        def blocked(checks):
            return [c for c in checks if c.status == "fail"]
    """,
}


class Bridge(unittest.TestCase):
    def setUp(self):
        self.checkout = tempfile.mkdtemp(prefix="tt-orchard-")
        pkg = os.path.join(self.checkout, "orchard")
        os.mkdir(pkg)
        for name, text in PACKAGE.items():
            with open(os.path.join(pkg, name), "w") as f:
                f.write(textwrap.dedent(text))
        self.runs = os.path.join(self.checkout, "runs")
        self.calls = os.path.join(self.checkout, "calls.jsonl")
        self.env = {**os.environ, "FAKE_RUNS_ROOT": self.runs, "FAKE_CALLS": self.calls}

    def bridge(self, *args, **env):
        # As the agent runs it: `python3 -c <the file's text> CHECKOUT ...`.
        with open(BRIDGE) as f:
            code = f.read()
        out = subprocess.run(
            [sys.executable, "-c", code, self.checkout, *args],
            capture_output=True, text=True, timeout=30, env={**self.env, **env},
            cwd=tempfile.gettempdir(),
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        return json.loads(out.stdout)

    def recorded(self):
        with open(self.calls) as f:
            return [json.loads(line) for line in f]

    def test_info_reports_the_version_setup_checks_and_config(self):
        out = self.bridge("info")
        self.assertEqual(out["version"], "0.9.1")
        self.assertEqual(out["setup"][1], {"name": "hf token", "status": "todo", "detail": "missing"})
        cfg = out["config"]
        self.assertEqual(cfg["path"], os.path.join(self.checkout, "bringup.toml"))
        self.assertEqual(cfg["runs_root"], self.runs)
        self.assertEqual(cfg["coder"], {"target": "local", "port": 8001, "chips": 2})
        self.assertEqual(cfg["hf_home"], "/data/hf")
        self.assertIsNone(cfg["lab"])
        lab = self.bridge("info", FAKE_LAB="1")["config"]["lab"]
        self.assertEqual(lab, {"host": "lab-box", "root": "/srv/lab", "gozer": True, "path": ["/opt/bin"],
                               "python": "py", "test_python": "tpy"})

    def test_a_broken_setup_check_is_a_result_not_a_crash(self):
        out = self.bridge("info", FAKE_SETUP_BREAKS="1")
        self.assertEqual(out["setup_error"], "RuntimeError: uv not found")
        self.assertNotIn("setup", out)
        self.assertIn("config", out, "the rest still reported")

    def test_preflight_accepts_credentials_unless_told_not_to_and_passes_the_base(self):
        out = self.bridge("preflight", "Qwen/Qwen3-8B")
        self.assertEqual(out["run_name"], "Qwen--Qwen3-8B")
        self.assertEqual(out["run_dir"], os.path.join(self.runs, "Qwen--Qwen3-8B"))
        self.assertFalse(out["resuming"])
        self.assertFalse(out["local"])
        self.assertEqual(out["download_gb"], 16.4)
        self.assertEqual(out["license"], "apache-2.0")
        self.assertTrue(out["blocked"])
        self.assertEqual([c["name"] for c in out["checks"]], ["hub", "base", "chips"])
        self.assertEqual(out["checks"][1]["candidates"], ["acme/a", "acme/b"])
        self.assertIsNone(out["checks"][0]["candidates"], "data that isn't a dict has no candidates")

        os.makedirs(os.path.join(self.runs, "Qwen--Qwen3-8B"))
        open(os.path.join(self.runs, "Qwen--Qwen3-8B", "ledger.jsonl"), "w").close()
        out = self.bridge("preflight", "Qwen/Qwen3-8B", "--refuse-credentials", "--base", "acme/b", FAKE_LOCAL="1")
        self.assertTrue(out["resuming"])
        self.assertEqual(out["download_gb"], 0.0, "the weights are already here")
        self.assertEqual(self.recorded(), [
            {"model": "Qwen/Qwen3-8B", "accept": True, "resuming": False, "base": None},
            {"model": "Qwen/Qwen3-8B", "accept": False, "resuming": True, "base": "acme/b"},
        ])

    def test_an_older_tt_orchard_without_base_still_preflights(self):
        out = self.bridge("preflight", "acme/m", "--base", "acme/a", FAKE_OLD_ORCHARD="1")
        self.assertNotIn("error", out)
        self.assertEqual(self.recorded(), [{"model": "acme/m", "accept": True, "resuming": False, "base": "(unsupported)"}])

    def test_failures_and_unknown_commands_come_back_as_json(self):
        self.assertEqual(self.bridge("frobnicate"), {"error": "unknown command frobnicate"})
        out = self.bridge("lab-check")
        self.assertEqual(out, {"error": "bringup.toml has no [lab] table"})
        # A checkout with no orchard package at all.
        empty = tempfile.mkdtemp()
        with open(BRIDGE) as f:
            code = f.read()
        res = subprocess.run([sys.executable, "-c", code, empty, "preflight", "x/y"],
                             capture_output=True, text=True, timeout=30, cwd=empty)
        self.assertTrue(json.loads(res.stdout)["error"].startswith("ModuleNotFoundError"), res.stdout)
        # --base with nothing after it.
        self.assertIn("error", self.bridge("preflight", "x/y", "--base"))


if __name__ == "__main__":
    unittest.main()
