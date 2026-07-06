# Thin Telemetry + Shared Subscription — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the macOS dashboard a lightweight `/telemetry?view=lite` stream (per-device temp/power/aiclk, no process-scan / vLLM-scrape) and make the app use one shared, ref-counted telemetry socket per box instead of two.

**Architecture:** The agent gains a lite mode: same route, `?view=lite` skips the `tt_toplike` enrichment (process scan + vLLM scrape) and sends a trimmed frame that is a *subset of the same `tt-smi`-shaped JSON* the app already decodes — so an older agent that ignores the param still works. The app moves telemetry ownership onto `BoxViewModel` as one ref-counted subscription shared by the window's device strip and the popover chip; it requests lite.

**Tech Stack:** Rust (axum agent, serde_json), Swift 5 / SwiftUI (`@Observable @MainActor`), XcodeGen, `cargo test` / `swift test`.

## Global Constraints

- **Veneer rule:** trimming/skip decisions live in the agent (Rust); the app requests lite and renders. No new Swift network logic beyond the existing telemetry WS.
- **Lite frame shape = subset of the full tt-smi shape:** `{"device_info":[{"board_info":{"board_type":<v>},"telemetry":{"asic_temperature":<v>,"power":<v>,"aiclk":<v>}}, …]}`, values preserved **verbatim** from tt-smi (string or number — the app decoder handles both). No `tt_toplike`/firmwares/limits/smbus_telem. An old agent ignoring `?view=lite` sends the full frame (superset) → app decodes unchanged.
- **Lite skips the expensive per-tick work:** no `ProcessSampler` scan, no `scrape_vllm_metrics`, no `InferenceSampler` for lite clients.
- **tt-toplike untouched:** full `/telemetry` (with `tt_toplike` enrichment) is exactly as today.
- **One shared subscription per box, ref-counted:** subscribe 0→1 starts (lite URL), extra subscribes don't restart, unsubscribe to 0 stops, extra unsubscribe floors at 0.
- **App version bump** on completion (0.6.x).
- Pure logic (agent `lite_frame`, app ref-count) is TDD; route wiring + SwiftUI are owner-verified.

---

## Task 1: Agent `lite_frame` (Rust, pure)

**Files:**
- Modify: `crates/tt-station-agentd/src/telemetry.rs` (add `lite_frame` + tests)

**Interfaces:**
- Produces: `pub fn lite_frame(tt_smi_json: &str) -> String` — parse a `tt-smi -s` JSON snapshot and re-emit a trimmed frame containing only, per device, `board_info.board_type` and `telemetry.{asic_temperature,power,aiclk}`; values copied verbatim; missing fields omitted; malformed/absent `device_info` → `{"device_info":[]}`. Never panics.

- [ ] **Step 1: Write failing tests** in `telemetry.rs` tests module:

