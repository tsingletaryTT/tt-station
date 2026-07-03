#!/usr/bin/env python3
"""tt-station panel — a tiny GTK4 control surface for the box.

Runs ON the QuietBox. It supervises the `tt-station-agentd` daemon and shows,
at a glance, the one thing an operator standing at the box actually needs:

  • the 6-digit PAIRING CODE, big, the moment a client pairs (read from the
    agent's own stdout — no more log-scraping)
  • whether the box is linked up and serving (idle / serving:<model> + endpoint)
  • Start / Stop / Restart the agent, and Reset-to-fresh

Deliberately small — "enough to know hey, it's working," not a dashboard.
Pure PyGObject + GTK4 (no libadwaita), stdlib only otherwise.

Config is via env vars (sensible box defaults below) so the same file works on
any box:

  TTS_AGENT_BIN     path to tt-station-agentd            (default: ./target/release/tt-station-agentd)
  TTS_TT_BIN        path to tt (for Reset)               (default: ./target/release/tt)
  TTS_NAME          --name                               (default: qb2-lab)
  TTS_CTRL_PORT     --ctrl-port                          (default: 8765)
  TTS_SERVING_HOST  --serving-host                       (default: <hostname>.local)
  TTS_SERVING_PORT  --serving-port                       (default: 8003)
  TTS_REPO          --tt-inference-repo                  (default: ~/code/tt-inference-server)
  TTS_IMAGE         --serving-image (optional override)  (default: unset → agent auto-picks/pins)
  TTS_HF_ENV        file to read HF_TOKEN from           (default: <repo>/.env)
"""

import os
import re
import socket
import subprocess
import threading
import urllib.request
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk, Gdk  # noqa: E402

# ── Config ──────────────────────────────────────────────────────────────────
REPO = Path(os.environ.get("TTS_REPO", str(Path.home() / "code/tt-inference-server")))
HOSTNAME = socket.gethostname()
AGENT_BIN = os.environ.get("TTS_AGENT_BIN", "./target/release/tt-station-agentd")
TT_BIN = os.environ.get("TTS_TT_BIN", "./target/release/tt")
NAME = os.environ.get("TTS_NAME", "qb2-lab")
CTRL_PORT = os.environ.get("TTS_CTRL_PORT", "8765")
SERVING_HOST = os.environ.get("TTS_SERVING_HOST", f"{NAME}.local")
SERVING_PORT = os.environ.get("TTS_SERVING_PORT", "8003")
IMAGE = os.environ.get("TTS_IMAGE", "")  # empty → let the agent resolve/pin
HF_ENV = Path(os.environ.get("TTS_HF_ENV", str(REPO / ".env")))
PAIR_TTL_SECS = 120  # matches the agent's pairing-code TTL

CODE_RE = re.compile(r"pairing code:\s*(\d{6})")

CSS = b"""
window { background: #070d14; }
.title { font-size: 18px; font-weight: 700; color: #4fd1c5; }
.subtle { color: #607d8b; font-size: 11px; }
.pill { border-radius: 999px; padding: 2px 12px; font-weight: 700; font-size: 12px; }
.pill-idle  { background: rgba(96,125,139,0.25); color: #90a4ae; }
.pill-serve { background: rgba(104,211,145,0.18); color: #68d391; }
.pill-off   { background: rgba(252,129,129,0.18); color: #fc8181; }
.codecard { background: #0d2035; border: 1px solid rgba(79,209,197,0.35);
            border-radius: 14px; padding: 14px; }
.code { font-family: monospace; font-size: 54px; font-weight: 800;
        letter-spacing: 10px; color: #f4c471; }
.codehint { color: #607d8b; font-size: 12px; }
.endpoint { font-family: monospace; color: #63b3ed; font-size: 13px; }
.statusline { color: #e8f0f2; font-size: 13px; }
.log { font-family: monospace; color: #4a6070; font-size: 11px; }
button.suggested { background: #4fd1c5; color: #070d14; font-weight: 700; }
"""


def read_hf_token() -> str:
    try:
        for line in HF_ENV.read_text().splitlines():
            if line.startswith("HF_TOKEN="):
                return line.split("=", 1)[1].strip().strip("'\"")
    except OSError:
        pass
    return os.environ.get("HF_TOKEN", "")


