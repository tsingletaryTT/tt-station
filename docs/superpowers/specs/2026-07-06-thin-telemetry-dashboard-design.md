# Thin telemetry stream for dashboard ops (+ single shared subscription)

**Date:** 2026-07-06
**Status:** approved design (in-session, "Thin stream + share (full)" chosen), ready for plan
**Scope:** `crates/tt-station-agentd` (a lite `/telemetry` mode), `macos/TTStation`
(one shared, ref-counted telemetry subscription per box, requesting the lite stream).

---

## Problem

The macOS app's dashboard only needs a few numbers per device — temperature, power, aiclk
(and a max-temp for the popover chip). But the agent's `GET /telemetry` streams the **entire
verbatim `tt-smi -s` JSON** (smbus_telem, firmwares, limits, every telemetry field) **plus** an
additive `tt_toplike` enrichment: a `/proc` **process scan** and a **vLLM `/metrics` scrape**
every tick. That enrichment is what **tt-toplike** needs — the dashboard throws it away.

On top of that, the app opens **two** telemetry sockets: `DeviceStripView` (the control-room
window) and `BoxDetailView` (the menu-bar popover) each own their own `TelemetryService`. Two
views open ⇒ two full streams ⇒ double the per-tick box work.

(tt-toplike, launched from the workbench, opens its own separate full stream — that's fine and
out of scope; it genuinely needs the full data.)

## Goals

- A **lite telemetry mode** the dashboard uses: per-device temp/power/aiclk only, and the box
  **skips the process scan + vLLM scrape** for lite clients.
- **One shared telemetry subscription per box** in the app (ref-counted), so window + popover
  read a single socket instead of opening two.
- **Graceful fallback:** the lite frame is a *subset of the same `tt-smi`-shaped JSON* the app
  already decodes, so an older agent that ignores the lite request (sending the full frame)
  still works with zero app decode changes.
- Keep the veneer: the trimming/skip decision lives in the agent (Rust); the app just requests
  lite and renders.

## Non-goals

- tt-toplike is untouched — full `/telemetry` (with `tt_toplike` enrichment) stays exactly as
  is; tt-toplike keeps using it.
- No change to what the dashboard *shows* (still temp/power/aiclk); this is about how it's fed.
- Not preventing tt-toplike from being a separate stream (user is fine with that).

---

## Component 1 — agent: lite `/telemetry` mode

The route `GET /telemetry` gains an optional query param **`?view=lite`** (read via axum
`RawQuery`/`Query`; absent or any other value = today's full behavior). `telemetry_ws` passes a
`lite: bool` into `telemetry_stream`. In the per-tick loop (`routes.rs::telemetry_stream`):

- **Full (unchanged):** `collect_snapshot` (tt-smi) → `sampler.sample()` (process scan) +
  `scrape_vllm_metrics` (vLLM) → `enrich_frame(json, Some(&toplike))`. tt-toplike's path.
- **Lite:** `collect_snapshot` (tt-smi — still needed for device temps), then **skip** the
  process scan, the vLLM scrape, and the inference sampler entirely, and send a **trimmed**
  frame: `crate::telemetry::lite_frame(&json)` — parse the tt-smi JSON and re-emit only
  `{"device_info":[{"board_info":{"board_type":<v>},"telemetry":{"asic_temperature":<v>,
  "power":<v>,"aiclk":<v>}}, …]}`, preserving each field's value verbatim (values stay whatever
  tt-smi emitted — quoted strings or numbers; the app decoder already tolerates both).
  - This is the **same shape** the app's `TelemetrySnapshot.decode` reads today, just a subset
    — dropping smbus_telem/firmwares/limits and the `tt_toplike` key. No app decode change.
  - `ProcessSampler`/`InferenceSampler` are simply not constructed/ticked in lite mode → the
    expensive `/proc` readlinks and the per-tick HTTP scrape don't happen for lite clients.
- The error-frame path (transient tt-smi failure) is shared/unchanged.

`lite_frame` is a **pure** function (`&str` tt-smi JSON → trimmed `String`), unit-tested with
canned tt-smi JSON (incl. missing fields → omitted, malformed → best-effort/empty device_info),
mirroring `enrich_frame`'s test style. It must never panic and never fabricate values.

## Component 2 — app: one shared, ref-counted subscription

Move telemetry ownership from the two views onto **`BoxViewModel`** (the per-box `@Observable
@MainActor` state holder, same package as `TelemetryService`):

