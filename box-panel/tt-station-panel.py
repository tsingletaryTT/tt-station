#!/usr/bin/env python3
"""tt-station panel — a tiny GTK4 control surface for the box.

Runs ON the QuietBox. It shows, at a glance, the one thing an operator
standing at the box actually needs:

  • the 6-digit PAIRING CODE, big, the moment a client pairs
  • whether the box is linked up and serving (idle / serving:<model> + endpoint)
  • Start / Stop / Restart the agent, and Reset-to-fresh

The agent (`tt-station-agentd`) runs as a `systemctl --user` service
(`tt-station-agentd.service`) — the SAME lifecycle model `tt console` (the
operator TUI) uses. This panel does not spawn or supervise a child process
itself: Start/Stop/Restart just shell `systemctl --user <verb>`, and ALL
state (service state, pairing code + TTL, serving status/endpoint, active
profile) comes from a single poll of `tt console --snapshot` — the exact
same `BoxLifecycleSnapshot` JSON the TUI renders. One source of truth: the
panel and `tt console` can never disagree about what state the box is in.
Closing this window does NOT stop the service — it just stops watching it.

Deliberately small — "enough to know hey, it's working," not a dashboard.
Pure PyGObject + GTK4 (no libadwaita), stdlib only otherwise.

Config is via env vars (sensible box defaults below) so the same file works on
any box:

  TTS_SERVICE_NAME  systemd --user unit to control        (default: tt-station-agentd.service)
  TTS_AGENT_BIN     agent binary name/path baked into a    (default: tt-station-agentd)
                     profile drop-in's ExecStart= line —
                     mirrors the Rust side's
                     `console::names::ToolNames::agent_bin`
                     default; NOT used to spawn a process
  TTS_TT_BIN        path to tt (for --snapshot and Reset)  (default: ./target/release/tt)
  TTS_NAME          box name, shown in the window title    (default: qb2-lab)
  TTS_CTRL_PORT     --ctrl-port passed to `tt console`     (default: 8765)
  TTS_SERVING_HOST  fallback endpoint host, used only if   (default: <hostname>.local)
                     the snapshot doesn't carry one yet
  TTS_SERVING_PORT  fallback endpoint port (see above)     (default: 8003)
  TTS_REPO          base dir for TTS_HF_ENV's default      (default: ~/code/tt-inference-server)
  TTS_HF_ENV        file to read HF_TOKEN from (display    (default: <repo>/.env)
                     only now — the systemd service's own
                     environment is what the agent actually uses)
  TTS_CONFIG        agentd.toml path (profile dropdown)    (default: $TT_CONFIG_DIR/agentd.toml or
                                                             ~/.config/tt-station/agentd.toml)
"""

import json
import os
import socket
import subprocess
import threading
import tomllib
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk, Gdk  # noqa: E402

# ── Config ──────────────────────────────────────────────────────────────────
REPO = Path(os.environ.get("TTS_REPO", str(Path.home() / "code/tt-inference-server")))
HOSTNAME = socket.gethostname()
# The binary NAME/path baked into a profile drop-in's `ExecStart=` line (see
# `apply_profile`/`render_profile_dropin` below) — it is NOT used to launch a
# child process anymore. Default matches the Rust side's
# `console::names::ToolNames::agent_bin` default exactly, so a box with no
# override agrees with `tt console` about what the unit actually execs.
AGENT_BIN = os.environ.get("TTS_AGENT_BIN", "tt-station-agentd")
# The systemd --user unit this panel controls and polls. Default matches the
# Rust side's `console::names::ToolNames::service_name` default exactly.
SERVICE_NAME = os.environ.get("TTS_SERVICE_NAME", "tt-station-agentd.service")
TT_BIN = os.environ.get("TTS_TT_BIN", "./target/release/tt")
NAME = os.environ.get("TTS_NAME", "qb2-lab")
CTRL_PORT = os.environ.get("TTS_CTRL_PORT", "8765")
SERVING_HOST = os.environ.get("TTS_SERVING_HOST", f"{NAME}.local")
SERVING_PORT = os.environ.get("TTS_SERVING_PORT", "8003")

