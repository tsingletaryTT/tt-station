# GTK Panel Connect Launchers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the on-box GTK panel one-click Connect launchers — Open WebUI (local docker), opencode (local terminal), and Copy/Open endpoint — for the model the box is serving.

**Architecture:** Pure, importable builder functions live in a new `box-panel/panel_launchers.py` (unit-tested with stdlib `unittest`); the panel imports them and adds a "Connect" row whose button handlers do the side effects (docker, terminal spawn, `xdg-open`, clipboard) in worker threads, marshaling status back with `GLib.idle_add`. Endpoint + model come from the `serving` list the panel already polls via `tt console --snapshot`.

**Tech Stack:** Python 3, PyGObject/GTK4, docker CLI, `xdg-open`, stdlib `unittest`.

## Global Constraints

- The panel runs ON the box — launchers are LOCAL (no SSH, no osascript, no IPv4 resolution).
- Reuse the macOS launcher recipes verbatim where they are pure: Open WebUI docker image `ghcr.io/open-webui/open-webui:main`, container name `ttstation-openwebui`, `-p <hostPort>:8080`, `--add-host=host.docker.internal:host-gateway`, `OPENAI_API_BASE_URL=http://host.docker.internal:<servingPort>/v1`, `OPENAI_API_KEY=sk-none`, `WEBUI_AUTH=false`, volume `ttstation-openwebui:/app/backend/data`. opencode config: provider `ttstation` = `@ai-sdk/openai-compatible`, `options.baseURL`, `models[<model>]`, top-level `model: ttstation/<model>` (rely on opencode's first-`/` split — do NOT re-split vendored ids).
- New env var: `TTS_OPENWEBUI_PORT` (default `3000`) for the published host port.
- Never raise out of a builder given odd snapshot input — mirror `derive_view`'s defensive style.
- Follow the panel's existing patterns: button wiring `Gtk.Button(...).connect("clicked", lambda _b: self.method())`; worker threads like `reset_fresh` + `GLib.idle_add`; inline status labels.
- No auto-install of tools on Linux — surface an actionable inline message when `docker`/`opencode`/a terminal emulator/`xdg-open` is missing.

---

### Task 1: Pure builders — endpoint resolution + opencode config

**Files:**
- Create: `box-panel/panel_launchers.py`
- Test: `box-panel/test_panel_launchers.py`

**Interfaces:**
- Produces:
  - `endpoint_from_snapshot(snap) -> tuple[str, str] | None` — returns `(base_url, model)` from the agent-source (else first) `serving` entry, or `None` when nothing is serving / snapshot is not a dict.
  - `serving_port_from_base_url(base_url, fallback) -> int` — parse the port from a `http://host:port/v1` string, else `fallback`.
  - `build_opencode_config(base_url, model) -> str` — the `opencode.json` text.

- [ ] **Step 1: Write the failing tests**

`box-panel/test_panel_launchers.py`:

```python
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'panel_launchers'`.

- [ ] **Step 3: Write the minimal implementation**

`box-panel/panel_launchers.py`:

```python
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: PASS (all tests in the three classes).

- [ ] **Step 5: Commit**

```bash
git add box-panel/panel_launchers.py box-panel/test_panel_launchers.py
git commit -m "feat(panel): pure builders for endpoint resolution + opencode config"
```

---

### Task 2: Pure builders — Open WebUI docker command, terminal resolution

**Files:**
- Modify: `box-panel/panel_launchers.py`
- Test: `box-panel/test_panel_launchers.py`

**Interfaces:**
- Consumes: the constants from Task 1.
- Produces:
  - `build_openwebui_command(serving_port, host_port=3000) -> str` — an idempotent shell script (reuse-if-running, retry pull, `docker run -d`).
  - `opencode_terminal_command(config_dir) -> str` — `cd '<dir>' && opencode`.
  - `resolve_terminal_emulator() -> list[str] | None` — argv prefix that runs a command in a new terminal window (e.g. `["x-terminal-emulator", "-e"]`), or None if none found.
  - `resolve_tool(name) -> str | None` — absolute path of a CLI tool, probing common dirs (opencode PATH-resolution parity with the macOS `resolveBrewBinary`).

- [ ] **Step 1: Write the failing tests** (append to `box-panel/test_panel_launchers.py`, before the `if __name__` guard)

```python
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: FAIL — `AttributeError: module 'panel_launchers' has no attribute 'build_openwebui_command'`.

- [ ] **Step 3: Add the implementations to `box-panel/panel_launchers.py`**

```python
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
    konsole, xterm. Each of these takes `-e <cmd...>` to run a command. The
    caller appends `bash -lc "<command>"` so PATH resolves opencode via a login
    shell (GUI apps don't inherit the shell PATH).
    """
    for term in ("x-terminal-emulator", "gnome-terminal", "konsole", "xterm"):
        path = shutil.which(term)
        if path:
            return [path, "-e"]
    return None


def resolve_tool(name):
    """Absolute path of a CLI tool, or None. Probes ~/.local/bin, /usr/local/bin,
    /usr/bin (GUI processes may not inherit the shell PATH), then falls back to
    shutil.which. Mirrors the macOS resolveBrewBinary probe order."""
    import os
    home = os.path.expanduser("~")
    for p in (f"{home}/.local/bin/{name}", f"/usr/local/bin/{name}", f"/usr/bin/{name}"):
        if os.access(p, os.X_OK):
            return p
    return shutil.which(name)
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: PASS (all classes).

- [ ] **Step 5: Commit**

```bash
git add box-panel/panel_launchers.py box-panel/test_panel_launchers.py
git commit -m "feat(panel): Open WebUI docker command + terminal/tool resolution builders"
```

---

### Task 3: Wire the Connect row into the panel

**Files:**
- Modify: `box-panel/tt-station-panel.py` (imports ~line 53-64; config ~line 66-92; widget build in `__init__` after the button row ~line 476; `_render_snapshot` ~line 653; add new handler methods)

**Interfaces:**
- Consumes: `panel_launchers.endpoint_from_snapshot`, `serving_port_from_base_url`, `build_opencode_config`, `build_openwebui_command`, `opencode_terminal_command`, `resolve_terminal_emulator`, `resolve_tool` (Tasks 1–2).
- Produces: a Connect row shown only when serving; four handlers (`connect_openwebui`, `connect_opencode`, `connect_copy_endpoint`, `connect_open_endpoint`).

- [ ] **Step 1: Import the builders and add the OpenWebUI-port config**

In `box-panel/tt-station-panel.py`, after the existing `import` block that ends with the `gi.repository` import (~line 64), add:

```python
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import panel_launchers as pl  # noqa: E402
```

Then in the config section (after `SERVING_PORT`, ~line 83) add:

```python
# Published host port for the on-box Open WebUI container (8080 is taken on QB2).
OPENWEBUI_PORT = int(os.environ.get("TTS_OPENWEBUI_PORT", "3000"))
```

- [ ] **Step 2: Build the Connect row in `__init__`**

Immediately after the button row is appended (`root.append(btns)`, ~line 476) and before `self.log_label = …`, insert:

```python
        # ── Connect row: one-click launchers for the serving model. Shown only
        # while the box is serving (see _refresh_connect). Mirrors the macOS
        # app's Connect card, run LOCALLY here (docker / terminal / xdg-open).
        self.connect_row = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        connect_hdr = Gtk.Label(label="Connect", xalign=0)
        connect_hdr.add_css_class("subtle")
        self.connect_row.append(connect_hdr)
        connect_btns = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8, homogeneous=True)
        self.btn_webui = Gtk.Button(label="Open WebUI")
        self.btn_webui.set_tooltip_text(
            "Start Open WebUI (a browser chat UI) as a docker container on this box, "
            "wired to the serving model's /v1, then open it in the browser.")
        self.btn_webui.connect("clicked", lambda _b: self.connect_openwebui())
        self.btn_opencode = Gtk.Button(label="opencode")
        self.btn_opencode.set_tooltip_text(
            "Open a terminal running opencode (a coding agent) pointed at the "
            "serving model's /v1.")
        self.btn_opencode.connect("clicked", lambda _b: self.connect_opencode())
        self.btn_copy_ep = Gtk.Button(label="Copy /v1")
        self.btn_copy_ep.set_tooltip_text("Copy the serving /v1 base URL to the clipboard.")
        self.btn_copy_ep.connect("clicked", lambda _b: self.connect_copy_endpoint())
        self.btn_open_ep = Gtk.Button(label="Open endpoint")
        self.btn_open_ep.set_tooltip_text("Open the serving /v1 base URL in the browser.")
        self.btn_open_ep.connect("clicked", lambda _b: self.connect_open_endpoint())
        for b in (self.btn_webui, self.btn_opencode, self.btn_copy_ep, self.btn_open_ep):
            connect_btns.append(b)
        self.connect_row.append(connect_btns)
        self.connect_status = Gtk.Label(label="", xalign=0, wrap=True, max_width_chars=52)
        self.connect_status.add_css_class("subtle")
        self.connect_row.append(self.connect_status)
        self.connect_row.set_visible(False)
        root.append(self.connect_row)
