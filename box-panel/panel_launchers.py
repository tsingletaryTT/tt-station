"""Pure, importable builders for the tt-station panel's Connect launchers.

No GTK, no side effects — everything here is unit-tested with stdlib unittest
(the panel file itself, tt-station-panel.py, is hyphenated and not importable,
so testable logic lives here and the panel imports it). Ports the macOS
launcher recipes (OpenWebUILauncher / OpenCodeLauncher), simplified because the
panel runs ON the box: launches are local (docker/terminal/xdg-open), never SSH.
"""

import json
import shutil
from urllib.parse import urlparse

# ── Open WebUI (docker) constants — match the macOS OpenWebUILauncher ──
OPENWEBUI_IMAGE = "ghcr.io/open-webui/open-webui:main"
OPENWEBUI_CONTAINER = "ttstation-openwebui"
OPENWEBUI_INTERNAL_PORT = 8080  # the container's own port


def endpoint_from_snapshot(snap):
    """Return (base_url, model) for the box's serving endpoint, or None.

    Prefers the agent-source serving entry (the one the agent launched), else
    the first entry (an external run.py the operator may still want to connect
    to). Returns None when nothing is serving, the snapshot isn't a dict, or the
    chosen entry lacks base_url/model. Never raises.
    """
    if not isinstance(snap, dict):
        return None
    serving = snap.get("serving") or []
    if not isinstance(serving, list) or not serving:
        return None
    agent = next((e for e in serving
                  if isinstance(e, dict) and e.get("source") == "agent"), None)
    entry = agent or (serving[0] if isinstance(serving[0], dict) else None)
    if not entry:
        return None
    base_url = entry.get("base_url")
    model = entry.get("model")
    if not base_url or not model:
        return None
    return (base_url, model)


def serving_port_from_base_url(base_url, fallback):
    """Parse the port out of a http://host:port/v1 base URL, else `fallback`."""
    try:
        port = urlparse(base_url).port
    except (ValueError, AttributeError):
        port = None
    return port if port else fallback


def build_opencode_config(base_url, model):
    """Return the opencode.json text registering the `ttstation` provider.

    opencode splits the selection id (`ttstation/<model>`) on the FIRST `/`, so
    a vendored id like `meta-llama/Llama-3.3-70B` still resolves under the
    `ttstation` provider — we register the full model id (slashes intact) and do
    NOT re-split it (mirrors the macOS OpenCodeLauncher).
    """
    cfg = {
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "ttstation": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "TT Station",
                "options": {"baseURL": base_url},
                "models": {model: {"name": f"{model} (TT)"}},
            },
        },
        "model": f"ttstation/{model}",
    }
    return json.dumps(cfg, indent=2, sort_keys=True)