# Branding assets. The Tenstorrent mark (shared with the macOS app, recolored to
# the panel's teal) shows in the window header and — via the .desktop install
# below — in the dock/taskbar. One day this gets replaced with something more
# tt-station-specific; until then it's the same logo used everywhere else.
APP_ID = "com.tenstorrent.ttstation.panel"
ASSETS = Path(__file__).resolve().parent / "assets"
LOGO = ASSETS / "tt-logo.png"
HF_ENV = Path(os.environ.get("TTS_HF_ENV", str(REPO / ".env")))
PAIR_TTL_SECS = 120  # matches the agent's pairing-code TTL (fallback only —
                      # the real TTL comes from the snapshot's own `pairing`)

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


def read_profiles() -> tuple[list[str], str | None]:
    """Read named-profile info from the box-local agentd.toml, for the dropdown.

    Mirrors the agent's own config-file resolution order: `TTS_CONFIG` env
    var, else `$TT_CONFIG_DIR/agentd.toml`, else `~/.config/tt-station/agentd.toml`.

    GRACEFUL DEGRADATION: any error at all (no file, unreadable, bad TOML, no
    `[profile.*]` tables) returns `([], None)` — the panel must keep working
    exactly as before with no config file, i.e. no profile dropdown shown.
    This is deliberately broad (bare `except Exception`) because a malformed
    box-local file should never stop the panel from starting.
    """
    path = os.environ.get("TTS_CONFIG") or os.path.join(
        os.environ.get("TT_CONFIG_DIR", os.path.expanduser("~/.config/tt-station")),
        "agentd.toml")
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
        return sorted(data.get("profile", {}).keys()), data.get("default_profile")
    except Exception:
        return [], None


def _systemctl(verb: str) -> None:
    """Shell `systemctl --user <verb> <SERVICE_NAME>`, fire-and-forget.

    `check=False` deliberately: a failing `systemctl` call (e.g. the unit
    isn't installed yet, or the user session bus isn't up) must never crash
    the panel — the next `tt console --snapshot` poll will simply keep
    reporting whatever state actually resulted (see `_service_state_of`).
    """
    subprocess.run(["systemctl", "--user", verb, SERVICE_NAME], check=False)


def render_profile_dropin(agent_bin: str, profile: str) -> str:
    """Systemd drop-in content that pins the service to `--profile <profile>`.

    Must be EXACTLY the format the Rust side emits
    (`crates/tt/src/console/actions.rs::render_profile_dropin`) — the blank
    `ExecStart=` line first clears the unit's original `ExecStart=` before
    the real one is set (systemd accumulates multiple `ExecStart=` values
    across drop-ins otherwise), then the real one pins the profile.
    """
    return f"[Service]\nExecStart=\nExecStart={agent_bin} --profile {profile}\n"


def profile_dropin_path() -> Path:
    """`<config_dir>/<unit>.d/profile.conf`, matching the Rust side's
    `LifecycleActions::set_profile` path resolution
    (`$XDG_CONFIG_HOME/systemd/user`, else `~/.config/systemd/user`)."""
    xdg = os.environ.get("XDG_CONFIG_HOME")
    base = Path(xdg) if xdg else Path.home() / ".config"
    return base / "systemd" / "user" / f"{SERVICE_NAME}.d" / "profile.conf"