```rust
#[test]
fn lite_frame_trims_to_dashboard_fields() {
    let full = r#"{"device_info":[{"smbus_telem":{"x":1},"board_info":{"board_type":"p300c","other":"drop"},"telemetry":{"asic_temperature":"61.4","power":"85.2","aiclk":"1000","voltage":"0.8","fan_speed":"30"},"firmwares":{"a":"b"},"limits":{"c":"d"}}]}"#;
    let out = lite_frame(full);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let dev = &v["device_info"][0];
    assert_eq!(dev["board_info"]["board_type"], "p300c");
    assert_eq!(dev["telemetry"]["asic_temperature"], "61.4");
    assert_eq!(dev["telemetry"]["power"], "85.2");
    assert_eq!(dev["telemetry"]["aiclk"], "1000");
    // Trimmed: dropped fields + keys are gone.
    assert!(dev["telemetry"].get("voltage").is_none());
    assert!(dev["telemetry"].get("fan_speed").is_none());
    assert!(dev["board_info"].get("other").is_none());
    assert!(dev.get("smbus_telem").is_none());
    assert!(dev.get("firmwares").is_none());
    assert!(dev.get("limits").is_none());
    assert!(v.get("tt_toplike").is_none());
}

#[test]
fn lite_frame_preserves_numeric_values_verbatim() {
    let full = r#"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":60,"power":85,"aiclk":1000}}]}"#;
    let v: serde_json::Value = serde_json::from_str(&lite_frame(full)).unwrap();
    assert_eq!(v["device_info"][0]["telemetry"]["asic_temperature"], 60);
}

#[test]
fn lite_frame_omits_missing_fields() {
    let full = r#"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#; // no telemetry
    let v: serde_json::Value = serde_json::from_str(&lite_frame(full)).unwrap();
    assert_eq!(v["device_info"][0]["board_info"]["board_type"], "p300c");
    // telemetry object present but empty (or absent) — assert no crash + no stray fields
    let dev = &v["device_info"][0];
    assert!(dev["telemetry"].get("power").is_none());
}

#[test]
fn lite_frame_garbage_yields_empty_device_info() {
    let v: serde_json::Value = serde_json::from_str(&lite_frame("not json")).unwrap();
    assert_eq!(v["device_info"].as_array().unwrap().len(), 0);
    let v2: serde_json::Value = serde_json::from_str(&lite_frame(r#"{"device_info":"nope"}"#)).unwrap();
    assert_eq!(v2["device_info"].as_array().unwrap().len(), 0);
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p tt-station-agentd telemetry` (lite_frame undefined).

- [ ] **Step 3: Implement `lite_frame`** using `serde_json::Value` (tolerant, like `enrich_frame`):
  - `serde_json::from_str::<Value>(tt_smi_json)` → on `Err`, return `r#"{"device_info":[]}"#.to_string()`.
  - Walk `value["device_info"].as_array()`; for each entry object, build a new object with `board_info.board_type` (if present) and a `telemetry` object copying only `asic_temperature`/`power`/`aiclk` keys that exist (verbatim `Value` clones). Omit keys/objects that aren't present.
  - Collect into `{"device_info": [ …trimmed… ]}` and `serde_json::to_string`. If `device_info` isn't an array, emit `{"device_info":[]}`.
  - Never `unwrap` on shapes; use `.get`/`.as_array`/`.as_object` with graceful fallbacks.

- [ ] **Step 4: Run, expect PASS** — `cargo test -p tt-station-agentd telemetry`. Then `cargo test -p tt-station-agentd` all green.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/telemetry.rs
git commit -m "feat(agent): lite_frame — trim tt-smi to dashboard fields (temp/power/aiclk)"
```

---

## Task 2: Agent route — honor `?view=lite` (owner-verified wiring)

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (`telemetry_ws` reads the query; `telemetry_stream` takes `lite: bool` and branches)

**Interfaces:**
- Consumes: `crate::telemetry::lite_frame` (Task 1), existing `collect_snapshot`, `enrich_frame`, `ProcessSampler`, `InferenceSampler`.
- Produces: `GET /telemetry?view=lite` streams trimmed frames with no process scan / vLLM scrape; plain `GET /telemetry` unchanged.

- [ ] **Step 1: Read** `routes.rs` around `telemetry_ws` (~1251) and `telemetry_stream` (~1271). Note the per-tick full path: `collect_snapshot` → `sampler.sample()` + `scrape_vllm_metrics` + `inference_sampler.tick` → `enrich_frame(&json, Some(&toplike))`, and the `.route("/telemetry", get(telemetry_ws))` registration (~1612).

- [ ] **Step 2: Parse the query in `telemetry_ws`.** Add `axum::extract::RawQuery(query): axum::extract::RawQuery` (import from `axum::extract`) as a handler arg; compute `let lite = query.as_deref().map(|q| q.split('&').any(|kv| kv == "view=lite")).unwrap_or(false);` and pass it: `ws.on_upgrade(move |socket| telemetry_stream(socket, state, lite))`. (RawQuery avoids adding a typed Query struct + serde derive for one flag.)

- [ ] **Step 3: Branch in `telemetry_stream`.** Change the signature to `telemetry_stream(mut socket, state, lite: bool)`. Only construct `ProcessSampler`/`InferenceSampler` when `!lite` (or construct lazily but never call them in lite). In the tick's `Ok(json)` arm:
  ```rust
  let frame = if lite {
      crate::telemetry::lite_frame(&json)
  } else {
      let mut toplike = sampler.sample();
      let status = state.status();
      let port = resolve_metrics_port(&state);
      let scrape_body = scrape_vllm_metrics(port).await;
      toplike.inference = inference_sampler.tick(&status, scrape_body.as_deref()).map(|e| vec![e]);
      crate::telemetry::enrich_frame(&json, Some(&toplike))
  };
  ```
  The `Err(err) => telemetry_error_frame(&err)` path is shared/unchanged. In lite mode the scan/scrape/inference code never runs.
  - To avoid an "unused sampler in lite" warning: guard construction, e.g. `let mut sampler = (!lite).then(ProcessSampler::new);` and `if let Some(s) = sampler.as_mut() { … }` — or keep them constructed but only used in the `else` branch and `#[allow]` if needed. Prefer conditional construction so lite truly does zero scan work.

