# tt-station on Linux: tray app and main window

Status: design, awaiting owner review. Branch `feat/linux-tray`, cut from `experiments/egpu`.
Date: 2026-10-05.

## Goal

Every Linux or Mac machine is a **station**. A station can host Tenstorrent hardware, use
another station's hardware, or both. The Linux app does what the Mac app does: it lists
stations, starts and stops models, and connects clients to the running `/v1`. It works in
GNOME, KDE, XFCE and similar desktops. franklinstower is the first test machine.

Success means a user adds a station once, pairs it with a 6-digit code, and from then on can
start, stop and connect to models from the tray, without thinking about the network.

## What the owner said

- Option 3 of the networking question, "tray is local-first and also controls remote
  stations", is the direction. Version 1 finds stations by mDNS and by manual entry.
  Tailscale-style auto-discovery and a relay service come later.
- Pairing is part of the same app flow, using the existing 6-digit pairing system.
- GNOME is the primary Linux target. The tray must work there.
- The tray does simple stop and start. Choosing happens in the main app, because the model
  list can be dozens of entries and a run can take many parameters.
- Remote run parameters go through an extended agent `POST /run` (option 1).

## Assumptions (mine, open to correction)

- The official `tt` CLI is the local control path. We call `tt --json ...` and do not
  reimplement serving.
- One tray implementation covers every desktop through StatusNotifierItem (SNI).
- The app is a thin client. Logic that the Mac app keeps in Swift lives in `libttstation`
  or in the tray core, and the UI holds none.

## Findings that shaped the design

- The Mac app is a thin layer over `tt-station --json` and the agent's HTTP and WebSocket
  routes. Pairing is already `pair-init` then `pair-complete`. Manual hosts already exist.
- `tt` 1.0.1 provides `serve`, `launch`, `model list|ps|stop|logs|curl` and a global
  `--json`. `tt --json model list` returns `name`, `source`, `type`, `hardware`, `engines`,
  `cached`, `installed`, `cache_size_bytes`. `tt --json model ps` returns `served`.
- `tt serve` takes `--device`, `--port`, `--profile`, `--offline`, `--force`, `--workflow`,
  `--dry-run`, `--detach`, and vLLM flags after `--`.
- The agent's `POST /run` accepts only `{model, force}` today
  (`crates/libttstation/src/agent_client.rs`). Its default backend is `run.py` from
  tt-inference-server, not `tt`.
- Plain GNOME does not draw SNI icons. Ubuntu enables the `ubuntu-appindicators` extension
  by default. Other GNOME distributions need "AppIndicator and KStatusNotifierItem Support".
- The existing GTK panel (`box-panel/`) is a window with a Python and `python3-gi`
  dependency. It stays as the box's own screen and is not the tray.
- franklinstower answers ping (192.168.4.38). As of 2026-10-05 it has no agent on `:8765`,
  and key-based SSH from this box is refused. Its distribution and hardware are unchecked.

## Architecture

### New crate: `crates/tt-station-tray`

One Rust binary, `tt-station-tray`. It owns the station registry, polling, pairing and
actions. It reuses `libttstation` (agent client, discovery, pairing, Wake-on-LAN, device
mesh).

- **Tray icon and menu:** `ksni` (SNI over D-Bus).
- **Main window:** GTK4 with libadwaita through `gtk4-rs`. It is native on GNOME and runs on
  KDE and XFCE.
- **Process model:** one process. The tray and the window share state. The window opens on
  demand and closing it leaves the tray running. A second launch focuses the running
  instance.

### The `Station` trait

The UI talks to stations through one trait, so it never branches on station type.

- `LocalStation` calls `tt --json model list|ps`, `tt serve`, `tt model stop|logs`.
- `RemoteStation` calls the agent: `/status`, `/models`, `/serving`, `/run`, `/stop`,
  `/logs/stream`, `/telemetry`, `/power`.

Operations: `status`, `models`, `serving`, `run(model, options)`, `stop`, `logs`,
`connect_info` (base URL and model for the Connect actions).

### Station registry

A persisted list of known stations: the local one, mDNS-discovered ones, and manual ones
(address or pairing link). It mirrors the Mac app's `HostRegistry`. Tokens go in the Secret
Service keyring where available and fall back to the CLI's file store. The registry also
stores the last run options for each model on each station.

### Tray menu (simple)

- One row per station: state dot, name, mesh, running model.
- Running model: **Stop**, **Restart** (repeats the last options), Connect submenu
  (Open WebUI, opencode, copy `/v1`).
- Idle station: **Run last model**.
- **Open TT Station...** opens the window.
- On GNOME with no SNI host on the session bus, the app shows a one-time notice with the
  install step for that distribution. It keeps working as a windowed app.

### Main window