def derive_view(snap, profile_names, selected_profile, serving_host, serving_port):
    """Pure snapshot → view-model mapping. NO GTK/widget access here.

    Deliberately split out of `Panel._render_snapshot` so it's testable with
    a plain function call (see the throwaway check run for this task) rather
    than needing a live GTK application/display. `snap` is either a decoded
    `BoxLifecycleSnapshot` dict (from `tt console --snapshot`) or `None`
    (the `tt` invocation itself failed/couldn't be parsed — the agent AND
    systemd are both unreachable). Every field on `snap` may independently
    be null per the wire contract (`libttstation::model::BoxLifecycleSnapshot`)
    — this function must never raise regardless of which fields are missing.

    Returns a dict of plain values the caller pokes into widgets:
      pill_text, pill_class, status_text, endpoint_text, profile_text,
      serving_text, code (str | None), ttl (int seconds).
    """
    if not isinstance(snap, dict):
        return {
            "pill_text": "unknown",
            "pill_class": "pill-off",
            "status_text": "unable to read box state (tt console --snapshot failed)",
            "endpoint_text": "",
            "profile_text": "",
            "serving_text": "",  # offline: nothing to summarize, don't render
            "code": None,
            "ttl": 0,
        }

    service = snap.get("service") or "unknown"
    reachable = bool(snap.get("reachable"))
    status_str = snap.get("status")  # "idle" / "serving:<model>" / None
    chips = snap.get("chips") or ""
    endpoint = snap.get("endpoint") or {}

    if service in ("inactive", "failed", "unknown"):
        label = {"inactive": "stopped", "failed": "failed", "unknown": "unknown"}[service]
        pill_text, pill_class = label, "pill-off"
        status_text, endpoint_text = f"agent {label}", ""
    elif service == "deactivating":
        pill_text, pill_class = "stopping…", "pill-idle"
        status_text, endpoint_text = "agent stopping", ""
    elif not reachable:
        # active/activating per systemd, but not yet answering HTTP.
        pill_text, pill_class = "starting…", "pill-idle"
        status_text, endpoint_text = f"agent {service} / unreachable", ""
    elif status_str and status_str.startswith("serving:"):
        model = status_str.split(":", 1)[1]
        pill_text, pill_class = "serving", "pill-serve"
        status_text = f"serving  {model}  ·  {chips}"
        # `/endpoint` collection is v1-unimplemented on the Rust side (it's
        # an authed route; `tt console`'s collector only probes unauthed
        # endpoints today — see `console::env::collect_snapshot`), so
        # `endpoint` is `None` in practice. Fall back to the configured
        # serving host/port, same as the panel did pre-migration.
        endpoint_text = endpoint.get("base_url") or f"http://{serving_host}:{serving_port}/v1"
    else:
        pill_text, pill_class = "idle", "pill-idle"
        status_text = f"idle  ·  {chips}  ·  ready to run a model"
        endpoint_text = ""

    # active-profile line, straight from the snapshot's redacted config when
    # we have it (the ground truth for what's actually serving); fall back
    # to the dropdown's current pick when the agent isn't reachable yet, so
    # the line isn't just blank while starting.
    config = snap.get("config")
    if config is not None:
        active = config.get("active_profile")
        profile_text = f"active profile: {active}" if active else "active profile: (implicit default)"
    elif profile_names:
        profile_text = f"profile selected: {selected_profile}" if selected_profile else ""
    else:
        profile_text = ""

    pairing = snap.get("pairing")
    code = pairing.get("code") if pairing else None
    ttl = (pairing.get("expires_in_secs") or 0) if pairing else 0

    # Compact one-line summary of `GET /serving` (every live `/v1` endpoint
    # on the box -- the agent's own PLUS anything external like tt-studio;
    # see `libttstation::model::ServingEntry`/`ServingList`). This is
    # deliberately NOT a per-endpoint dashboard -- one line, count +
    # agent/external breakdown, with the external entry's host:port/model
    # appended when there is one, since that's the case this panel couldn't
    # show before (the agent only ever knew about its own process).
    serving = snap.get("serving") or []
    if serving:
        agent_n = sum(1 for e in serving if e.get("source") == "agent")
        external_entries = [e for e in serving if e.get("source") == "external"]
        external_n = len(external_entries)
        other_n = len(serving) - agent_n - external_n
        parts = []
        if agent_n:
            parts.append(f"{agent_n} agent")
        if external_n:
            parts.append(f"{external_n} external")
        if other_n:
            parts.append(f"{other_n} other")
        breakdown = " · ".join(parts)
        serving_text = f"endpoints: {len(serving)} live"
        if breakdown:
            serving_text += f" ({breakdown})"
        if external_entries:
            ext = external_entries[0]
            model = ext.get("model") or "?"
            port = ext.get("host_port")
            serving_text += f" — external: {model} (:{port})" if port else f" — external: {model}"
    else:
        serving_text = "endpoints: none"

    return {
        "pill_text": pill_text,
        "pill_class": pill_class,
        "status_text": status_text,
        "endpoint_text": endpoint_text,
        "profile_text": profile_text,
        "serving_text": serving_text,
        "code": code,
        "ttl": ttl,
    }