- [ ] **Step 4: Build + test.** `cargo build -p tt-station-agentd && cargo test -p tt-station-agentd`. Then a manual smoke (note in report): run the agent (or mock-box if it grows the param in Task 5), connect `ws://…/telemetry?view=lite`, confirm frames have `device_info` with only the 3 telemetry fields and no `tt_toplike`.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/routes.rs
git commit -m "feat(agent): /telemetry?view=lite skips process scan + vLLM scrape, sends trimmed frame"
```

---

## Task 3: App `TelemetryService.start(lite:)` + `BoxViewModel` shared ref-counted subscription (Swift, TDD for ref-count)

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/TelemetryService.swift` (add `lite` param → `?view=lite`)
- Modify: `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift` (own the service + subscribe/unsubscribe)
- Test: `macos/TTStation/Tests/TTStationKitTests/BoxViewModelTests.swift`

**Interfaces:**
- Consumes: existing `TelemetryService` (`start(host:ctrlPort:)`, `stop()`, `snapshot`, `state`).
- Produces: `TelemetryService.start(host:ctrlPort:lite:)` (lite defaults true, appends `?view=lite`); `BoxViewModel.telemetry: TelemetryService`, `BoxViewModel.subscribeTelemetry()`, `BoxViewModel.unsubscribeTelemetry()` (ref-counted, start/stop on 0↔1).

- [ ] **Step 1: Write failing `BoxViewModel` ref-count tests.** These need to observe start/stop without a real socket — add a tiny seam: make `BoxViewModel.telemetry` injectable, or (simpler) test the ref-count via a subclass/spy. Recommended seam: give `TelemetryService` an overridable/observable `startCount`/`stopCount` OR inject via `BoxViewModel(..., telemetry:)`. Use whatever's least invasive; if `TelemetryService` is `final`, add an internal `var onStart: ((String,Int,Bool)->Void)?` / `var onStop: (()->Void)?` test hook it calls at the top of `start`/`stop` (no behavior change in prod where they're nil). Then:

```swift
func testTelemetrySubscribeStartsOnceRefCounted() {
    let box = makeBox()  // existing helper
    var starts = 0, stops = 0
    box.telemetry.onStart = { _,_,_ in starts += 1 }
    box.telemetry.onStop = { stops += 1 }
    box.subscribeTelemetry()          // 0->1: start
    box.subscribeTelemetry()          // 1->2: no new start
    XCTAssertEqual(starts, 1)
    box.unsubscribeTelemetry()        // 2->1: no stop
    XCTAssertEqual(stops, 0)
    box.unsubscribeTelemetry()        // 1->0: stop
    XCTAssertEqual(stops, 1)
    box.unsubscribeTelemetry()        // floor at 0: no extra stop
    XCTAssertEqual(stops, 1)
}

func testTelemetrySubscribeRequestsLite() {
    let box = makeBox()
    var liteSeen: Bool?
    box.telemetry.onStart = { _,_,lite in liteSeen = lite }
    box.subscribeTelemetry()
    XCTAssertEqual(liteSeen, true)
}
```

