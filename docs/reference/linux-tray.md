# Linux tray (`tt-station-tray`) — reference

`tt-station-tray` is the Linux counterpart of the macOS menu-bar app. It puts an icon in the
system tray and opens a small GTK4 window. From either, you start, stop and connect to models
served by this machine and by paired QuietBoxes.

Status: built, unit-tested and smoke-checked headless. **It has never been run on a display.**
See "What is verified" below before relying on any behavior described here.

## Stations

A *station* is anything that can serve models. The tray treats two kinds the same way
(`Station` enum in `crates/tt-station-tray/src/station.rs`; the spec proposed a trait object,
and the enum replaced it because there are exactly two fixed kinds and it avoids an
`async-trait` dependency):

| Kind | Talks to | Pairing |
|------|----------|---------|
| `LocalStation` | This machine, through the official Tenstorrent CLI (`tt --json ...`) | None |
| `RemoteStation` | A QuietBox, through the `tt-station-agentd` HTTP API | One-time 6-digit code |

`LocalStation` refuses a `tt` binary that does not answer `--version` with `tt <semver>`, so a
stale or shadowing copy is reported and never run. `RemoteStation` uses the same endpoints as
`tt-station run|stop|status`.

Known stations are kept in a registry file in `$TT_CONFIG_DIR` (else `$XDG_CONFIG_HOME/tt-station`, else
`~/.config/tt-station`), guarded by a `flock` so two tray processes do not
overwrite each other. The registry holds no tokens.

## Finding and pairing boxes

Discovery combines mDNS (`_tenstorrent._tcp`) with hosts added by hand (probed through their
`/status`). Tailscale auto-discovery is planned and not built.

Pairing uses the same handshake and token store as the CLI. A box paired with `tt-station pair`
is already paired in the tray, and the reverse. The agent shows its code on the box (panel or
`tt-station console`); enter it in the tray's pairing dialog.

## Tray and window

* **Tray.** Built on `ksni` (StatusNotifierItem over D-Bus). KDE Plasma hosts StatusNotifierItem
  icons itself. XFCE needs a status-notifier or indicator panel plugin; the package name was not
  confirmed here (`xfce4-indicator-plugin` has an apt candidate on 24.04, but whether it hosts
  these icons was not checked). **GNOME does not host StatusNotifierItem icons by
  default**: install the AppIndicator extension (`gnome-shell-extension-appindicator`) and enable
  it. At start-up the tray checks whether a host owns `org.kde.StatusNotifierWatcher` and, if
  not, shows install hints for the detected distribution in the window.
* **Window.** Plain GTK4 with Stations, Models and Logs pages. libadwaita is not used: the
  development box has no `libadwaita-dev`, and plain GTK4 should also build against Ubuntu
  22.04's GTK 4.6 (not yet built there). A libadwaita restyle is a later swap.
* **Connect launchers.** Open WebUI (a local `docker run`, published on `127.0.0.1` only; a
  loopback model endpoint is rewritten to `host.docker.internal` for the container), opencode in
  a terminal, copy the `/v1` URL, open it in a browser. Missing docker, a terminal, `xdg-open`
  or `notify-send` is reported in the UI.
* **Single instance.** The GTK application id `com.tenstorrent.ttstation.tray` is unique on the
  session bus. A second launch activates the first.

Run modes: no argument (tray plus window host; used by the autostart entry), `--no-window` (tray
only), `--smoke`, `--version`. A `--no-window` instance does not take part in single-instance,
so the autostart entry uses the default mode.

## Run options

`POST /run` on the agent (and `AgentClient::run_with_options`) accepts an optional `options`
object. The shared type is `libttstation::run_options::RunOptions`; the agent, CLI client and
tray all validate through it.

| Field | Meaning |
|-------|---------|
| `device` | Device name for the serve (for example `p300x2`) |
| `port` | Port to serve on |
| `profile` | tt-model bundle profile |
| `offline` | Serve without network access |
| `workflow` | `server`, `benchmarks` or `evals` |
| `vllm_args` | Pass-through vLLM flags, allowlist only |

Unknown keys are rejected by name. Every string that reaches a command line is checked
character by character. vLLM flags are never free text. The allowlist is `max-model-len`,
`max-num-seqs`, `max-num-batched-tokens`, `block-size` (integers with ranges),
`enable-auto-tool-choice` (switch), and `tool-call-parser`, `reasoning-parser` (short
identifiers). See `VLLM_ALLOWLIST` in `crates/libttstation/src/run_options.rs`.

`GET /run/schema` (unauthed) returns a description of every option, generated from the same
tables that drive validation, so a UI can build its form from it. Only the `tt` backend applies
`options`; other backends ignore them.

## The `tt` backend

`tt-station-agentd --backend tt` (or `backend = "tt"` in a profile) serves through the official
`tt` CLI. Its steps, option restrictions and settings are in
`docs/reference/agentd-config.md` under "The `tt` backend". The mock box (`mock-box`) takes
matching flags so the tray and CLI can be tested without hardware.

## Running the headless check

```
tt-station-tray --smoke
```

It builds the window view models and the app wiring against a temporary registry, never
initialises GTK and never opens the session bus, so it works over SSH and in CI. It prints `ok`
and exits 0, or prints the failure and exits 1. Run it with no `DISPLAY` or
`WAYLAND_DISPLAY` set to confirm that.

## What is verified

Verified:
* Unit tests for the options allowlist, the `Station` implementations (against `mock-box` and a
  fake `tt`), the registry, pairing flow, menu model, window view models, SNI probe and
  launcher command lines.
* `tt-station-tray --smoke` runs headless.
* `cargo clippy --workspace --all-targets -- -D warnings` is clean.

Not verified:
* **Nothing has run on a display**: the tray icon, the window, default mode, `--no-window`, the
  GApplication single-instance logic, ksni menu updates, and the real runner for docker,
  terminal, clipboard, `notify-send` and Wake-on-LAN.
* **The `tt` backend has not run against live hardware.** The check was deferred because all
  four chips on the development box were held by another tenant.
* **Nothing has been tested on `franklinstower`**, the intended second machine.
* **A `.deb` has not been built or installed** from these packaging files.
* **Ubuntu 22.04 (jammy) has not been built.** The crate enables no `gtk4` `v4_*` feature, so
  it should compile against the base GTK 4.0 API. That is the only evidence for jammy, and it
  is an inference.

## Open decision: leasing

`LocalStation::run` does not take a gozer lease. On a shared box a tray-started model can
collide with another tenant's chips. The agent's `tt` backend does lease through gozer. The
owner has to decide whether the local station should lease too.

## Known gaps against the spec

* The Logs page fetches on demand; there is no live stream.
* The detail pane shows the model row and does not call `tt model info`.
* No presets.
* A 409 (chips busy) shows an error and has no Queue button.
* GNOME needs the AppIndicator extension; the window shows the hint.

## Packaging

Package `tt-station-tray` (amd64) installs `/usr/bin/tt-station-tray`, the launcher
`com.tenstorrent.ttstation.tray.desktop`, an autostart entry in `/etc/xdg/autostart/`, and the
panel's icons under the tray's own icon name. It depends on `tt-station (= same version)` and
`libgtk-4-1`, and recommends the GNOME AppIndicator extension.