class Panel(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="tt-station")
        self.set_default_size(440, 420)

        # The full last-polled snapshot (a `BoxLifecycleSnapshot` dict, or
        # `None` before the first poll / when `tt console --snapshot` itself
        # fails) — the single source of truth for button sensitivity and
        # every rendered field. There is deliberately no child-process
        # handle here anymore: the agent's lifecycle is systemd's problem.
        self.snapshot: dict | None = None
        self.code: str | None = None
        self.code_expires_at = 0.0

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        root.set_margin_top(16); root.set_margin_bottom(16)
        root.set_margin_start(18); root.set_margin_end(18)
        self.set_child(root)

        # header: logo + title + status pill
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        if LOGO.exists():
            logo = Gtk.Image.new_from_file(str(LOGO))
            logo.set_pixel_size(28)  # crisp: source is 256px, shown at 28
            header.append(logo)
        title = Gtk.Label(label=f"tt-station · {NAME}", xalign=0)
        title.add_css_class("title"); title.set_hexpand(True)
        header.append(title)
        self.pill = Gtk.Label(label="stopped"); self.pill.add_css_class("pill")
        self.pill.add_css_class("pill-off")
        header.append(self.pill)
        root.append(header)

        sub = Gtk.Label(label=f"ctrl :{CTRL_PORT} · advertising _tenstorrent._tcp", xalign=0)
        sub.add_css_class("subtle"); root.append(sub)

        # profile dropdown — populated from the box-local agentd.toml
        # (read_profiles()). Hidden entirely when there's no config file, so
        # a box with no agentd.toml behaves exactly as before this feature.
        # Switching profiles now writes a systemd drop-in + restarts (see
        # `apply_profile`) rather than passing `--profile` at process launch,
        # since systemctl start/restart take no argv of their own.
        self.profile_names, default_profile = read_profiles()
        self.profile_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        profile_caption = Gtk.Label(label="profile:", xalign=0)
        profile_caption.add_css_class("subtle")
        self.profile_row.append(profile_caption)
        self.profile_combo = Gtk.ComboBoxText()
        for name in self.profile_names:
            self.profile_combo.append_text(name)
        if self.profile_names:
            default_idx = (self.profile_names.index(default_profile)
                           if default_profile in self.profile_names else 0)
            self.profile_combo.set_active(default_idx)
        self.profile_row.append(self.profile_combo)
        self.profile_apply_btn = Gtk.Button(label="Apply")
        self.profile_apply_btn.set_tooltip_text(
            "Pin the agent to this profile: writes a systemd drop-in "
            f"(~/.config/systemd/user/{SERVICE_NAME}.d/profile.conf), reloads "
            "the systemd user manager, and restarts the service so it takes effect.")
        self.profile_apply_btn.connect("clicked", lambda _b: self.apply_profile())
        self.profile_row.append(self.profile_apply_btn)
        self.profile_row.set_visible(bool(self.profile_names))
        root.append(self.profile_row)

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
        # compact one-line summary of `GET /serving` — every live `/v1` on
        # the box, agent + external (e.g. tt-studio). Blank until the first
        # snapshot lands / while offline, same treatment as endpoint_label.
        self.serving_label = Gtk.Label(label="", xalign=0, selectable=True)
        self.serving_label.add_css_class("subtle"); root.append(self.serving_label)
        # what's actually running, per the snapshot's own config summary —
        # separate from the dropdown above so a dropdown change that hasn't
        # been Applied yet doesn't look like it already took effect.
        self.profile_status_label = Gtk.Label(label="", xalign=0)
        self.profile_status_label.add_css_class("subtle"); root.append(self.profile_status_label)

        # buttons
        btns = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8, homogeneous=True)
        # Tooltips spell out exactly what each button does — "Reset" in
        # particular is vague on its own, and it's the destructive one.
        self.btn_start = Gtk.Button(label="Start"); self.btn_start.add_css_class("suggested")
        self.btn_start.set_tooltip_text(
            "Start the tt-station-agentd systemd service (systemctl --user start). "
            "Advertises on the LAN (mDNS) so clients can discover, pair, and run "
            "models. The panel doesn't supervise the process directly, so closing "
            "this window later will NOT stop it.")
        self.btn_start.connect("clicked", lambda _b: self.start_agent())
        self.btn_stop = Gtk.Button(label="Stop")
        self.btn_stop.set_tooltip_text(
            "Stop the tt-station-agentd systemd service (systemctl --user stop) — "
            "the control API and LAN advertisement go offline. A model already "
            "serving in its container keeps running; use Reset to stop that too.")
        self.btn_stop.connect("clicked", lambda _b: self.stop_agent())
        self.btn_restart = Gtk.Button(label="Restart")
        self.btn_restart.set_tooltip_text(
            "Restart the tt-station-agentd systemd service (systemctl --user "
            "restart) — picks up a rebuilt binary or config/profile changes. A "
            "model already serving is left running.")
        self.btn_restart.connect("clicked", lambda _b: self.restart_agent())
        self.btn_reset = Gtk.Button(label="Reset")
        self.btn_reset.set_tooltip_text(
            "Return the box to a fresh state (tt reset): stop the serving model, "
            "clear ALL client pairings (every client must pair again), and reset the "
            "Blackhole board (tt-smi -r). Use when the board is wedged or before "
            "handing the box off.")
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
        # NOTE: deliberately no `close-request` handler anymore — the agent
        # is a systemd-managed service now, independent of this window's
        # lifetime. Closing the panel must not stop it.

    # ── styling ──
    def _apply_css(self):
        prov = Gtk.CssProvider(); prov.load_from_data(CSS)
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(), prov, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

    def _log(self, msg: str):
        self.log_label.set_text(msg)

    # ── agent lifecycle (systemd --user) ──
    def start_agent(self):
        _systemctl("start")
        self._log("start requested (systemctl --user start)")

    def stop_agent(self):
        _systemctl("stop")
        self._log("stop requested (systemctl --user stop)")

    def restart_agent(self):
        _systemctl("restart")
        self._log("restart requested (systemctl --user restart)")

    def apply_profile(self):
        """Pin the running agent to the dropdown's selected profile.

        Systemctl start/restart take no argv, so a profile switch can no
        longer be "pass --profile on the next launch" — instead this writes
        a drop-in overriding `ExecStart=` (exactly mirroring the Rust side's
        `LifecycleActions::set_profile`), reloads the systemd user manager,
        and restarts the unit so the new `ExecStart=` actually takes effect.
        """
        if not self.profile_names:
            return
        selected = self.profile_combo.get_active_text()
        if not selected:
            return
        try:
            path = profile_dropin_path()
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(render_profile_dropin(AGENT_BIN, selected))
        except OSError as e:
            self._log(f"profile drop-in write failed: {e}")
            return
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=False)
        _systemctl("restart")
        self._log(f"profile pinned to {selected!r} (drop-in written, service restarting)")

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

    # ── pairing code display + TTL ──
    def _set_code(self, code: str, ttl_secs: int):
        self.code = code
        self.code_expires_at = GLib.get_monotonic_time() / 1e6 + (ttl_secs or PAIR_TTL_SECS)
        self.code_label.set_text(f"{code[:3]} {code[3:]}")

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

    # ── snapshot polling — the single source of truth, shared with `tt console` ──
    def _poll_status(self):
        threading.Thread(target=self._fetch_snapshot, daemon=True).start()
        return True

    def _fetch_snapshot(self):
        """Run `tt console --snapshot --ctrl-port <CTRL_PORT>` and decode its
        JSON. Never raises out of this method: any failure (missing `tt`
        binary, non-zero exit, non-JSON stdout) degrades to `snap = None`,
        which `derive_view` renders as a safe "unknown/offline" state — the
        agent AND the systemd unit being completely absent must never crash
        the panel.
        """
        snap = None
        try:
            out = subprocess.run(
                [TT_BIN, "console", "--snapshot", "--ctrl-port", CTRL_PORT],
                capture_output=True, text=True, timeout=10)
            if out.returncode == 0:
                snap = json.loads(out.stdout)
        except (OSError, subprocess.TimeoutExpired, ValueError):
            snap = None
        GLib.idle_add(self._render_snapshot, snap)

    def _render_snapshot(self, snap):
        self.snapshot = snap
        selected = self.profile_combo.get_active_text() if self.profile_names else None
        view = derive_view(snap, self.profile_names, selected, SERVING_HOST, SERVING_PORT)

        for c in ("pill-off", "pill-idle", "pill-serve"):
            self.pill.remove_css_class(c)
        self.pill.set_text(view["pill_text"])
        self.pill.add_css_class(view["pill_class"])
        self.status_label.set_text(view["status_text"])
        self.endpoint_label.set_text(view["endpoint_text"])
        self.serving_label.set_text(view["serving_text"])
        self.profile_status_label.set_text(view["profile_text"])

        if view["code"]:
            self._set_code(view["code"], view["ttl"])
        else:
            self._clear_code()

        self._refresh_buttons()
        return False

    def _service_state(self) -> str:
        return (self.snapshot or {}).get("service", "unknown")

    def _refresh_buttons(self):
        on = self._service_state() in ("active", "activating", "deactivating")
        self.btn_start.set_sensitive(not on)
        self.btn_stop.set_sensitive(on)
        self.btn_restart.set_sensitive(on)
        self.btn_reset.set_sensitive(on)