- [ ] **Step 2: Run, expect FAIL** — `swift test --filter BoxViewModelTests` (subscribeTelemetry / onStart undefined).

- [ ] **Step 3: Implement.**
  - `TelemetryService`: add `public var onStart: ((String, Int, Bool) -> Void)?` and `public var onStop: (() -> Void)?` (test hooks, nil in prod). `start(host:ctrlPort:lite: Bool = true)` calls `onStart?(host, ctrlPort, lite)` first, then builds the URL `ws://<canonicalHost>:<ctrlPort>/telemetry` + `lite ? "?view=lite" : ""`. `stop()` calls `onStop?()` first (before the existing cancel logic).
  - `BoxViewModel`: add `public let telemetry = TelemetryService()`, `private var telemetrySubscribers = 0`. `public func subscribeTelemetry() { telemetrySubscribers += 1; if telemetrySubscribers == 1 { telemetry.start(host: record.host, ctrlPort: record.ctrlPort, lite: true) } }`. `public func unsubscribeTelemetry() { guard telemetrySubscribers > 0 else { return }; telemetrySubscribers -= 1; if telemetrySubscribers == 0 { telemetry.stop() } }`.

- [ ] **Step 4: Run, expect PASS** — `swift test` all green (existing telemetry-decode tests unaffected; lite is a subset shape).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TelemetryService.swift macos/TTStation/Sources/TTStationKit/BoxViewModel.swift macos/TTStation/Tests/TTStationKitTests/BoxViewModelTests.swift
git commit -m "feat(macos): shared ref-counted telemetry subscription on BoxViewModel (requests lite)"
```

---

## Task 4: Rewire the two views to the shared subscription (Swift, owner-verified)

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/DeviceStripView.swift`
- Modify: `macos/TTStation/AppShell/Sources/BoxDetailView.swift`
- Modify: `macos/TTStation/AppShell/project.yml` (version bump)

**Interfaces:**
- Consumes: `BoxViewModel.subscribeTelemetry()/unsubscribeTelemetry()/telemetry` (Task 3).

- [ ] **Step 1: `DeviceStripView`.** Remove `@State private var telemetry = TelemetryService()`. Replace its reads `telemetry.snapshot` → `box.telemetry.snapshot`. Replace `.task { telemetry.start(...) } .onDisappear { telemetry.stop() }` with `.onAppear { box.subscribeTelemetry() } .onDisappear { box.unsubscribeTelemetry() }`.

- [ ] **Step 2: `BoxDetailView`.** Remove its `@State private var telemetry = TelemetryService()`. The max-temp chip read `telemetry.snapshot?.devices...` → `box.telemetry.snapshot?.devices...`. Its current `.task(id: box.endpoint?.baseURL) { if box.endpoint != nil { start } else { stop } } .onDisappear { stop }` becomes: subscribe/unsubscribe tracking the same "only while serving" intent —
  ```swift
  .task(id: box.endpoint?.baseURL) {
      if box.endpoint != nil { box.subscribeTelemetry() } else { box.unsubscribeTelemetry() }
  }
  .onDisappear { box.unsubscribeTelemetry() }
  ```
  CAUTION on balance: `.task(id:)` re-runs on every id change and cancels the prior; to keep subscribe/unsubscribe balanced, track a local `@State private var popoverSubscribed = false` and only subscribe/unsubscribe on actual transitions:
  ```swift
  .task(id: box.endpoint?.baseURL) {
      let want = box.endpoint != nil
      if want && !popoverSubscribed { popoverSubscribed = true; box.subscribeTelemetry() }
      else if !want && popoverSubscribed { popoverSubscribed = false; box.unsubscribeTelemetry() }
  }
  .onDisappear { if popoverSubscribed { popoverSubscribed = false; box.unsubscribeTelemetry() } }
  ```
  This guarantees exactly one net subscribe while the chip wants data, matched by one unsubscribe — no ref-count drift.