```

- [ ] **Step 3: Refresh the Connect row from each snapshot**

At the end of `_render_snapshot` (after the pairing-code branch, ~line 670), add:

```python
        self._refresh_connect()
```

Then add this helper method to the `Panel` class (near `_refresh_buttons`):

```python
    def _refresh_connect(self):
        """Show the Connect row only when the box is serving something, and
        stash the current (base_url, model) for the launchers to use."""
        self._endpoint = pl.endpoint_from_snapshot(self.snapshot)
        self.connect_row.set_visible(self._endpoint is not None)
```

Initialize `self._endpoint = None` in `__init__` (near the other state fields such as `self.snapshot`).

- [ ] **Step 4: Add the four handler methods**

Add to the `Panel` class (after `reset_fresh`, following its worker-thread + `GLib.idle_add` pattern):

```python
    # ── Connect launchers (local: docker / terminal / xdg-open) ──
    def _connect_log(self, msg: str):
        self.connect_status.set_text(msg)

    def connect_copy_endpoint(self):
        if not self._endpoint:
            return
        base_url, _model = self._endpoint
        clip = Gdk.Display.get_default().get_clipboard()
        clip.set(base_url)
        self._connect_log(f"copied: {base_url}")

    def connect_open_endpoint(self):
        if not self._endpoint:
            return
        base_url, _model = self._endpoint
        if not pl.resolve_tool("xdg-open"):
            self._connect_log("xdg-open not found — install xdg-utils.")
            return
        subprocess.run(["xdg-open", base_url], check=False)

    def connect_opencode(self):
        if not self._endpoint:
            return
        base_url, model = self._endpoint
        if not pl.resolve_tool("opencode"):
            self._connect_log("opencode not installed — install it, then retry.")
            return
        term = pl.resolve_terminal_emulator()
        if not term:
            self._connect_log("no terminal emulator found (x-terminal-emulator/gnome-terminal/konsole/xterm).")
            return
        # Per-endpoint scratch dir under ~/.local/share, keyed by a safe form of
        # the base URL (mirrors the macOS scratchDir).
        safe = base_url.replace("https://", "").replace("http://", "").replace("/", "_").replace(":", "_")
        cfg_dir = Path.home() / ".local/share/tt-station/opencode" / safe
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "opencode.json").write_text(pl.build_opencode_config(base_url, model))
        cmd = pl.opencode_terminal_command(str(cfg_dir))
        # Run through a login shell so PATH resolves opencode.
        subprocess.Popen([*term, "bash", "-lc", cmd])
        self._connect_log(f"opencode launched in a terminal ({model}).")

    def connect_openwebui(self):
        if not self._endpoint:
            return
        base_url, _model = self._endpoint
        if not pl.resolve_tool("docker"):
            self._connect_log("docker not found — install docker.io, then retry.")
            return
        serving_port = pl.serving_port_from_base_url(base_url, int(SERVING_PORT))
        self._connect_log("starting Open WebUI on the box (first run pulls the image)…")

        def worker():
            script = pl.build_openwebui_command(serving_port, host_port=OPENWEBUI_PORT)
            subprocess.run(["bash", "-lc", script], check=False)
            # Poll the local health endpoint (~180s: first run pulls + inits).
            url = f"http://localhost:{OPENWEBUI_PORT}"
            health = f"{url}/health"
            import urllib.request
            for _ in range(180):
                try:
                    with urllib.request.urlopen(health, timeout=2) as r:
                        if r.status == 200:
                            GLib.idle_add(self._openwebui_ready, url)
                            return
                except OSError:
                    pass
                import time
                time.sleep(1)
            GLib.idle_add(self._connect_log,
                          f"Open WebUI didn't come up on :{OPENWEBUI_PORT} — retry shortly.")

        threading.Thread(target=worker, daemon=True).start()

    def _openwebui_ready(self, url: str):
        if pl.resolve_tool("xdg-open"):
            subprocess.run(["xdg-open", url], check=False)
            self._connect_log(f"Open WebUI ready — opened {url}")
        else:
            self._connect_log(f"Open WebUI ready at {url} (xdg-open missing).")