def install_desktop_icon():
    """Best-effort: make the dock/taskbar show the Tenstorrent icon for this app.

    GTK4 has no API to set a toplevel window's icon directly. On Wayland the
    compositor finds it by matching the window's app_id (our `APP_ID`, set on
    the Gtk.Application below) to a `.desktop` file whose `Icon=` resolves in a
    standard icon-theme dir. So we copy our hicolor PNGs into the user's icon
    theme and drop a matching `.desktop`. Idempotent, and wrapped so any
    failure here can never stop the panel from opening.
    """
    import shutil

    try:
        data = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share"))
        # 1. icons into the hicolor theme the compositor actually searches
        for size in ("48x48", "128x128", "256x256"):
            src = ASSETS / "icons" / "hicolor" / size / "apps" / f"{APP_ID}.png"
            if not src.exists():
                continue
            dst = data / "icons" / "hicolor" / size / "apps" / f"{APP_ID}.png"
            dst.parent.mkdir(parents=True, exist_ok=True)
            if not dst.exists() or dst.read_bytes() != src.read_bytes():
                shutil.copyfile(src, dst)
        # 2. a .desktop keyed to APP_ID so the compositor associates the icon
        apps = data / "applications"
        apps.mkdir(parents=True, exist_ok=True)
        script = Path(__file__).resolve()
        repo_root = script.parent.parent  # <repo>/box-panel/.. → repo root (for ./target paths)
        desktop = apps / f"{APP_ID}.desktop"
        content = (
            "[Desktop Entry]\n"
            "Type=Application\n"
            "Name=tt-station panel\n"
            "Comment=Tenstorrent QuietBox agent control panel\n"
            f"Exec=python3 {script}\n"
            f"Path={repo_root}\n"
            f"Icon={APP_ID}\n"
            f"StartupWMClass={APP_ID}\n"
            "Terminal=false\n"
            "Categories=Development;\n"
        )
        if not desktop.exists() or desktop.read_text() != content:
            desktop.write_text(content)
        # 3. refresh caches so the desktop shell notices the new .desktop/icon.
        # KDE Plasma reads its own "sycoca" cache (kbuildsycoca), NOT
        # update-desktop-database, so a new .desktop is invisible to the
        # taskbar until that runs — hence both are attempted. All best-effort:
        # whichever tools exist run; the rest OSError and are ignored.
        for cmd in (
            ["gtk-update-icon-cache", "-f", "-t", str(data / "icons" / "hicolor")],
            ["update-desktop-database", str(apps)],
            ["kbuildsycoca6"],
            ["kbuildsycoca5"],
        ):
            try:
                subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            except OSError:
                pass
    except Exception as e:  # noqa: BLE001 — never let branding break startup
        print(f"tt-station-panel: icon install skipped ({e})")


def main():
    install_desktop_icon()
    app = Gtk.Application(application_id=APP_ID)

    def on_activate(a):
        # Also let GTK resolve our icon by name for in-window use (display now exists).
        disp = Gdk.Display.get_default()
        if disp is not None:
            Gtk.IconTheme.get_for_display(disp).add_search_path(str(ASSETS / "icons"))
        panel = Panel(a)
        panel.present()
        # TTS_AUTOSTART=1 → bring the agent up immediately (handy for kiosk/demo).
        if os.environ.get("TTS_AUTOSTART") == "1":
            panel.start_agent()

    app.connect("activate", on_activate)
    app.run(None)


if __name__ == "__main__":
    main()