- [ ] **Step 2b: DeviceStripView balance.** `.onAppear`/`.onDisappear` are naturally balanced (one each per appearance). If concerned about double-onAppear, apply the same `@State private var subscribed` guard as the popover. Keep it simple: onAppear/onDisappear is fine for a card that appears once per box-view lifetime.

- [ ] **Step 3: Version bump** → `MARKETING_VERSION` to the next patch (0.6.2) in `project.yml`.

- [ ] **Step 4: Build + test.** `cd macos/TTStation/AppShell && xcodegen generate && xcodebuild … build` (BUILD SUCCEEDED); `cd macos/TTStation && swift test` green.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/DeviceStripView.swift macos/TTStation/AppShell/Sources/BoxDetailView.swift macos/TTStation/AppShell/project.yml
git commit -m "feat(macos): device strip + popover chip share the one box telemetry socket, vX.Y.Z"
```

---

## Task 5: mock-box honors `?view=lite` + docs

**Files:**
- Modify: `crates/mock-box/src/main.rs` (telemetry route honors `?view=lite`)
- Modify: `macos/README.md` (+ telemetry doc note)

- [ ] **Step 1: mock-box lite.** Read mock-box's `/telemetry` handler. Make it read the query (RawQuery, same as Task 2) and, when `view=lite`, send the trimmed canned frame (a `device_info` with just `board_info.board_type` + `telemetry.{asic_temperature,power,aiclk}`, no `tt_toplike`). When not lite, send today's fuller canned frame. (If mock-box's frame is already minimal, the param can be a no-op that just proves the app's lite request is accepted — but prefer emitting the trimmed shape so the no-hardware path matches the agent.)

- [ ] **Step 2: Build + smoke.** `cargo build -p mock-box`; connect `ws://127.0.0.1:<port>/telemetry?view=lite` and confirm the trimmed frame; the app's DeviceStripView still renders temps against it.

- [ ] **Step 3: Docs.** `macos/README.md` telemetry note: the dashboard uses `GET /telemetry?view=lite` (per-device temp/power/aiclk, no process scan / vLLM scrape); tt-toplike uses the full `GET /telemetry`; the app shares one socket per box across the window + popover. Note the box-redeploy dependency (lite reduces box load once the agent has it; older agents send the full frame, still decoded).

- [ ] **Step 4: Commit**

```bash
git add crates/mock-box/src/main.rs macos/README.md
git commit -m "feat(mock-box): honor /telemetry?view=lite; docs for thin dashboard stream"
```

---

## Self-review notes

- **Spec coverage:** Component 1 (agent lite) → Tasks 1–2; Component 2 (app shared subscription + lite request) → Tasks 3–4; no-hardware + docs → Task 5. Fallback-via-subset is inherent (lite frame = subset of the decoded shape) — no separate task needed.
- **Type consistency:** `lite_frame(&str)->String` (Task 1) used in Task 2; `TelemetryService.start(host:ctrlPort:lite:)` + `onStart/onStop` + `BoxViewModel.subscribeTelemetry/unsubscribeTelemetry/telemetry` (Task 3) consumed in Task 4. Lite frame shape matches the existing `TelemetrySnapshot.decode` fields (device_info[].board_info.board_type, telemetry.{asic_temperature,power,aiclk}) — decode unchanged.
- **TDD vs owner-verified:** `lite_frame` (Task 1) + BoxViewModel ref-count (Task 3) are TDD; route wiring (Task 2), SwiftUI rewire (Task 4), mock-box (Task 5) are owner-verified with focused smokes.
- **Coordination:** Tasks 1–2 touch `telemetry.rs`/`routes.rs` the box session also edits — keep the lite branch minimal/additive, push promptly.