```

- [ ] **Step 5: Verify the panel still parses**

Run: `python3 -c "import ast; ast.parse(open('box-panel/tt-station-panel.py').read()); print('parse ok')"`
Expected: `parse ok`.

- [ ] **Step 6: Re-run the builder unit tests (unaffected, but confirm the import path)**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add box-panel/tt-station-panel.py
git commit -m "feat(panel): Connect row — Open WebUI / opencode / copy-open endpoint"
```

---

### Task 4: Owner click-through on the live box (manual verification)

**Files:** none (verification only).

- [ ] **Step 1: Launch the panel against the live box**

Run: `cd /home/ttuser/code/tt-station && python3 box-panel/tt-station-panel.py`
Expected: panel opens; if a model is serving, the "Connect" row is visible (hidden if idle).

- [ ] **Step 2: Serve a model, then exercise each launcher**

With a model serving (via the panel's Start / a `tt run`):
- Click **Copy /v1** → status shows `copied: http://…/v1`; paste to confirm.
- Click **Open endpoint** → browser opens the `/v1` URL.
- Click **opencode** → a terminal opens running opencode pointed at the box (a per-endpoint `opencode.json` exists under `~/.local/share/tt-station/opencode/`).
- Click **Open WebUI** → status shows "starting…", then a browser tab opens a working chat that completes against the box's model. (First run pulls the image — allow up to ~3 min.)

- [ ] **Step 3: Confirm the missing-tool messages (optional)**

Temporarily rename/remove a tool from PATH (or test on a box lacking it) and confirm the inline message is actionable rather than a crash or a "command not found" terminal.

- [ ] **Step 4: Note results in the project CLAUDE.md**

Record what was verified (which launchers, on which box, any slips fixed) in `CLAUDE.md`'s state section, per the project's logging convention.

---

## Self-Review Notes

- **Spec coverage:** Open WebUI docker (Task 2 builder + Task 3 handler), opencode terminal (Tasks 1–3), copy/open endpoint (Task 3), endpoint from snapshot's serving list (Task 1), Connect row shown only when serving (Task 3), pure builders unit-tested + glue owner-verified (Tasks 1–2 tests, Task 4), `TTS_OPENWEBUI_PORT` (Task 3). All covered.
- **Type consistency:** `endpoint_from_snapshot` returns `(base_url, model)` everywhere; handlers unpack it identically. `resolve_tool`/`resolve_terminal_emulator` names consistent across tasks.
- **No SSH / IPv4 dance:** deliberately dropped — panel is local to the box.