1. **Stations.** Local, discovered and manual stations, with state, mesh and pairing status.
   **Add station**: enter an address (or pick a discovered one), receive the pairing code
   prompt, enter the 6-digit code. This uses the existing `pair-init` and `pair-complete`.
2. **Model browser.** Search, filters (kind, source, cached only), and the Mac app's
   three groups: runs on this station, experimental, needs other hardware. A detail pane
   shows `tt model info` data.
3. **Run sheet.** Basic fields first. An Advanced section holds `device`, `port`, `profile`,
   `offline`, `force`, `workflow` and vLLM arguments, built from `GET /run/schema` for remote
   stations and from `tt serve --help` metadata for the local one. A "show the exact command"
   preview maps to `tt serve --dry-run`. Named presets cover repeated configurations.
4. **Logs and telemetry.** Follows `/logs/stream` and `/telemetry` for remote stations and
   `tt model logs` for the local one.

## Agent changes (`crates/tt-station-agentd`)

### `POST /run` options

The request becomes `{model, force, options?}`. `options` is optional, so the Mac app and
existing clients are unaffected. Fields: `device`, `port`, `profile`, `offline`, `workflow`,
`vllm_args`.

The agent validates `options` against an allowlist. The route runs a container as the box
user, so raw argv is never accepted. `vllm_args` takes known flag names with typed values
(for example `--max-model-len <int>`). Unknown flags, wrong types and out-of-range values
return 400 with the field name. Nothing starts on a rejected request.

### `GET /run/schema`

Unauthed, like `/models`. Returns each allowed option with type, default, limits and a
one-line description. The run sheet builds its Advanced form from this, so the tray carries
no hardcoded flag list.

### `tt` backend

Add a `tt` backend that runs `tt serve ... --detach` and reads `tt --json model ps`. It is
the way `options` reach `tt serve`, and it follows the repo rule to delegate to the official
CLI. `runpy` stays as the default until the `tt` backend is proven on franklinstower, and
backend choice stays in `agentd.toml` profiles. The `tt` backend uses `tt model ps` as its
source of truth, which closes the known gap where `/serving` cannot see tt-model
containers. Chip leasing stays with the agent through gozer, so a contended Run still
returns the 409 message naming the holder.

The agent must find the official `tt` the way `tt-station local` does: it must accept only a
binary whose `--version` output is `tt <semver>`. Per the repo naming rule, nothing we ship
is named `tt`.

## Errors

- 409 (chips held): show the holder, offer Queue or Cancel.
- Unreachable station: grey "offline" state. Offer Wake-on-LAN when the MAC is known.
- Rejected option: shown on the field, before any container starts.
- `tt` missing or not `tt <semver>` on the local station: the Stations pane says so and
  offers to install it with `uv tool install tenstorrent`, as the Mac app's first run does.

## Testing

- `Station` trait, registry, pairing flow and run options against `mock-box`, with no
  hardware. `mock-box` gains `/run/schema` and `options` handling.
- Option validation: rejected flags, wrong types, range limits, injection strings. Each test
  is run once against a deliberately broken validator and must fail (red), then restored.
- The tray menu model (pure data, built from station state) is unit tested. Rendering is
  checked by hand.
- Wiring checks sit at the layer that can fail: a test asserts that `RemoteStation::run`
  puts `options` on the wire, and that the agent hands them to the backend, as well as
  testing the validator on its own.
- Live pass on franklinstower, covering discover or manual add, pair, browse, run with
  options, stop, restart and connect. Needs an agent installed there. See open items.

## Packaging

A third `.deb`, `tt-station-tray`, depending on `tt-station`, GTK4 and libadwaita. It ships
an autostart `.desktop` file and a hicolor icon. Workspace version stays unified through
`scripts/bump-version.sh`. `ci.yml` and `release.yml` gain the new package for noble and
jammy.

## Build order

1. Agent `options`, `/run/schema` and the `tt` backend, tested on this box under gozer.
2. Tray core: `Station` trait, registry, pairing.
3. Tray menu (SNI), then the libadwaita window.
4. GNOME setup check, then packaging.
5. Live pass on franklinstower.

## Out of scope for version 1

Tailscale or other overlay discovery. A rendezvous or relay service. A native GNOME Shell
extension (it could later talk to the tray process over D-Bus). Windows. Changes to the Mac
app beyond adopting `/run/schema` later.

## Open items

- **franklinstower access.** It needs `tt-station-agentd` installed and running, and a way
  for me to reach it (an authorized SSH key, or the owner runs the install commands).
  Its distribution, GNOME or KDE session, and TT hardware are unchecked.
- **Local pairing UX on the same machine.** Whether a station should pair with itself is
  undecided. The design assumes `LocalStation` needs no pairing.
- **Window toolkit risk.** `gtk4-rs` and libadwaita versions on Ubuntu 22.04 (jammy) may be
  older than the crates need. If so, jammy gets the tray menu only, or the window crate
  pins older bindings.
