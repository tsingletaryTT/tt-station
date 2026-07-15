"""Pure, importable builders for the tt-station panel's Connect launchers.

No GTK, no side effects — everything here is unit-tested with stdlib unittest
(the panel file itself, tt-station-panel.py, is hyphenated and not importable,
so testable logic lives here and the panel imports it). Ports the macOS
launcher recipes (OpenWebUILauncher / OpenCodeLauncher), simplified because the
panel runs ON the box: launches are local (docker/terminal/xdg-open), never SSH.
"""

import json
import os
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


def build_openwebui_command(serving_port, host_port=3000):
    """Idempotent shell script to (re)launch Open WebUI on the box, wired to the
    box's LOCAL vLLM on `serving_port`. Reuse if already running; else remove any
    stale container, pull with retries (first pull can flake), and `docker run`.
    Ports the macOS OpenWebUILauncher.dockerCommand, run locally (no SSH).
    """
    c = OPENWEBUI_CONTAINER
    img = OPENWEBUI_IMAGE
    return (
        f"if [ \"$(docker inspect -f '{{{{.State.Running}}}}' {c} 2>/dev/null)\" = \"true\" ]; then exit 0; fi\n"
        f"docker rm -f {c} >/dev/null 2>&1 || true\n"
        f"if ! docker image inspect {img} >/dev/null 2>&1; then\n"
        f"  for i in 1 2 3 4 5; do docker pull {img} && break; sleep 3; done\n"
        f"fi\n"
        f"docker run -d --name {c} \\\n"
        f"  -p {host_port}:{OPENWEBUI_INTERNAL_PORT} \\\n"
        f"  --add-host=host.docker.internal:host-gateway \\\n"
        f"  -e OPENAI_API_BASE_URL=http://host.docker.internal:{serving_port}/v1 \\\n"
        f"  -e OPENAI_API_KEY=sk-none -e WEBUI_AUTH=false \\\n"
        f"  -v {c}:/app/backend/data \\\n"
        f"  {img}\n"
    )


def opencode_terminal_command(config_dir):
    """The shell line a terminal runs: cd into the config dir and start opencode.
    Single-quoted dir (our own scratch path under ~/.local/share, no quotes)."""
    return f"cd '{config_dir}' && opencode"


def resolve_terminal_emulator():
    """An argv prefix that runs a shell command in a NEW terminal window, or None.

    Tries, in order: x-terminal-emulator (Debian alternatives), gnome-terminal,
    konsole, xterm. Each entry pairs the terminal with the flag that makes it
    treat the REST of the argv as the command to run: gnome-terminal needs `--`
    (its `-e` takes a single deprecated string, not a trailing argv), while
    x-terminal-emulator/konsole/xterm all accept `-e <cmd> <args...>`. The
    caller appends `bash -lc "<command>"` so PATH resolves opencode via a login
    shell (GUI apps don't inherit the shell PATH).
    """
    candidates = (
        ("x-terminal-emulator", "-e"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xterm", "-e"),
    )
    for term, sep in candidates:
        path = shutil.which(term)
        if path:
            return [path, sep]
    return None


# Local power actions for the box's own screen. reset-chips is a board reset
# (tt-smi -r); the machine ops shell systemctl (permitted by the polkit rule
# shipped with the tt-station .deb). No Wake — meaningless on the box itself.
_POWER_COMMANDS = {
    "reset-chips": ["tt-smi", "-r"],
    "suspend": ["systemctl", "suspend"],
    "reboot": ["systemctl", "reboot"],
    "shutdown": ["systemctl", "poweroff"],
}


def power_command(action):
    """Return the argv for a local power action, or raise ValueError."""
    try:
        return list(_POWER_COMMANDS[action])
    except KeyError:
        raise ValueError(f"unknown power action: {action}")


def resolve_tool(name):
    """Absolute path of a CLI tool, or None. Probes ~/.local/bin, /usr/local/bin,
    /usr/bin (GUI processes may not inherit the shell PATH), then falls back to
    shutil.which. Mirrors the macOS resolveBrewBinary probe order."""
    home = os.path.expanduser("~")
    for p in (f"{home}/.local/bin/{name}", f"/usr/local/bin/{name}", f"/usr/bin/{name}"):
        if os.access(p, os.X_OK):
            return p
    return shutil.which(name)
