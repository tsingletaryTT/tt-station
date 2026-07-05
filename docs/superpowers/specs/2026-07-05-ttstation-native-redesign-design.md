# TTStation 0.3 — native, hardware-aware control room

**Date:** 2026-07-05
**Status:** approved design, ready for implementation plan
**Scope:** `macos/TTStation` (Swift app + TTStationKit), plus small enrichments to
`crates/tt-station-agentd`, `crates/tt`, and `crates/mock-box`.

---

## Problem

The current macOS app (v0.2.0) is a functional but plain SwiftUI veneer over
`tt --json`. Three weaknesses drove this redesign:

1. **It doesn't feel native.** The window and popover are stacks of `.caption`
   `Text` and small controls with weak hierarchy and no brand identity.
2. **Model browsing ignores the hardware.** Models are listed alphabetically by
   family. The box reports only a loose `chips: "4xBH"` string, so the app can't
   surface the models that will actually run on *this* box first — even though
   every model already declares the device meshes it supports (`P300X2`, `T3K`,
   `GALAXY`, …).
3. **The box-connected tools are buried.** Terminal / tt-toplike / VS Code are a
   small trailing "Workbench" row; connecting a front-end (opencode / Open WebUI)
   fails with an instruction to go install something by hand.

## Goals

- A window-first experience that reads as a real, native macOS control room,
  with a fast menu-bar popover for glance + quick actions.
- **Hardware-aware model ranking:** models that run on the connected box's
  detected device mesh are surfaced first; incompatible models are still visible
  but clearly marked with the hardware they need.
- **First-class workbench:** Terminal (SSH), tt-toplike (live remote telemetry),
  and VS Code + the `Tenstorrent.tt-vscode-toolkit` extension — prominent,
  labeled, one click each.
- **Fast Connect:** opencode and Open WebUI come up quickly, installing missing
  dependencies as needed instead of erroring with manual instructions.
- **A live, "alive" feel:** per-device temperature / utilization streamed from
  the agent's `/telemetry` WebSocket, rendered inline.

## Non-goals

- No reimplementation of discovery / pairing / HTTP control in Swift. The veneer
  rule holds: **control logic lives in Rust**, the app shells out to `tt --json`.
  The single sanctioned exception is the read-only telemetry WebSocket (below).
- No changes to how models are actually served (`run.py` path is untouched).
- No App Intents / deep links (still deferred).

---

## Architecture

### The veneer rule, restated

