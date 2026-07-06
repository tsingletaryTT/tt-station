# Window Redesign (Persistent Action Bar + TIS-Focused Model List) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the control-room window an always-visible Run/Stop action bar and an elegant model list focused on tt-inference-server models, de-duping the scattered serving/endpoint displays.

**Architecture:** The "focus on tt-inference-server" decision is made in Rust (`libttstation::catalog::classify`) so the app stays a veneer. The Swift window gains a persistent `RunStopBar` pinned outside the scroll (the single owner of serving/endpoint display), and `ModelBrowserView` is polished with the primary list = the TIS `runs_here` tier and tt-forge/tt-metal demoted to the Experimental aside.

**Tech Stack:** Rust (serde, `libttstation`), Swift 5 / SwiftUI, XcodeGen, `cargo test` / `swift test`.

## Global Constraints

- **Veneer rule:** the tt-inference-server focus is decided in Rust `classify`, not the app. No new Swift network I/O.
- **tt-inference-server match:** case-insensitive; normalize by lowercasing and folding `_`→`-`, then compare against `tt-inference-server` (also accept a value that *contains* `inference-server`). "No software listed" counts as NOT tt-inference-server.
- **runs_here rule (new):** live `/models` entry → always runs_here; a catalog model `Supported` on the box mesh goes to runs_here ONLY if its on-mesh compatibility entry's `software` includes tt-inference-server, else it goes to `experimental`. `experimental` (Experimental-on-mesh) and `other_hardware` otherwise unchanged. Not-Supported-everywhere still omitted.
- **Action bar:** pinned in the window OUTSIDE the ScrollView; single owner of the serving/endpoint + Run/Stop/Cancel display. Reads only existing `BoxViewModel` state (`selectedModel, endpoint, status, starting, cancelling, canStopOrCancel, run(), stop(), cancelStart()`).
- **Don't touch** the menu-bar popover (`BoxDetailView`) Run/Stop, pairing, telemetry, workbench, catalog fetch/cache.
- **App version → 0.6.0** on completion.
- Pure logic (Rust classify) is TDD; SwiftUI views are owner-verified (xcodebuild BUILD SUCCEEDED + swift test green).

---

## Task 1: TIS-focus in `classify` (Rust, pure)

**Files:**
- Modify: `crates/libttstation/src/catalog.rs` (`classify` + a small `software` helper + tests)

**Interfaces:**
- Consumes: existing `CompatCatalog`, `HardwareCompat.software`, `CompatStatus`, `hw_to_mesh`, `classify` (from the catalog feature).
- Produces: `classify` with the new runs_here rule; `BoxCatalog`/`CatalogEntry` wire shape UNCHANGED.

- [ ] **Step 1: Write failing tests** (append to `catalog.rs` tests):