class Panel(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="tt-station")
        self.set_default_size(440, 420)

        self.proc: subprocess.Popen | None = None
        self.code: str | None = None
        self.code_expires_at = 0.0

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        root.set_margin_top(16); root.set_margin_bottom(16)
        root.set_margin_start(18); root.set_margin_end(18)
        self.set_child(root)

        # header: title + status pill
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        title = Gtk.Label(label=f"tt-station · {NAME}", xalign=0)
        title.add_css_class("title"); title.set_hexpand(True)
        header.append(title)
        self.pill = Gtk.Label(label="stopped"); self.pill.add_css_class("pill")
        self.pill.add_css_class("pill-off")
        header.append(self.pill)
        root.append(header)

        sub = Gtk.Label(label=f"ctrl :{CTRL_PORT} · advertising _tenstorrent._tcp", xalign=0)
        sub.add_css_class("subtle"); root.append(sub)

        # pairing code card (the star of the show)
        card = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        card.add_css_class("codecard")
        self.code_label = Gtk.Label(label="——————"); self.code_label.add_css_class("code")
        card.append(self.code_label)
        self.code_hint = Gtk.Label(label="no pairing in progress", xalign=0.5)
        self.code_hint.add_css_class("codehint"); card.append(self.code_hint)
        root.append(card)

        # status + endpoint
        self.status_label = Gtk.Label(label="agent stopped", xalign=0)
        self.status_label.add_css_class("statusline"); root.append(self.status_label)
        self.endpoint_label = Gtk.Label(label="", xalign=0, selectable=True)
        self.endpoint_label.add_css_class("endpoint"); root.append(self.endpoint_label)

        # buttons
        btns = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8, homogeneous=True)
        self.btn_start = Gtk.Button(label="Start"); self.btn_start.add_css_class("suggested")
        self.btn_start.connect("clicked", lambda _b: self.start_agent())
        self.btn_stop = Gtk.Button(label="Stop")
        self.btn_stop.connect("clicked", lambda _b: self.stop_agent())
        self.btn_restart = Gtk.Button(label="Restart")
        self.btn_restart.connect("clicked", lambda _b: self.restart_agent())
        self.btn_reset = Gtk.Button(label="Reset")
        self.btn_reset.connect("clicked", lambda _b: self.reset_fresh())
        for b in (self.btn_start, self.btn_stop, self.btn_restart, self.btn_reset):
            btns.append(b)
        root.append(btns)

        self.log_label = Gtk.Label(label="", xalign=0, wrap=True, max_width_chars=52)
        self.log_label.add_css_class("log"); root.append(self.log_label)

        self._apply_css()
        self._refresh_buttons()
        GLib.timeout_add_seconds(2, self._poll_status)
        GLib.timeout_add(500, self._tick_code)
        self.connect("close-request", lambda _w: (self.stop_agent(), False)[1])

    # ── styling ──
    def _apply_css(self):
        prov = Gtk.CssProvider(); prov.load_from_data(CSS)
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(), prov, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

    def _log(self, msg: str):
        self.log_label.set_text(msg)

    # ── agent supervision ──
    def _agent_cmd(self):
        cmd = [AGENT_BIN, "--name", NAME, "--ctrl-port", CTRL_PORT,
               "--backend", "runpy", "--tt-inference-repo", str(REPO),
               "--serving-host", SERVING_HOST, "--serving-port", SERVING_PORT]
        if IMAGE:
            cmd += ["--serving-image", IMAGE]
        return cmd  # device auto-detected; token-store default; HF via env

    def running(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def start_agent(self):
        if self.running():
            return
        env = dict(os.environ)
        tok = read_hf_token()
        if tok:
            env["HF_TOKEN"] = tok
        try:
            self.proc = subprocess.Popen(
                self._agent_cmd(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True, bufsize=1, env=env)
        except OSError as e:
            self._log(f"start failed: {e}")
            return
        self._log("agent started")
        threading.Thread(target=self._read_output, args=(self.proc,), daemon=True).start()
        self._refresh_buttons()

    def stop_agent(self):
        if not self.running():
            return
        p = self.proc
        try:
            p.send_signal(2)  # SIGINT → graceful mDNS unregister
            try:
                p.wait(timeout=6)
            except subprocess.TimeoutExpired:
                p.terminate()
        except ProcessLookupError:
            pass
        self.proc = None
        self._clear_code()
        self._log("agent stopped")
        self._refresh_buttons()

    def restart_agent(self):
        self.stop_agent()
        GLib.timeout_add_seconds(1, lambda: (self.start_agent(), False)[1])

    def reset_fresh(self):
        """Reset the box to a fresh state via `tt reset` (stops model, clears tokens)."""
        base = f"127.0.0.1:{CTRL_PORT}"
        try:
            subprocess.Popen([TT_BIN, "reset", "--host", base, "--yes"],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self._log("reset-to-fresh requested (model stopped, pairings cleared)")
        except OSError as e:
            self._log(f"reset failed: {e}")
        self._clear_code()

    def _read_output(self, proc):
        for line in proc.stdout:  # blocks in this daemon thread
            m = CODE_RE.search(line)
            if m:
                GLib.idle_add(self._set_code, m.group(1))
        GLib.idle_add(self._on_exit)

    def _on_exit(self):
        if not self.running():
            self.proc = None
            self._refresh_buttons()

    # ── pairing code display + TTL ──
    def _set_code(self, code: str):
        self.code = code
        self.code_expires_at = GLib.get_monotonic_time() / 1e6 + PAIR_TTL_SECS
        self.code_label.set_text(f"{code[:3]} {code[3:]}")
        return False

    def _clear_code(self):
        self.code = None
        self.code_label.set_text("——————")
        self.code_hint.set_text("no pairing in progress")

    def _tick_code(self):
        if self.code:
            left = int(self.code_expires_at - GLib.get_monotonic_time() / 1e6)
            if left <= 0:
                self._clear_code()
            else:
                self.code_hint.set_text(f"enter this on your Mac · expires in {left}s")
        return True

    # ── status polling ──
    def _poll_status(self):
        threading.Thread(target=self._fetch_status, daemon=True).start()
        return True

    def _fetch_status(self):
        if not self.running():
            GLib.idle_add(self._render_status, None)
            return
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{CTRL_PORT}/status", timeout=3) as r:
                import json
                GLib.idle_add(self._render_status, json.loads(r.read().decode()))
        except Exception:
            GLib.idle_add(self._render_status, {"_unreachable": True})

    def _render_status(self, data):
        for c in ("pill-off", "pill-idle", "pill-serve"):
            self.pill.remove_css_class(c)
        if data is None:
            self.pill.set_text("stopped"); self.pill.add_css_class("pill-off")
            self.status_label.set_text("agent stopped")
            self.endpoint_label.set_text("")
        elif data.get("_unreachable"):
            self.pill.set_text("starting…"); self.pill.add_css_class("pill-idle")
            self.status_label.set_text("agent starting / unreachable")
            self.endpoint_label.set_text("")
        else:
            status = data.get("status", "idle")
            if status.startswith("serving:"):
                model = status.split(":", 1)[1]
                self.pill.set_text("serving"); self.pill.add_css_class("pill-serve")
                self.status_label.set_text(f"serving  {model}  ·  {data.get('chips','')}")
                self.endpoint_label.set_text(f"http://{SERVING_HOST}:{SERVING_PORT}/v1")
            else:
                self.pill.set_text("idle"); self.pill.add_css_class("pill-idle")
                self.status_label.set_text(f"idle  ·  {data.get('chips','')}  ·  ready to run a model")
                self.endpoint_label.set_text("")
        return False

    def _refresh_buttons(self):
        on = self.running()
        self.btn_start.set_sensitive(not on)
        self.btn_stop.set_sensitive(on)
        self.btn_restart.set_sensitive(on)
        self.btn_reset.set_sensitive(on)


def main():
    app = Gtk.Application(application_id="com.tenstorrent.ttstation.panel")
    app.connect("activate", lambda a: Panel(a).present())
    app.run(None)


if __name__ == "__main__":
    main()
