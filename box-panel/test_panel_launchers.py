import json
import unittest

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


if __name__ == "__main__":
    unittest.main()