All *control* (discover, pair, run, stop, status, endpoint, serving) continues to
go through `tt --json` via `TTClient`. The redesign adds exactly one new I/O path
in Swift — a **read-only** telemetry WebSocket mirror — and one new piece of data
sourced from Rust (the box's detected device mesh). Everything else is pure Swift
presentation logic, unit-tested.

### Data flow additions

```
agent /status  ──(device_mesh)──▶  tt --json status/discover  ──▶  BoxRecord.deviceMesh
agent /telemetry (ws, tt-smi -s) ──────────────────────────────▶  TelemetryService ──▶ TelemetrySnapshot
model_spec.json ──(devices per model)──▶ tt --json models ──▶ ModelInfo.devices
                                                              └─▶ ModelRanking.rankForHardware(models, deviceMesh)
```

---

## Component 1 — device mesh as data (Rust)

**Why:** ranking "will this run here?" needs the box's detected mesh
(`p300x2`), not the loose `chips` string. The mapping already exists inside
`RunPyBackend::resolve_tt_device` (`("p300c", 4) => "p300x2"`, `("p300c", 2) =>
"p300"`, `("p150"|"p150c", 4) => "p150x4"`, `("n300", 4) => "n300x4"`,
`("n300", 1) => "n300"`).

**Change:**
- Extract the `(board_type, count) -> mesh` mapping into a shared, pure function
  (e.g. `libttstation` or an agent module) — `detect_device_mesh(tt_smi_json) ->
  Option<String>` — reused by both the runpy backend and the status route. No
  behavior change to serving.
- The agent runs mesh detection **once at startup** (via the existing
  `tt-smi -s` seam) and stores the result on `AppState` (`Option<String>`).
- Add `device_mesh: Option<String>` to the `/status` response and to whatever
  the discover/status payload carries into `tt`. `None` when detection fails
  (mixed fleet, no `tt-smi`, etc.) — never fatal, ranking degrades gracefully.
- `crates/tt`: decode and expose `device_mesh` in `status` and `discover`
  `--json` output.
- `crates/mock-box`: emit a fixed `device_mesh` (e.g. `"p300x2"`) so the app's
  ranking is verifiable with no hardware.

**Tests (Rust):** `detect_device_mesh` unit tests over canned `tt-smi -s` JSON
(each board/count case + the mixed-fleet / empty → `None` cases); status route
includes the field; CLI decodes it.

---

## Component 2 — hardware-aware model ranking (Swift, pure)

**New pure logic in `ModelDefaults` (or a new `ModelRanking`):**

- `meshMatches(_ modelDevices: [String], boxMesh: String?) -> Bool` —
  case-insensitive membership (`model.devices` uses `P300X2`; box mesh is
  `p300x2`). `boxMesh == nil` → treat all as "unknown compatibility" (no split).
- `rankForHardware(_ models: [ModelInfo], boxMesh: String?) -> RankedModels`
  returning two tiers:
  - **Runs here** — models whose `devices` include `boxMesh`, sorted by the
    existing quality `score` (instruct + ~7–9B first), then family-grouped for
    display.
  - **Needs other hardware** — the rest, each annotated with the mesh(es) it
    needs, sorted after the compatible tier.
- `pickDefaultModel` becomes **compatible-first**: prefer the best-scoring model
  in the "runs here" tier; only fall back to the global best if that tier is
  empty (or `boxMesh` is unknown). Last-used still wins if it's compatible.

**Display:** the browser shows the "Runs here" tier expanded and prominent, with
a dimmed, collapsible "Needs other hardware" section below. Search filters across
both; family grouping stays *within* the compatible tier. Each row carries a
compatibility affordance: `✓ Runs on P300X2` vs `Needs T3K` (secondary color).

**Tests (Swift):** `meshMatches` case-insensitivity; `rankForHardware` tiering,
ordering, and the `boxMesh == nil` degrade case; `pickDefaultModel`
compatible-first + last-used-wins-if-compatible.

---

## Component 3 — live telemetry (Swift, the one new I/O path)

**`TelemetrySnapshot` (pure, tested):** decodes a verbatim `tt-smi -s` frame —
`device_info: [{ board_info: { board_type }, telemetry: { asic_temperature, … } }]` —
into `[DeviceReading]` (index, boardType, tempC, utilization if present). Tolerant
decode: unexpected shapes yield an empty/partial snapshot, never a throw that
kills the stream. This mirrors the exact shape tt-toplike's `JSONBackend` parses;
we must not require a reshape from the agent.

**`TelemetryService` (I/O, `@Observable @MainActor`):** opens
`ws://<host>:<ctrlPort>/telemetry` with `URLSessionWebSocketTask`, feeds each
text frame through `TelemetrySnapshot`, publishes the latest snapshot + a
connection state (`connecting / live / stalled / failed`), and auto-reconnects
with backoff. `start(host:ctrlPort:)` / `stop()`; started when a box's window
detail appears, stopped on disappear so we hold at most one socket per visible
box. Unauthed, read-only — no bearer token, no control.

**Display — `DeviceStripView`:** per-device compact gauges — temperature
(color-ramped teal → yellow → red across a sane range) and a utilization bar when
present — plus a small aggregate sparkline and `Open tt-toplike ↗` for the deep
view. Degrades to "telemetry unavailable" quietly if the socket never comes up.

**Tests (Swift):** `TelemetrySnapshot` decode over the canned frame + malformed
frames (missing keys, non-numeric temp, empty `device_info`). `TelemetryService`
itself is owner-verified against the live box (I/O, like `LaunchController`).

---

## Component 4 — native visual identity

- **Palette (editor variant, per global CLAUDE.md):** teal `#4FD1C5` accent on
  deep blue-black `#0F2A35`. A `TTTheme` with named colors; applied as the app
  `.tint` and on cards/badges — tastefully, over native system materials, not as
  a heavy reskin.
- **Type:** Berkeley Mono for machine strings only (endpoints, mesh, temps);
  system font everywhere else.
- **Cards:** each detail section is a `GroupBox`-style card with a title and clear
  padding, giving the window real hierarchy.
- **Status dots:** green = serving, amber = starting, gray = idle, red =
  unreachable — consistent across sidebar, popover, and header.
- **Menu-bar icon:** a proper TT template icon pulled from `tt-vscode-toolkit` /
  `tt-local-generator` (do not redraw — see memory note).

---

## Component 5 — workbench, elevated

`WorkbenchCardView` promotes the three tools to first-class, labeled buttons with
icon + one-line subtitle:

- **Terminal** — `ssh` into the box (unchanged builder).
- **tt-toplike** — remote telemetry against the box's control port (unchanged
  builder; still resolves IPv4 first).
- **VS Code + toolkit** — Remote-SSH into the box **and** best-effort
  `--install-extension Tenstorrent.tt-vscode-toolkit`. If the marketplace id
  isn't resolvable, fall back to installing a local packaged `.vsix` if one is
  found under `~/code/tt-vscode-toolkit`; otherwise proceed with Remote-SSH and
  surface a non-fatal note. Extension install is best-effort and never blocks the
  window from opening.

Per-action spinner + honest error (already present) are retained.

**Tests (Swift):** VS Code arg builder includes the `--install-extension` flag +
remote target ordering; `.vsix` fallback path selection is pure and tested. The
actual launch is owner-verified.

