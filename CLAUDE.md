# tt-station — project CLAUDE.md

Plug-and-play Tenstorrent from a Mac: discover a QuietBox on the LAN, pair once,
`tt run <model>`, get one OpenAI-compatible `/v1`. **No llama.cpp** — usability rides
on the `/v1` that `tt-inference-server` (vLLM, via `run.py`) exposes.

Repo: github.com/tsingletaryTT/tt-station (private). Work happens on `main`.
This box (`tsingletaryTT-quietbox`) IS a real QuietBox: 4× Blackhole (`p300c`),
`tt-smi`, docker, `~/code/tt-inference-server`. Real serving has been proven here.

---

## ▶ PICK UP HERE — especially on the Mac

**The entire macOS app (`macos/TTStation`, now v0.2.0) was authored on a Linux box
with NO Swift toolchain — it is committed but has NEVER been compiled.** First thing
on the Mac: build + verify it, fix any compile slips, then it's ready.

```
cd macos/TTStation && swift test                      # TTStationKit unit tests (pure logic)
cd macos/TTStation/AppShell && xcodegen generate \
  && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Then click-through against the live box (see "Activate the live box" below):
discover → pair → (smart-default model pre-selected) → search/pick → **Run** (amber
"starting"→green "serving") → **Connect** buttons (Open WebUI / opencode) → the
"Serving" list.

**Written-blind spots to eyeball on the Mac** (all target macOS 14):
- opencode provider/model id split on `ttstation/<vendor>/<model>` (`OpenCodeLauncher`).
- `Label` button width in the MenuBarExtra popover; the plain-`TextField` search (I
  avoided `.searchable` in the popover for compatibility) look; `LazyVStack` pinned
  section headers (`ModelPickerView`).
- `LaunchController` (Process/osascript/NSWorkspace) is owner-verified, not unit-tested.

Per-feature reports (what each change did + assumptions) are in `.superpowers/sdd/*.md`
(gitignored, local to this box) — regenerate/ignore as needed; the source + tests are
the truth.

**tt-toplike is a SEPARATE repo, owner-managed.** The remote-QuietBox work
(`WsBackend`, `--remote HOST:PORT`, `--remote <name>` via `tt discover`, and the
`/remote` slash command that hot-swaps to a box's live telemetry) is on tt-toplike's
`inference-server-monitoring` branch, committed locally there — NOT pushed by this
session, NOT part of tt-station. Design: `~/code/tt-toplike/docs/REMOTE_QUIETBOX_DESIGN.md`.

---

## Current state (what's shipped on `main`)

**Agent (`crates/tt-station-agentd`)** — box-side daemon, default backend `runpy`:
- Serves via `tt-inference-server/run.py`, deferring device/image/impl/engine to it.
  On this board `run.py` can't auto-detect the device, so the agent does: **`--tt-device`
  auto-detected from `tt-smi`** (4× p300c → `p300x2`); **`--serving-image` must be pinned**
  (image↔run.py compat is a curated matrix; `--auto-image` picks newest-local but is
  unsafe/opt-in). Before each serve: **stop any stale container on the port** + **`tt-smi -r`
  board reset** (clears wedged mesh ethernet cores). Readiness is gated on **`/v1/models`
  actually listing the model** (not just `/health`) — no dead endpoints; served id comes
  from `/v1/models`. Health-poll ceiling ~40 min.
- Pairing: 6-digit code (TTL + `MAX_PAIR_ATTEMPTS` lockout), **tokens persisted**
  (`--token-store`, default `~/.config/tt-station/agentd-tokens.json`) so pairing survives
  restarts. Two-step `pair-init`/`pair-complete` for the app.
- Routes: `GET /status` (unauthed), `GET /models` (unauthed, **vLLM-servable only**),
  `POST /pair/init|complete`, `POST /run|stop`, `GET /endpoint`, `POST /reset` (authed),
  `GET /telemetry` (**WebSocket**, unauthed — streams `tt-smi -s` for remote tt-toplike),
  `GET /serving` (unauthed — every running `tt-inference-server` `/v1`, `source: agent|external`),
  `GET /config` (unauthed — redacted resolved config, no secrets).
- mDNS `_tenstorrent._tcp` status re-published on run/stop; graceful shutdown unregisters.
- **Config file + named profiles:** optionally reads `agentd.toml` (default
  `$TT_CONFIG_DIR/agentd.toml` or `~/.config/tt-station/agentd.toml`, override with
  `--config`) with named `[profile.*]` serving configs (e.g. `stable`/`bleeding`), selected via
  `--profile` (else `default_profile`, else the sole profile, else today's flag-only behavior).
  `--print-config` resolves and prints the redacted summary without binding the port. No config
  file at all = unchanged pre-feature behavior. Full schema/precedence/errors:
  `docs/reference/agentd-config.md`; copy-paste starter: `box-panel/agentd.example.toml`.

**CLI (`crates/tt`):** `discover` (`--host`/`--no-mdns`), `pair`/`pair-init`/`pair-complete`,
`run`, `stop`, `status` (unauthed), `endpoint`, `models`, `serving`, `reset`,
`config` (unauthed — active/available profiles + resolved backend + serving host/port, mirrors
`GET /config`; see `docs/reference/agentd-config.md`). Global `--json`.
Tokens in macOS Keychain / file store. Respects `TT_CONFIG_DIR`.

**Box panel (`box-panel/tt-station-panel.py`, GTK4):** the box's own screen — Start/Stop/
Restart/Reset the agent, **live 6-digit pairing code** (with TTL), status/endpoint,
**profile dropdown** (reads `agentd.toml`'s profile list, passes `--profile` on Start/Restart;
hidden when no config file exists). Config via `TTS_*` env (repo path, serving host/port,
`TTS_IMAGE`, `TTS_AUTOSTART`, `TTS_CONFIG` for the profile dropdown's TOML path).

**macOS app (`macos/TTStation`, v0.4.0 — native control room):** window-first veneer over
`tt --json` with a fast MenuBarExtra popover for glance + quick actions. The resizable window
is a card-based control room: **box header** with a detected **device-mesh badge** (`P300X2`);
a **live device strip** (per-device temp/power/aiclk streamed from the agent's `/telemetry`
WebSocket — the one read-only Swift I/O path); a **hardware-aware model browser** that ranks
models that run on this box's mesh first ("Runs on this box" vs a dimmed "Needs other
hardware"), with a compatible-first smart default; **fast Connect** (Open WebUI / opencode that
`brew install` missing deps as needed); and an elevated **workbench** (Terminal / tt-toplike /
VS Code with the `Tenstorrent.tt-vscode-toolkit` extension). TT brand theme (teal `#4FD1C5`).
The device mesh is sourced from Rust: the agent detects it once at startup and reports it in
`/status` + the mDNS TXT record (so `tt --json discover`/`status` carry `device_mesh`). See
`macos/README.md`.

**Keyless SSH on pairing (v0.4.0):** the workbench launchers SSH as **`ttuser`** (QuietBox 2
default, override via the `tt.sshUser` UserDefault / agent `--ssh-user`). The pair flow has an
opt-in toggle (default on) that, on a successful pair, installs this Mac's SSH **public** key on
the box as `ttuser` — the PIN handshake is the trust anchor. Flow: `tt ssh-authorize` (reads or
generates `~/.ssh/id_ed25519`, never transmits the private key) → authed `POST /ssh/authorize`
on the agent → appended to the run-user's `~/.ssh/authorized_keys` (validated public-key-only,
idempotent, tagged `ttstation:<host>:<date>`; `DELETE`/`tt ssh-authorize --revoke` removes it).
The SSH step is non-fatal to pairing. The `authkeys` module hardened against label
newline-injection + unanchored-revoke; mock-box serves `/ssh/authorize` against a temp file.

**mock-box (`crates/mock-box`):** dev fixture — mDNS advertise + `serve` faking the control
API + `/v1` (used by the CLI e2e, no hardware).

**Docs:** `docs/reference/tt-inference-server-docker.md` (the real run.py launch),
`docs/tt-studio-integration.md` (verdict: **no clean cache-share without modifying tt-studio**;
`/serving` makes tt-studio's models visible), `docs/superpowers/{specs,plans}` (PoC, macOS
menubar, connect launchers), `docs/superpowers/cleanup-analysis.md`.

---

## Activate the live box (:8765 agent, via the panel)

The panel launches `./target/release/tt-station-agentd`. To pick up the latest agent
routes/fixes: **hit Restart on the panel** (or relaunch it). Box-local config it uses:
`--backend runpy --tt-inference-repo ~/code/tt-inference-server --serving-host qb2-lab.local
--serving-port 8003 --serving-image ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0-80180b9-7678b70`
(device auto-detects; pass `TTS_IMAGE` to the panel). Telemetry/serving/status are unauthed;
`run`/`stop`/`endpoint`/`reset` need a pairing (code shows on the panel). Restarting the
agent is fine now — **persist-tokens** keeps the Mac paired across restarts.

## Run / test (Rust, on this box)
- `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings`.
- CLI e2e (no hardware): `cargo test -p tt --test e2e_mock -- --ignored`.
- Live remote-telemetry smoke: start the agent, `python3` WebSocket read of `ws://…/telemetry`.

## How this project is built
Subagent-driven: fresh implementer + independent reviewer per change, TDD, frequent
commits, honest reports. Blend sources (glean + repos + docs). The git history is the
detailed log; this file is the current-state map.

## Known follow-ups (not blocking)
- tt-studio: real cache-sharing needs tt-studio running + config changes (see the doc).
- Agent: wrap `advertise_status` mDNS send in `spawn_blocking` (async-hygiene nit).
- macOS: build-verify everything above; wire discovery-by-name into the app's `/remote`
  story if desired; App Intents / deep links (deferred in the connect spec).