- `BoxViewModel` gains `public let telemetry = TelemetryService()` and a private subscriber
  count. `public func subscribeTelemetry()` — increments; on 0→1 calls
  `telemetry.start(host: record.host, ctrlPort: record.ctrlPort, lite: true)`.
  `public func unsubscribeTelemetry()` — decrements (floored at 0); on 1→0 calls
  `telemetry.stop()`. Idempotent-safe under SwiftUI's appear/disappear churn.
- `TelemetryService.start` gains a `lite: Bool = true` param: when true it requests
  `ws://<host>:<ctrlPort>/telemetry?view=lite`, else the plain path. The dashboard uses lite.
- **`DeviceStripView`** and **`BoxDetailView`** drop their own `@State TelemetryService`; each
  does `.onAppear { box.subscribeTelemetry() } .onDisappear { box.unsubscribeTelemetry() }` and
  reads `box.telemetry.snapshot` (device strip) / `box.telemetry.snapshot?.devices.compactMap
  (\.tempC).max()` (popover chip). Both views showing at once ⇒ count 2 ⇒ still one socket;
  close one ⇒ count 1 ⇒ socket stays; close both ⇒ 0 ⇒ socket stops.
  - The popover's current "only while serving" gate becomes a subscribe/unsubscribe on the
    serving transition (keep that behavior: the popover subscribes when it wants the chip,
    unsubscribes when it doesn't) — but it now shares the same socket the window uses, so if the
    window already has it open, the popover adds zero new box load.
- Box-switch: `BoxWorkspaceView`'s `.id(box.id)` already tears down per-box view state; with the
  service on the (per-box) `BoxViewModel`, the old box's views disappear (unsubscribe → its
  socket stops) and the new box's appear (subscribe → its socket). Bounded, one per visible box.

## Data flow

```
agent GET /telemetry?view=lite  → tt-smi only, NO proc scan / NO vLLM scrape
                                 → lite_frame() (trimmed, same tt-smi shape, subset)
app: BoxViewModel.telemetry (ONE per box, ref-counted subscribe/unsubscribe)
   → DeviceStripView + BoxDetailView both read box.telemetry.snapshot  (share the one socket)
tt-toplike (separate process) → GET /telemetry (full, with tt_toplike)  — unchanged
```

## Testing

- **Rust (TDD):** `lite_frame(tt_smi_json)` — trims to device_info[].board_info.board_type +
  telemetry.{asic_temperature,power,aiclk}; omits missing fields; empty/garbage → valid
  `{"device_info":[]}`; never includes `tt_toplike`/firmwares/limits. (The route/loop wiring +
  the skip-scan branch are owner-verified against the box; the pure trim is unit-tested.)
- **Swift (TDD):** `BoxViewModel` ref-count — subscribe 0→1 starts (with lite URL), a 2nd
  subscribe doesn't restart, unsubscribe to 0 stops, extra unsubscribe floors at 0 (no
  double-stop). Use a fake/injected `TelemetryService` seam or assert via a spy. `TelemetryService`
  lite-URL construction (`?view=lite`) is checked. Decode is unchanged (lite is a subset).
- **No-hardware:** mock-box already streams a canned frame; extend it to honor `?view=lite` by
  emitting the trimmed shape (or ignore the param — the app decodes either), so the shared-socket
  path is exercisable without the box.

## Versioning & docs

- App bump (0.6.x). `macos/README.md` + the `TT_TOPLIKE_STREAM.md`/telemetry docs: note the lite
  `?view=lite` mode (dashboard) vs the full stream (tt-toplike), and the single shared app socket.

## Risks / open questions

- **Coordination:** `routes.rs::telemetry_stream` is actively evolved by the box session
  (procscan/inference). Keep the lite branch minimal + additive; rebase/push promptly to avoid
  conflicts.
- **Value types in the trimmed frame:** preserve tt-smi's own value encoding verbatim (don't
  coerce string↔number) so the app decoder (which already handles both) is unaffected.
- **Popover "while serving" behavior:** preserved via subscribe/unsubscribe timing; the shared
  socket means the popover no longer adds a second stream when the window is already open.
- **Deploy dependency:** the lite mode only reduces box load once the box's agent is rebuilt;
  until then `?view=lite` is ignored and the app gets the full frame (still correct). The
  app-side single-socket share helps immediately regardless.