---

## Component 6 — fast Connect (install-as-needed)

The Connect actions provision missing dependencies instead of erroring:

- **opencode:** if the binary is absent, run `brew install sst/tap/opencode`
  (state: `installing`), then write the per-box `opencode.json` and open Terminal
  (unchanged).
- **Open WebUI:** if `uv`/`uvx` is absent, run `brew install uv`, then
  `uvx open-webui serve` (unchanged), reusing an already-healthy instance.
- **Provisioning state machine** per action:
  `checking → installing → starting → ready | failed`, surfaced on the button so
  the user always sees where it is (`Installing opencode…`, `Starting Open WebUI…`).
- **Homebrew** is the one thing we do **not** auto-install: if `brew` itself is
  missing, surface an honest, actionable message (link to brew.sh) and stop.
- All steps non-fatal and safe to re-invoke; a failed install leaves a clear
  error and the button returns to its idle label.

**Tests (Swift):** pure install-command builders (`brew install sst/tap/opencode`,
`brew install uv`) and the state-machine transitions. The actual `brew`/`uvx`
runs are owner-verified.

---

## Surface layout

**Menu-bar popover (fast):**
```
Tenstorrent Boxes                         ⟳
● qb2-lab.local   serving Qwen3-8B   61°C
  [ Run ]  [ Stop ]        [ Open window ]
Add host…                            Quit
```

**Window (control room):** `NavigationSplitView`
- Sidebar: boxes with status dot + chips.
- Detail (cards, top → bottom):
  1. Header — name · `4× Blackhole p300c` · `P300X2` badge · reachability.
  2. Device strip — per-device temp/util gauges · `Open tt-toplike ↗`.
  3. Model — ranked browser (Runs here / Needs other hardware) · Run/Stop ·
     starting/serving state · endpoint + copy.
  4. Connect — Open WebUI / opencode (install-as-needed) when serving.
  5. Workbench — Terminal · tt-toplike · VS Code + toolkit.
  6. Serving — all `/v1` endpoints (agent + external badge).

---

## File plan (Swift)

New focused units so nothing becomes a god-view:
- `TTStationKit/DeviceMesh.swift` — pure mesh-match helpers (or fold into `ModelDefaults`).
- `TTStationKit/ModelRanking.swift` — `rankForHardware`, tiers, compatibility labels.
- `TTStationKit/TelemetrySnapshot.swift` — pure `tt-smi -s` frame decode.
- `TTStationKit/TelemetryService.swift` — WS I/O, observable snapshot + state.
- `TTStationKit/TTTheme.swift` — palette + fonts.
- `AppShell/Sources/DeviceStripView.swift`, `ModelBrowserView.swift`,
  `WorkbenchCardView.swift`, `ConnectCardView.swift`, `BoxHeaderView.swift`,
  `ServingCardView.swift` — the cards, each a focused file.
- Refactor `BoxWorkspaceView` into a thin composition of the cards.

Changed:
- `Models.swift` — `BoxRecord.deviceMesh: String?`.
- `ModelDefaults.swift` — compatible-first default.
- `LaunchController.swift` — install-as-needed provisioning + VS Code toolkit install.
- `project.yml` — `MARKETING_VERSION: 0.3.0`.

## File plan (Rust)

- `tt-station-agentd`: shared `detect_device_mesh`, startup detection on
  `AppState`, `device_mesh` in `/status`.
- `tt`: decode + expose `device_mesh` in `status`/`discover` JSON.
- `mock-box`: emit `device_mesh` + a telemetry frame.

---

## Testing strategy

- **TDD** for every pure unit (Rust `detect_device_mesh`; Swift `meshMatches`,
  `rankForHardware`, `pickDefaultModel` compatible-first, `TelemetrySnapshot`
  decode, install-command builders, VS Code toolkit args).
- **No-hardware end-to-end** via `mock-box` (device_mesh + telemetry frame): the
  ranking split and the live strip both exercise-able without the QuietBox.
- **Live verification** against the connected QB2 (`qb2-lab.local:8765`):
  telemetry stream, ranking against the real `p300x2` mesh, workbench launchers,
  fast-Connect installs.
- **Manual GUI smoke** (owner) — the popover + window click-through per release.

## Versioning & docs

- Bump app to **0.3.0**.
- Update `macos/README.md` (new surfaces, telemetry, ranking, fast Connect) and
  `tt-station/CLAUDE.md` current-state map.

## Risks / open questions

- **Toolkit marketplace id:** `Tenstorrent.tt-vscode-toolkit` may not be on the
  public marketplace; the `.vsix` fallback + non-fatal handling covers this.
- **Telemetry field names beyond temp:** utilization field naming in `tt-smi -s`
  varies by version; the decoder treats util as optional and renders temp-only if
  absent.
- **Swift WS reconnect churn:** backoff + tearing down on window disappear keep
  this bounded to one socket per visible box.
```