```rust
#[test]
fn classify_runs_here_requires_tt_inference_server() {
    let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
      {"id":"tis","display_name":"TIS","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
      {"id":"forgeonly","display_name":"ForgeOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-forge"]}]},
      {"id":"metalonly","display_name":"MetalOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-metal"]}]},
      {"id":"nosoftware","display_name":"NoSoftware","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
    ]}"#).unwrap();
    let bc = classify(Some(&cat), &[], Some("p300x2"), false);
    // Only the tt-inference-server model runs here.
    assert_eq!(bc.runs_here.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec!["tis"]);
    // Supported-on-mesh-but-not-TIS demote to experimental.
    let exp: Vec<String> = bc.experimental.iter().map(|e| e.id.clone()).collect();
    assert!(exp.contains(&"forgeonly".to_string()));
    assert!(exp.contains(&"metalonly".to_string()));
    assert!(exp.contains(&"nosoftware".to_string()));
    // None of them wrongly landed in other_hardware.
    assert!(bc.other_hardware.is_empty());
}

#[test]
fn classify_live_model_still_runs_here_regardless_of_software() {
    use crate::model::ModelInfo;
    let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
      {"id":"forgeonly","display_name":"ForgeOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-forge"]}]}
    ]}"#).unwrap();
    // The box's live /models reports it → it IS tt-inference-server-servable now.
    let live = vec![ModelInfo { name: "forgeonly".into(), devices: vec!["P300X2".into()] }];
    let bc = classify(Some(&cat), &live, Some("p300x2"), false);
    assert_eq!(bc.runs_here.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec!["forgeonly"]);
    assert!(bc.experimental.is_empty());
}

#[test]
fn software_is_tt_inference_server_matches_tolerantly() {
    assert!(software_is_tis(&["tt-inference-server".into()]));
    assert!(software_is_tis(&["TT-Inference-Server".into()]));
    assert!(software_is_tis(&["tt_inference_server".into()]));
    assert!(software_is_tis(&["tt-forge".into(), "inference-server".into()]));
    assert!(!software_is_tis(&["tt-forge".into()]));
    assert!(!software_is_tis(&[]));
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p libttstation catalog` (the new runs_here rule + `software_is_tis` don't exist yet).

- [ ] **Step 3: Implement.**
  - Add a pure helper:
    ```rust
    /// True if any entry in `software` names tt-inference-server (the engine
    /// the box actually serves via run.py). Tolerant: lowercased, `_`→`-`,
    /// matches "tt-inference-server" or any value containing "inference-server".
    pub fn software_is_tis(software: &[String]) -> bool {
        software.iter().any(|s| {
            let f = s.to_lowercase().replace('_', "-");
            f == "tt-inference-server" || f.contains("inference-server")
        })
    }
    ```
  - In `classify`, when deciding a catalog model's tier for a known `box_mesh`: it qualifies for **runs_here** only if it has an on-mesh (`hw_to_mesh(hardware)` compatible with `box_mesh` — reuse the existing `mesh_compatible`) compatibility entry that is `Supported` AND `software_is_tis(&entry.software)`. If it's `Supported` on-mesh but NO such entry is tt-inference-server, route it to **experimental** (it's "supported with the tools," not run-now). The existing live-match rule (a live `/models` key → runs_here, available_now) stays and takes precedence (live models are TIS by definition). Everything else (Experimental-on-mesh → experimental; other-mesh → other_hardware; not-supported → omit) unchanged.
  - Keep ordering deterministic as before.

- [ ] **Step 4: Run, expect PASS** — `cargo test -p libttstation` (all green; adjust any pre-existing classify test whose fixture used a Supported-on-mesh model with no `tt-inference-server` software and expected it in runs_here — such a fixture now correctly lands in experimental; update the assertion and note it).

- [ ] **Step 5: Commit**

```bash
git add crates/libttstation/src/catalog.rs
git commit -m "feat(lib): classify runs_here focuses on tt-inference-server models"
```

---

## Task 2: `RunStopBar` view (Swift, owner-verified)

**Files:**
- Create: `macos/TTStation/AppShell/Sources/RunStopBar.swift`

**Interfaces:**
- Consumes: `BoxViewModel` (`selectedModel`, `endpoint`, `status`, `starting`, `cancelling`, `canStopOrCancel`, `run()`, `stop()`, `cancelStart()`), `TTTheme`.
- Produces: `struct RunStopBar: View { @Bindable var box: BoxViewModel }` — a single compact row.

- [ ] **Step 1: Study** `BoxWorkspaceView.modelBody` (the current Run/Stop/Cancel HStack + "Serving \<model\>"/endpoint + starting/canceling text) and `TTTheme` (statusServing/statusColor, mono). The bar reproduces exactly that behavior, relocated.

- [ ] **Step 2: Implement `RunStopBar.swift`.** A single-row bar:
  - Leading: a status dot via `TTTheme.statusColor(isServing:isStarting:)` (serving if `box.endpoint != nil` or `box.status?.isServing == true`; starting if `box.starting`), then the model name — `box.endpoint?.model` if serving, else `box.selectedModel` ?? "No model selected" — in `.callout`/`.caption` medium weight.
  - Center/spacer.
  - Trailing actions: **Run** (`Label("Run", systemImage: "play.fill")`, `.borderedProminent`, `.disabled(box.selectedModel == nil || box.inFlight)`, `Task { await box.run() }`); then the `box.starting ? Cancel : Stop` button exactly as `modelBody` does today (`cancelStart()` / `stop()`, `.disabled(!box.canStopOrCancel)`, `role: .destructive`, `.bordered`). `if box.inFlight { ProgressView().scaleEffect(0.6) }`.
  - Below/inline (second line when relevant): when `box.cancelling` → "Canceling…"; else if `box.starting` → "Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)"; else if serving → the endpoint URL (`TTTheme.mono`, `lineLimit(1)`, `.truncationMode(.middle)`) + a copy button (same NSPasteboard copy as today).
  - Style: a top divider + subtle `.background(.regularMaterial)` (or `TTTheme` surface) so it reads as a pinned bar. `.controlSize(.small)` on the buttons. `.padding(10)`.
  - Purely reads `box`; no `@State` beyond nothing (or a tiny local for a copied-confirmation if desired — keep minimal).

- [ ] **Step 3: Build** (it compiles standalone against BoxViewModel):
```
cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -5
```
Expected: BUILD SUCCEEDED. `cd macos/TTStation && swift test` still green.

- [ ] **Step 4: Commit**

```bash
git add macos/TTStation/AppShell/Sources/RunStopBar.swift
git commit -m "feat(macos): RunStopBar — pinned action bar owning serving/endpoint display"
```

---

## Task 3: Pin the bar + strip Run/Stop/serving from the Model card (Swift)

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/TTStationApp.swift` (`WindowRootView` — pin RunStopBar outside the ScrollView)
- Modify: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift` (`modelBody` — remove Run/Stop + serving line; Model card becomes just the browser)

**Interfaces:**
- Consumes: `RunStopBar` (Task 2), `BoxWorkspaceView`, `AppModel.selectedBox`.

- [ ] **Step 1: Read** `WindowRootView` in `TTStationApp.swift` — currently `detail: { if let box { ScrollView { BoxWorkspaceView(box:).padding() } } else { ContentUnavailableView } }`.

- [ ] **Step 2: Pin the bar.** Change the detail branch so the bar sits below the scroll and is always visible when a box is selected AND paired:
```swift
} detail: {
    if let box = model.selectedBox {
        VStack(spacing: 0) {
            ScrollView { BoxWorkspaceView(box: box).padding() }
            if box.isPaired {
                Divider()
                RunStopBar(box: box).id(box.id)   // .id resets any per-box state on switch
            }
        }
    } else {
        ContentUnavailableView("Select a box", systemImage: "cpu")
    }
}
```
(Keep the existing `.frame(minWidth:minHeight:)`, `.task { scan }`, activation-policy `onAppear/onDisappear`.)

- [ ] **Step 3: Strip the Model card.** In `BoxWorkspaceView.modelBody`, REMOVE: the Run/Stop/Cancel `HStack`, the `box.cancelling`/`box.starting` progress text block, and the `if let ep = box.endpoint { "Serving \(ep.model)" + endpoint copy }` block. `modelBody` becomes just the `ModelBrowserView(...)` + its `.task { loadModels }`. (All that display now lives in `RunStopBar`.) Leave the `onOpenWorkbench` wiring intact.

- [ ] **Step 4: Build + test.**
```
cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -5
cd macos/TTStation && swift test
```
Expected: BUILD SUCCEEDED; tests green.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/TTStationApp.swift macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift
git commit -m "feat(macos): pin RunStopBar in the window; Model card is now just the browser"
```

---

## Task 4: Model list polish + TIS labeling (Swift, owner-verified)

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/ModelBrowserView.swift`

**Interfaces:**
- Consumes: `BoxCatalog` (runsHere now TIS-focused from Task 1), `TTTheme`.

- [ ] **Step 1: Primary tier header + caption.** Change the catalog-mode primary `tierHeader("Runs on this box")` to `"Models"` with a small secondary caption line `"tt-inference-server"` beneath it (so the demo reads "these are the models this box serves"). Keep the fallback-mode header as "Runs on this box" (no catalog → no TIS split to claim).

- [ ] **Step 2: Elegant row.** Refresh `runsHereRow` visual treatment: model `displayName` in `.callout`/`.caption` medium weight; a right-aligned size chip when `entry.size != nil` (small, `TTTheme.mono` in a subtle capsule, e.g. `8B`); the existing `availableNow` green "ready" dot; selected state = accent checkmark + a subtle selected-row background (`RoundedRectangle` fill `Color.accentColor.opacity(0.12)` when `isSelected`). More vertical padding (`.padding(.vertical, 4)`) for breathing room. Keep it a `Button` setting `box.selectedModel = runnableModelId(for:)` (unchanged selection logic).

- [ ] **Step 3: Keep the asides.** Experimental (now also holds the demoted tt-forge/tt-metal supported models from Task 1) and Needs-other-hardware stay as the existing collapsed, dimmed `goBeyondRow` sections with their workbench framing — no behavior change, they just now contain the TIS-demoted entries too. Verify the Experimental header copy still reads correctly for that mix (it already says "bring these up with the tools").

- [ ] **Step 4: Build + test.**
```
cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -5
cd macos/TTStation && swift test
```
Expected: BUILD SUCCEEDED; tests green.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/ModelBrowserView.swift
git commit -m "feat(macos): elegant TIS-focused Models list (size chip, selected state, caption)"
```

---

## Task 5: De-dup Serving card + version bump + docs

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/ServingCardView.swift` (avoid echoing the action bar's agent endpoint)
- Modify: `macos/TTStation/AppShell/project.yml` (`MARKETING_VERSION: 0.6.0`)
- Modify: `macos/README.md`

**Interfaces:**
- Consumes: `box.serving` (`[ServingEntry]`), `box.endpoint`.

- [ ] **Step 1: Read** `ServingCardView` — it lists all `/serving` entries with an `external` badge. The action bar now owns the agent's own current endpoint display.

- [ ] **Step 2: De-dup.** Pass the agent's current endpoint base URL into `ServingCardView` (or filter in `BoxWorkspaceView`) so the Serving card renders only entries that are NOT the exact agent endpoint the action bar already shows — i.e. show it only when there's an external/tt-studio entry (or an additional agent endpoint) beyond the bar's. If, after filtering, there are no entries, the card doesn't render (keep the existing "only render when non-empty" guard). Keep the `external` badge + copy affordance for the entries that do show. Minimal signature change: `ServingCardView(entries:excludingBaseURL:)` or filter the array at the call site in `BoxWorkspaceView` (call site filter is simplest — do that: `ServingCardView(entries: box.serving.filter { $0.baseURL != box.endpoint?.baseURL })`, and the card keeps its own empty-guard).

- [ ] **Step 3: Version bump** → `MARKETING_VERSION: 0.6.0` in `project.yml`.

- [ ] **Step 4: Docs.** In `macos/README.md`, update the browser bullet: the window now has a **persistent Run/Stop action bar** (always visible, owns the serving model + endpoint) and the **Models list is tt-inference-server-focused** (tt-forge/tt-metal supported models appear under Experimental). Bump the version mention to 0.6.0.

- [ ] **Step 5: Build + test + commit.**
```
cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -5
cd macos/TTStation && swift test
git add macos/TTStation/AppShell/Sources/ServingCardView.swift macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift macos/TTStation/AppShell/project.yml macos/README.md
git commit -m "feat(macos): de-dup serving display; window redesign v0.6.0 + docs"
```

---

## Self-review notes

- **Spec coverage:** Component 1 (TIS focus) → Task 1; Component 2 (action bar) → Tasks 2–3; Component 3 (list polish) → Task 4; Component 4 (de-dup: Model card stripped → Task 3; Serving card → Task 5). Version/docs → Task 5.
- **Type consistency:** `software_is_tis` (Task 1) is internal to catalog.rs; `RunStopBar(box:)` (Task 2) consumed in Task 3; `runnableModelId`/`runsHereRow` (Task 4) unchanged selection contract; `BoxViewModel` action API (`run/stop/cancelStart/canStopOrCancel`) reused verbatim.
- **TDD vs owner-verified:** Rust `classify`/`software_is_tis` (Task 1) is TDD; all SwiftUI (Tasks 2–5) is owner-verified via xcodebuild + swift test (matches repo convention).
- **Veneer preserved:** TIS focus in Rust; the app renders `runs_here` as given.
