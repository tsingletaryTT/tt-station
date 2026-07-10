import json
import unittest
from unittest import mock

import panel_launchers as pl


class EndpointFromSnapshot(unittest.TestCase):
    def test_prefers_agent_source(self):
        snap = {"serving": [
            {"source": "external", "base_url": "http://h:8001/v1", "model": "ext"},
            {"source": "agent", "base_url": "http://h:8003/v1", "model": "meta-llama/L-70B"},
        ]}
        self.assertEqual(
            pl.endpoint_from_snapshot(snap),
            ("http://h:8003/v1", "meta-llama/L-70B"))

    def test_falls_back_to_first_when_no_agent(self):
        snap = {"serving": [
            {"source": "external", "base_url": "http://h:8001/v1", "model": "ext"},
        ]}
        self.assertEqual(pl.endpoint_from_snapshot(snap),
                         ("http://h:8001/v1", "ext"))

    def test_none_when_empty(self):
        self.assertIsNone(pl.endpoint_from_snapshot({"serving": []}))
        self.assertIsNone(pl.endpoint_from_snapshot({}))
        self.assertIsNone(pl.endpoint_from_snapshot(None))

    def test_skips_entry_missing_fields(self):
        snap = {"serving": [{"source": "agent", "model": "m"}]}  # no base_url
        self.assertIsNone(pl.endpoint_from_snapshot(snap))


class ServingPort(unittest.TestCase):
    def test_parses_port(self):
        self.assertEqual(pl.serving_port_from_base_url("http://h:8003/v1", 8003), 8003)

    def test_fallback_on_garbage(self):
        self.assertEqual(pl.serving_port_from_base_url("not a url", 8003), 8003)
        self.assertEqual(pl.serving_port_from_base_url("http://h/v1", 8003), 8003)


class OpencodeConfig(unittest.TestCase):
    def test_shape(self):
        text = pl.build_opencode_config("http://h:8003/v1", "meta-llama/L-70B")
        cfg = json.loads(text)
        self.assertEqual(cfg["model"], "ttstation/meta-llama/L-70B")
        prov = cfg["provider"]["ttstation"]
        self.assertEqual(prov["npm"], "@ai-sdk/openai-compatible")
        self.assertEqual(prov["options"]["baseURL"], "http://h:8003/v1")
        self.assertIn("meta-llama/L-70B", prov["models"])


class OpenWebUICommand(unittest.TestCase):
    def test_contains_key_pieces(self):
        cmd = pl.build_openwebui_command(8003, host_port=3000)
        self.assertIn("ttstation-openwebui", cmd)
        self.assertIn("ghcr.io/open-webui/open-webui:main", cmd)
        self.assertIn("-p 3000:8080", cmd)
        self.assertIn("--add-host=host.docker.internal:host-gateway", cmd)
        self.assertIn("http://host.docker.internal:8003/v1", cmd)
        self.assertIn("WEBUI_AUTH=false", cmd)
        # idempotent reuse guard + volume
        self.assertIn("State.Running", cmd)
        self.assertIn("-f '{{.State.Running}}'", cmd)
        self.assertIn("ttstation-openwebui:/app/backend/data", cmd)

    def test_custom_host_port(self):
        cmd = pl.build_openwebui_command(8003, host_port=3100)
        self.assertIn("-p 3100:8080", cmd)


class TerminalCommand(unittest.TestCase):
    def test_quotes_dir(self):
        self.assertEqual(
            pl.opencode_terminal_command("/home/x/.local/share/tt-station/opencode/h_8003"),
            "cd '/home/x/.local/share/tt-station/opencode/h_8003' && opencode")


class ResolveTerminal(unittest.TestCase):
    def test_returns_none_or_list(self):
        result = pl.resolve_terminal_emulator()
        self.assertTrue(result is None or isinstance(result, list))

    def test_gnome_terminal_uses_double_dash(self):
        with mock.patch.object(
                pl.shutil, "which",
                side_effect=lambda n: "/usr/bin/gnome-terminal" if n == "gnome-terminal" else None):
            self.assertEqual(pl.resolve_terminal_emulator(), ["/usr/bin/gnome-terminal", "--"])

    def test_xterm_uses_dash_e(self):
        with mock.patch.object(
                pl.shutil, "which",
                side_effect=lambda n: "/usr/bin/xterm" if n == "xterm" else None):
            self.assertEqual(pl.resolve_terminal_emulator(), ["/usr/bin/xterm", "-e"])


if __name__ == "__main__":
    unittest.main()
