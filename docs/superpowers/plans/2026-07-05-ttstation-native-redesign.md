# TTStation 0.3 Native Hardware-Aware Redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the TTStation macOS app into a native, hardware-aware control room that ranks models by the connected box's device mesh, streams live device telemetry, elevates the box-connected workbench, and brings opencode / Open WebUI up fast (installing deps as needed).

**Architecture:** Rust remains the source of truth for all control and for the box's detected device mesh (exposed via `tt --json`). Swift adds exactly one read-only I/O path — a telemetry WebSocket mirror — plus pure, unit-tested presentation logic (ranking, mesh matching, telemetry decode, install-command builders). SwiftUI views are refactored into focused per-card files composed by a thin workspace view.

**Tech Stack:** Rust (axum agent, clap CLI, mock-box), Swift 5 / SwiftUI (`@Observable`, `MenuBarExtra`, `NavigationSplitView`, `URLSessionWebSocketTask`), XcodeGen, `swift test`.

## Global Constraints

- **Veneer rule:** all *control* (discover/pair/run/stop/status/endpoint/serving) goes through `tt --json` via `TTClient`. The ONLY new Swift I/O path permitted is the read-only, unauthed telemetry WebSocket. No discovery/pairing/HTTP-control reimplemented in Swift.
- **App version:** bump `MARKETING_VERSION` to `0.3.0` in `macos/TTStation/AppShell/project.yml`.
- **Target OS:** macOS 14.
- **Device mesh vocabulary:** model `devices` use upper-case mesh labels (`P300X2`, `T3K`, `GALAXY`); the box's detected mesh is lower-case (`p300x2`). All matching is case-insensitive.
- **Mesh mapping (verbatim from `resolve_tt_device`):** `("p300c",4)→p300x2`, `("p300c",2)→p300`, `("p150"|"p150c",4)→p150x4`, `("n300",4)→n300x4`, `("n300",1)→n300`, else `None`.
- **Telemetry frame contract:** a frame is verbatim `tt-smi -s` stdout — `{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"61.4"}}]}`. Do not require the agent to reshape it.
- **Brand palette (editor variant):** teal `#4FD1C5` accent on deep blue-black `#0F2A35`; Berkeley Mono for machine strings only.
- **VS Code toolkit id:** `Tenstorrent.tt-vscode-toolkit`.
- **Homebrew** is never auto-installed; every other Connect dependency is.
- Pure logic is TDD (test first, watch it fail, implement, watch it pass, commit). SwiftUI views + process/socket I/O are owner-verified (not unit-tested), matching the existing repo convention.

---

## Task 1: Shared `detect_device_mesh` (Rust, pure)

**Files:**
- Create: `crates/tt-station-agentd/src/device.rs`
- Modify: `crates/tt-station-agentd/src/lib.rs` (add `pub mod device;`) — verify the module list location first.
- Modify: `crates/tt-station-agentd/src/serving/runpy.rs:344-391` (`resolve_tt_device` calls the shared fn)

**Interfaces:**
- Produces: `pub fn detect_device_mesh(tt_smi_json: &str) -> Option<String>` — parses a `tt-smi -s` JSON snapshot and returns the lower-case mesh label, or `None` for empty/mixed/unknown fleets.

- [ ] **Step 1: Write the failing tests** in `crates/tt-station-agentd/src/device.rs`:

```rust
//! Pure mapping from a `tt-smi -s` snapshot to this box's device-mesh label.
//!
//! The single source of truth for `(board_type, count) -> mesh`. Both the
//! runpy backend (choosing `--tt-device`) and the `/status` route (reporting
//! the box's mesh so clients can rank models by hardware fit) call this, so the
//! table lives in exactly one place.

use serde_json::Value;

/// Map a verbatim `tt-smi -s` JSON snapshot to a device-mesh label
/// (`"p300x2"`, `"p150x4"`, …). Returns `None` when `device_info` is empty,
/// the fleet is mixed (boards of differing `board_type`), or the
/// (type, count) pair isn't a known mesh.
pub fn detect_device_mesh(tt_smi_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(tt_smi_json).ok()?;
    let board_types: Vec<String> = value
        .get("device_info")?
        .as_array()?
        .iter()
        .filter_map(|d| {
            Some(
                d.get("board_info")?
                    .get("board_type")?
                    .as_str()?
                    .to_lowercase(),
            )
        })
        .collect();
    let count = board_types.len();
    if count == 0 || !board_types.windows(2).all(|p| p[0] == p[1]) {
        return None;
    }
    let mesh = match (board_types[0].as_str(), count) {
        ("p300c", 4) => "p300x2",
        ("p300c", 2) => "p300",
        ("p150" | "p150c", 4) => "p150x4",
        ("n300", 4) => "n300x4",
        ("n300", 1) => "n300",
        _ => return None,
    };
    Some(mesh.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_four_p300c_to_p300x2() {
        let json = r#"{"device_info":[
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"p300c"}},
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"p300c"}}]}"#;
        assert_eq!(detect_device_mesh(json).as_deref(), Some("p300x2"));
    }

    #[test]
    fn maps_single_n300() {
        let json = r#"{"device_info":[{"board_info":{"board_type":"n300"}}]}"#;
        assert_eq!(detect_device_mesh(json).as_deref(), Some("n300"));
    }

    #[test]
    fn mixed_fleet_is_none() {
        let json = r#"{"device_info":[
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"n300"}}]}"#;
        assert_eq!(detect_device_mesh(json), None);
    }

    #[test]
    fn empty_device_info_is_none() {
        assert_eq!(detect_device_mesh(r#"{"device_info":[]}"#), None);
    }

    #[test]
    fn unknown_count_is_none() {
        let json = r#"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#;
        assert_eq!(detect_device_mesh(json), None); // 1x p300c is not a known mesh
    }

    #[test]
    fn garbage_json_is_none() {
        assert_eq!(detect_device_mesh("not json"), None);
    }
}
```

- [ ] **Step 2: Register the module.** Add `pub mod device;` to `crates/tt-station-agentd/src/lib.rs` beside the other `pub mod` lines.

- [ ] **Step 3: Run the tests, expect PASS**

Run: `cargo test -p tt-station-agentd device::tests`
Expected: 6 tests pass.

- [ ] **Step 4: Refactor `resolve_tt_device` to use the shared fn.** In `crates/tt-station-agentd/src/serving/runpy.rs`, replace the inline `(board_type, count)` parsing/match in `resolve_tt_device` (lines ~352-390) with a call to `crate::device::detect_device_mesh(&stdout)`, keeping the existing `eprintln!` logging of the outcome. Leave the tt-smi invocation itself untouched.

- [ ] **Step 5: Run the backend tests, expect PASS**

Run: `cargo test -p tt-station-agentd`
Expected: all pass (existing `resolve_tt_device` tests still green).

- [ ] **Step 6: Commit**

```bash
git add crates/tt-station-agentd/src/device.rs crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/src/serving/runpy.rs
git commit -m "feat(agent): shared detect_device_mesh; runpy reuses it"
```

---

## Task 2: Agent exposes `device_mesh` in `/status`

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (AppState field + `StatusJson` struct near line 811 + `status` handler)
- Modify: `crates/tt-station-agentd/src/main.rs` (detect mesh at startup, set on state)

**Interfaces:**
- Consumes: `crate::device::detect_device_mesh` (Task 1).
- Produces: `/status` JSON gains `"device_mesh": <string|null>`; `AppState::with_device_mesh(Option<String>)` builder; `AppState::device_mesh() -> Option<&str>`.

- [ ] **Step 1: Read the current status handler + AppState.** In `crates/tt-station-agentd/src/routes.rs`, locate the `StatusJson` (or equivalently-named) struct around line 811 that carries `chips: String`, and the `AppState`/`AppStateInner` definition near lines 198-240. Note the existing `with_*` builder pattern (`with_telemetry_config`, `with_serving_config`) that relies on `Arc::get_mut`.

- [ ] **Step 2: Add the field + builder + accessor.** Add `device_mesh: Option<String>` to `AppStateInner` (default `None` in the constructors), a `with_device_mesh(self, mesh: Option<String>) -> Self` builder mirroring `with_serving_config`, and `pub fn device_mesh(&self) -> Option<&str> { self.inner.device_mesh.as_deref() }`.

- [ ] **Step 3: Add `device_mesh` to the status response.** Add `device_mesh: Option<String>` to the status response struct and populate it from `state.device_mesh().map(str::to_string)` in the `status` handler.

- [ ] **Step 4: Add a status-route test.** In the routes test module, extend (or add) a test that hits `/status` on a state built `.with_device_mesh(Some("p300x2".into()))` and asserts the JSON body contains `"device_mesh":"p300x2"`; and one on a state without it asserting `"device_mesh":null`.

Run: `cargo test -p tt-station-agentd`
Expected: PASS.

- [ ] **Step 5: Detect at startup in `main.rs`.** After the `with_serving_config` line (~main.rs where state builders are chained), run `<tt_smi_bin> -s` once via the same `RealCommandRunner` used by telemetry, pass its stdout to `crate::device::detect_device_mesh`, and chain `.with_device_mesh(mesh)`. On any tt-smi failure, pass `None` and `eprintln!` a note — never fatal. Use `cli.tt_smi_bin`.

- [ ] **Step 6: Build the whole agent, expect success**

Run: `cargo build -p tt-station-agentd`
Expected: compiles clean.

- [ ] **Step 7: Commit**

```bash
git add crates/tt-station-agentd/src/routes.rs crates/tt-station-agentd/src/main.rs
git commit -m "feat(agent): report detected device_mesh in /status"
```

---

## Task 3: CLI `tt` exposes `device_mesh` in status/discover JSON

**Files:**
- Modify: `crates/tt/src/main.rs` (status ~line 336-355 + discover ~line 598; the box record struct)

**Interfaces:**
- Consumes: agent `/status` `device_mesh` (Task 2).
- Produces: `tt --json status` and `tt --json discover` records carry `"device_mesh": <string|null>`.

- [ ] **Step 1: Read the CLI's status + discover JSON shaping.** In `crates/tt/src/main.rs`, find the status response struct (~336) and the discover record shaping (~598) and the `chips` plumbing.

- [ ] **Step 2: Thread the field through.** Add `device_mesh: Option<String>` to the CLI's status/box-record serialization structs, decode it from the agent `/status` payload, and include it in both the `status` and `discover` `--json` output. Where discover synthesizes records, populate `device_mesh` from the per-box status probe if available, else `None`.

- [ ] **Step 3: Update the mock fixture default.** At `crates/tt/src/main.rs:815` (the `chips: "4xBH".into()` test/mock record) add `device_mesh: Some("p300x2".into())` (or `None` if that record is a pure fixture — match its intent).

- [ ] **Step 4: Build + run CLI tests**

Run: `cargo test -p tt && cargo build -p tt`
Expected: PASS + compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/main.rs
git commit -m "feat(cli): surface device_mesh in status/discover --json"
```

---

## Task 3.5: Advertise `device_mesh` in mDNS TXT (inserted post-Task-3)

**Why (discovered during Task 3):** Task 2 added `device_mesh` only to the HTTP `/status` body, so `tt --json discover` populates it *only* for manually-probed hosts. mDNS-discovered boxes (the live QB2, the common case) get `device_mesh: null`, which blunts the app's hardware-aware ranking for the primary use case. Fix: make `device_mesh` a uniform property of a discovered box by carrying it in the mDNS TXT record, exactly like `chips`.

**Files:**
- Modify: `crates/libttstation/src/model.rs` (`txt_encode` ~line 172, `txt_decode` ~line 182, tests)
- Modify: `crates/tt-station-agentd/src/main.rs` (`advertise` ~749 + `MdnsStatusAdvertiser` ~668-695 thread the detected mesh in)
- Modify: `crates/mock-box/src/main.rs` (`advertise` ~108 emits a fixed `Some("p300x2")` for dev parity)

**Interfaces:**
- Consumes: agent startup `device_mesh` (Task 2), CLI decode (Task 3).
- Produces: `txt_encode` emits a `device_mesh` TXT pair when `rec.device_mesh` is `Some`; `txt_decode` parses the optional `device_mesh` key back into `BoxRecord.device_mesh`.

- [ ] **Step 1: TDD `txt_encode`/`txt_decode` round-trip.** Add a test asserting a `BoxRecord` with `device_mesh: Some("p300x2")` encodes a `("device_mesh","p300x2")` TXT pair, and `txt_decode` of a TXT map containing `device_mesh=p300x2` yields `device_mesh: Some("p300x2")`; and that a TXT map WITHOUT the key still yields `None` (back-compat). Update the existing `txt_decode_builds_boxrecord` test's `assert_eq!(rec.device_mesh, None)` to a separate map that omits the key (keep a no-key case).

- [ ] **Step 2: Implement.** In `txt_encode`, after the `status` pair, conditionally push `("device_mesh".to_string(), mesh.clone())` when `rec.device_mesh` is `Some`. In `txt_decode`, read `map.get("device_mesh").cloned()` into `device_mesh` (replacing the hardcoded `None`). Update the now-stale doc comments that say TXT never carries the field.

- [ ] **Step 3: Run codec tests, expect PASS.**

Run: `cargo test -p libttstation`
Expected: PASS.

- [ ] **Step 4: Thread the mesh into the agent advertisers.** In `crates/tt-station-agentd/src/main.rs`: `advertise(cli, status)` is called at startup where the detected `device_mesh` is in scope — pass it in (add a `device_mesh: Option<String>` parameter to `advertise`) and set it on the `BoxRecord` instead of `None`. `MdnsStatusAdvertiser` (re-publishes on run/stop) must hold the `device_mesh` so its `advertise_status` sets it too — add a `device_mesh: Option<String>` field to the advertiser struct, populated at construction from the detected mesh. Remove the "txt_encode doesn't read device_mesh" comments.

- [ ] **Step 5: mock-box dev parity.** In `crates/mock-box/src/main.rs` `advertise`, set the advertised `BoxRecord.device_mesh` to `Some("p300x2".to_string())` (matching what Task 4 puts in its `/status`) so the no-hardware mDNS discovery path shows a ranked model list. Update the comment.

- [ ] **Step 6: Build all three crates + full test, expect PASS.**

Run: `cargo test -p libttstation -p tt-station-agentd && cargo build -p tt-station-agentd -p mock-box -p tt`
Expected: PASS + clean build.

- [ ] **Step 7: Commit**

```bash
git add crates/libttstation/src/model.rs crates/tt-station-agentd/src/main.rs crates/mock-box/src/main.rs
git commit -m "feat(mdns): advertise device_mesh in TXT so discover carries it"
```

---

## Task 4: mock-box emits `device_mesh` + a telemetry frame

**Files:**
- Modify: `crates/mock-box/src/*` (the status handler; add/verify a `/telemetry` WS that emits a canned frame)

**Interfaces:**
- Produces: `mock-box serve` `/status` includes `"device_mesh":"p300x2"`; `/telemetry` streams a canned `tt-smi -s` frame on an interval so the app's live strip + ranking are exercisable with no hardware.

- [ ] **Step 1: Read mock-box's routes.** `grep -rn "status\|telemetry\|device_info\|chips" crates/mock-box/src` to see what it already fakes.

- [ ] **Step 2: Add `device_mesh` to mock status.** Add `"device_mesh":"p300x2"` to the mock `/status` JSON so it matches the real agent contract from Task 2.

- [ ] **Step 3: Add/confirm a canned telemetry WS.** Ensure `GET /telemetry` upgrades to a WebSocket and sends, every ~1s, a canned frame with 4 boards to match `p300x2`:

```json
{"device_info":[
 {"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"61.4"}},
 {"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"58.0"}},
 {"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"60.2"}},
 {"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"55.7"}}]}
```

(If mock-box has no WS support yet, keep this task's telemetry piece minimal — a route that accepts the upgrade and pushes the canned frame — reusing whatever WS lib the agent uses.)

- [ ] **Step 4: Build + smoke**

Run: `cargo build -p mock-box && cargo test -p tt --test e2e_mock -- --ignored`
Expected: builds; the existing e2e still passes (device_mesh is additive, decoders ignore unknowns).

- [ ] **Step 5: Commit**

```bash
git add crates/mock-box/src
git commit -m "feat(mock-box): emit device_mesh + canned telemetry frame"
```

---

## Task 5: `BoxRecord.deviceMesh` (Swift decode)

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/Models.swift:3-36` (`BoxRecord`)
- Modify: `macos/TTStation/Tests/TTStationKitTests/Fixtures/discover.json` (add `device_mesh`)
- Test: `macos/TTStation/Tests/TTStationKitTests/ModelsTests.swift`

**Interfaces:**
- Consumes: `tt --json discover/status` `device_mesh` (Task 3).
- Produces: `BoxRecord.deviceMesh: String?` (CodingKey `device_mesh`), included in the public `init`.

- [ ] **Step 1: Write the failing test** in `ModelsTests.swift`:

```swift
func testBoxRecordDecodesDeviceMesh() throws {
    let json = #"{"name":"qb2","host":"qb2-lab.local","ctrl_port":8765,"chips":"4xBH","status":"idle","apiver":1,"device_mesh":"p300x2"}"#
    let rec = try JSONDecoder().decode(BoxRecord.self, from: Data(json.utf8))
    XCTAssertEqual(rec.deviceMesh, "p300x2")
}

func testBoxRecordDeviceMeshDefaultsNilWhenAbsent() throws {
    let json = #"{"name":"qb2","host":"qb2-lab.local","ctrl_port":8765,"chips":"4xBH","status":"idle","apiver":1}"#
    let rec = try JSONDecoder().decode(BoxRecord.self, from: Data(json.utf8))
    XCTAssertNil(rec.deviceMesh)
}
```

- [ ] **Step 2: Run, expect FAIL** (`deviceMesh` doesn't exist).

Run: `cd macos/TTStation && swift test --filter ModelsTests`
Expected: compile error / fail.

- [ ] **Step 3: Add the field.** In `BoxRecord`: add `public let deviceMesh: String?`, add `case deviceMesh = "device_mesh"` to `CodingKeys`, and add `deviceMesh: String? = nil` as the final parameter of the public `init` (assigning `self.deviceMesh = deviceMesh`). Making it optional-with-default keeps every existing `BoxRecord(...)` call site compiling.

- [ ] **Step 4: Update the discover fixture.** Add `"device_mesh":"p300x2"` to one record in `Fixtures/discover.json`.

- [ ] **Step 5: Run, expect PASS**

Run: `cd macos/TTStation && swift test --filter ModelsTests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/Models.swift macos/TTStation/Tests/TTStationKitTests/ModelsTests.swift macos/TTStation/Tests/TTStationKitTests/Fixtures/discover.json
git commit -m "feat(macos): BoxRecord.deviceMesh from tt --json"
```

---

## Task 6: Hardware-aware ranking (Swift, pure)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/ModelRanking.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/ModelRankingTests.swift`

**Interfaces:**
- Consumes: `ModelInfo` (`name`, `devices`), `ModelDefaults.score`/`groupModelsByFamily`.
- Produces:
  - `ModelRanking.meshMatches(_ devices: [String], boxMesh: String?) -> Bool`
  - `struct RankedModels { let compatible: [(family: String, models: [ModelInfo])]; let incompatible: [ModelInfo] }`
  - `ModelRanking.rankForHardware(_ models: [ModelInfo], boxMesh: String?) -> RankedModels`
  - `ModelRanking.compatibilityLabel(for model: ModelInfo, boxMesh: String?) -> String` (`"Runs on P300X2"` / `"Needs T3K"` / `""` when boxMesh nil)

- [ ] **Step 1: Write the failing tests** in `ModelRankingTests.swift`:

```swift
import XCTest
@testable import TTStationKit

final class ModelRankingTests: XCTestCase {
    private func m(_ name: String, _ devices: [String]) -> ModelInfo {
        ModelInfo(name: name, devices: devices)
    }

    func testMeshMatchesIsCaseInsensitive() {
        XCTAssertTrue(ModelRanking.meshMatches(["P300X2", "T3K"], boxMesh: "p300x2"))
        XCTAssertFalse(ModelRanking.meshMatches(["T3K"], boxMesh: "p300x2"))
    }

    func testNilBoxMeshMatchesNothingButRankingKeepsAllCompatible() {
        // Unknown hardware: everything goes in the compatible tier (no split).
        let models = [m("Qwen3-8B", ["P300X2"]), m("Llama-3.1-70B", ["T3K"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: nil)
        XCTAssertTrue(ranked.incompatible.isEmpty)
        XCTAssertEqual(ranked.compatible.flatMap { $0.models }.count, 2)
    }

    func testCompatibleTierExcludesIncompatibleModels() {
        let models = [m("Qwen3-8B", ["P300X2"]), m("Big-70B", ["T3K"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: "p300x2")
        XCTAssertEqual(ranked.compatible.flatMap { $0.models }.map(\.name), ["Qwen3-8B"])
        XCTAssertEqual(ranked.incompatible.map(\.name), ["Big-70B"])
    }

    func testCompatibleTierIsFamilyGrouped() {
        let models = [m("Qwen3-8B", ["P300X2"]), m("Llama-3.1-8B-Instruct", ["P300X2"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: "p300x2")
        XCTAssertEqual(ranked.compatible.map(\.family).sorted(), ["Llama", "Qwen"])
    }

    func testCompatibilityLabel() {
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Qwen3-8B", ["P300X2"]), boxMesh: "p300x2"),
            "Runs on P300X2")
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Big-70B", ["T3K"]), boxMesh: "p300x2"),
            "Needs T3K")
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Qwen3-8B", ["P300X2"]), boxMesh: nil),
            "")
    }
}
```

- [ ] **Step 2: Run, expect FAIL**

Run: `cd macos/TTStation && swift test --filter ModelRankingTests`
Expected: compile error (`ModelRanking` undefined).

- [ ] **Step 3: Implement `ModelRanking.swift`:**

```swift
import Foundation

/// Pure hardware-aware ranking of a box's servable models.
///
/// Splits `[ModelInfo]` into a **compatible** tier (models whose declared
/// `devices` include this box's detected mesh) and an **incompatible** tier
/// (everything else, annotated with the hardware it needs). The compatible tier
/// is family-grouped for display and its families/models keep `ModelDefaults`'
/// existing ordering. When `boxMesh` is `nil` (mesh unknown) there is no basis
/// to split, so every model is treated as compatible.
public enum ModelRanking {
    public struct RankedModels: Equatable {
        public let compatible: [(family: String, models: [ModelInfo])]
        public let incompatible: [ModelInfo]

        public static func == (l: RankedModels, r: RankedModels) -> Bool {
            l.incompatible == r.incompatible
                && l.compatible.map(\.family) == r.compatible.map(\.family)
                && l.compatible.map(\.models) == r.compatible.map(\.models)
        }
    }

    /// Case-insensitive membership of `boxMesh` in a model's device meshes.
    /// `nil` boxMesh matches nothing (caller decides how to treat unknown).
    public static func meshMatches(_ devices: [String], boxMesh: String?) -> Bool {
        guard let boxMesh else { return false }
        return devices.contains { $0.caseInsensitiveCompare(boxMesh) == .orderedSame }
    }

    public static func rankForHardware(_ models: [ModelInfo], boxMesh: String?) -> RankedModels {
        guard let boxMesh else {
            return RankedModels(
                compatible: ModelDefaults.groupModelsByFamily(models),
                incompatible: [])
        }
        let compatible = models.filter { meshMatches($0.devices, boxMesh: boxMesh) }
        let incompatible = models
            .filter { !meshMatches($0.devices, boxMesh: boxMesh) }
            .sorted { $0.name < $1.name }
        return RankedModels(
            compatible: ModelDefaults.groupModelsByFamily(compatible),
            incompatible: incompatible)
    }

    /// A short human label: `"Runs on P300X2"` for a compatible model,
    /// `"Needs <mesh, mesh>"` for an incompatible one, `""` when mesh unknown.
    public static func compatibilityLabel(for model: ModelInfo, boxMesh: String?) -> String {
        guard let boxMesh else { return "" }
        if meshMatches(model.devices, boxMesh: boxMesh) {
            return "Runs on \(boxMesh.uppercased())"
        }
        return "Needs \(model.devices.joined(separator: ", "))"
    }
}
```

- [ ] **Step 4: Run, expect PASS**

Run: `cd macos/TTStation && swift test --filter ModelRankingTests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/ModelRanking.swift macos/TTStation/Tests/TTStationKitTests/ModelRankingTests.swift
git commit -m "feat(macos): pure hardware-aware model ranking"
```

---

## Task 7: Compatible-first smart default

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/ModelDefaults.swift:25-39` (`pickDefaultModel`)
- Modify: `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift:102-115` (`loadModels` passes the mesh)
- Test: `macos/TTStation/Tests/TTStationKitTests/ModelDefaultsTests.swift`

**Interfaces:**
- Consumes: `ModelRanking.meshMatches` (Task 6), `BoxRecord.deviceMesh` (Task 5).
- Produces: `ModelDefaults.pickDefaultModel(from:lastUsed:boxMesh:) -> String?` (new `boxMesh` param, defaulted `nil` so old call sites/tests compile).

- [ ] **Step 1: Write the failing tests** in `ModelDefaultsTests.swift`:

```swift
func testPickDefaultPrefersCompatibleModel() {
    let models = [
        ModelInfo(name: "Llama-3.1-8B-Instruct", devices: ["T3K"]),   // higher score, wrong hw
        ModelInfo(name: "Qwen3-7B-Instruct", devices: ["P300X2"]),    // compatible
    ]
    let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: nil, boxMesh: "p300x2")
    XCTAssertEqual(pick, "Qwen3-7B-Instruct")
}

func testPickDefaultLastUsedWinsOnlyIfCompatible() {
    let models = [
        ModelInfo(name: "Qwen3-7B-Instruct", devices: ["P300X2"]),
        ModelInfo(name: "Old-Pick", devices: ["T3K"]),
    ]
    // Last-used is incompatible → fall back to best compatible.
    let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: "Old-Pick", boxMesh: "p300x2")
    XCTAssertEqual(pick, "Qwen3-7B-Instruct")
}

func testPickDefaultFallsBackToGlobalWhenNoneCompatible() {
    let models = [ModelInfo(name: "Llama-3.1-8B-Instruct", devices: ["T3K"])]
    let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: nil, boxMesh: "p300x2")
    XCTAssertEqual(pick, "Llama-3.1-8B-Instruct")
}
```

- [ ] **Step 2: Run, expect FAIL** (no `boxMesh:` label).

Run: `cd macos/TTStation && swift test --filter ModelDefaultsTests`

- [ ] **Step 3: Extend `pickDefaultModel`.** Change the signature to `pickDefaultModel(from models: [ModelInfo], lastUsed: String?, boxMesh: String? = nil) -> String?`. New logic:
  1. If `lastUsed` names a model in `models` AND (`boxMesh == nil` OR that model is `meshMatches`) → return it.
  2. Compute the compatible subset (`meshMatches`); if non-empty, pick the max by `score` (keep the existing tie-break) within it.
  3. Else fall back to the current global best over all `models`.
  Keep the existing `score`/tie-break helper.

- [ ] **Step 4: Wire the mesh in `loadModels`.** In `BoxViewModel.loadModels`, pass `boxMesh: record.deviceMesh` to `pickDefaultModel`.

- [ ] **Step 5: Run all kit tests, expect PASS**

Run: `cd macos/TTStation && swift test`
Expected: PASS (old `pickDefaultModel` tests still green via the defaulted param).

- [ ] **Step 6: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/ModelDefaults.swift macos/TTStation/Sources/TTStationKit/BoxViewModel.swift macos/TTStation/Tests/TTStationKitTests/ModelDefaultsTests.swift
git commit -m "feat(macos): compatible-first smart default model"
```

---

## Task 8: `TelemetrySnapshot` (Swift, pure decode)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/TelemetrySnapshot.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/TelemetrySnapshotTests.swift`

**Interfaces:**
- Produces:
  - `struct DeviceReading: Equatable { let index: Int; let boardType: String; let tempC: Double?; let utilization: Double? }`
  - `struct TelemetrySnapshot: Equatable { let devices: [DeviceReading] }`
  - `TelemetrySnapshot.decode(_ frame: String) -> TelemetrySnapshot` (tolerant; never throws)

- [ ] **Step 1: Write the failing tests:**

```swift
import XCTest
@testable import TTStationKit

final class TelemetrySnapshotTests: XCTestCase {
    func testDecodesCanonicalFrame() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"61.4"}}]}"#
        let snap = TelemetrySnapshot.decode(frame)
        XCTAssertEqual(snap.devices.count, 1)
        XCTAssertEqual(snap.devices[0].index, 0)
        XCTAssertEqual(snap.devices[0].boardType, "p300c")
        XCTAssertEqual(snap.devices[0].tempC, 61.4)
    }

    func testTempMayBeNumericOrString() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":60}}]}"#
        XCTAssertEqual(TelemetrySnapshot.decode(frame).devices.first?.tempC, 60)
    }

    func testMissingTelemetryYieldsNilTemp() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#
        let snap = TelemetrySnapshot.decode(frame)
        XCTAssertEqual(snap.devices.count, 1)
        XCTAssertNil(snap.devices[0].tempC)
    }

    func testGarbageYieldsEmptySnapshot() {
        XCTAssertTrue(TelemetrySnapshot.decode("not json").devices.isEmpty)
        XCTAssertTrue(TelemetrySnapshot.decode(#"{"device_info":[]}"#).devices.isEmpty)
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

Run: `cd macos/TTStation && swift test --filter TelemetrySnapshotTests`

- [ ] **Step 3: Implement `TelemetrySnapshot.swift`** with a tolerant `JSONSerialization`-based decode (temp may arrive as a JSON string `"61.4"` or a number). Parse `device_info[]`, index by position, read `board_info.board_type` (default `""`), `telemetry.asic_temperature` → `Double?`, and optional utilization if a known key is present (leave `nil` otherwise). Never throw — return an empty snapshot on any failure.

- [ ] **Step 4: Run, expect PASS.**

Run: `cd macos/TTStation && swift test --filter TelemetrySnapshotTests`

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TelemetrySnapshot.swift macos/TTStation/Tests/TTStationKitTests/TelemetrySnapshotTests.swift
git commit -m "feat(macos): tolerant tt-smi telemetry frame decode"
```

---

## Task 9: Connect install-command builders + VS Code toolkit args (Swift, pure)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/Provisioning.swift`
- Modify: `macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift:42-47` (`VSCodeLauncher`)
- Test: `macos/TTStation/Tests/TTStationKitTests/ProvisioningTests.swift`, extend `WorkbenchLaunchersTests.swift`

**Interfaces:**
- Produces:
  - `Provisioning.brewInstallArgs(formula: String) -> [String]` → `["install", formula]`
  - `Provisioning.opencodeFormula = "sst/tap/opencode"`, `Provisioning.uvFormula = "uv"`
  - `VSCodeLauncher.remoteArgs(user:host:path:installToolkit:) -> [String]` adds `--install-extension Tenstorrent.tt-vscode-toolkit` when `installToolkit` is true.
  - `VSCodeLauncher.toolkitExtensionID = "Tenstorrent.tt-vscode-toolkit"`

- [ ] **Step 1: Write the failing tests:**

```swift
// ProvisioningTests.swift
func testBrewInstallArgs() {
    XCTAssertEqual(Provisioning.brewInstallArgs(formula: "uv"), ["install", "uv"])
    XCTAssertEqual(Provisioning.brewInstallArgs(formula: Provisioning.opencodeFormula),
                   ["install", "sst/tap/opencode"])
}

// WorkbenchLaunchersTests.swift (add)
func testVSCodeRemoteArgsWithToolkit() {
    let args = VSCodeLauncher.remoteArgs(user: "u", host: "h", path: "/home/u", installToolkit: true)
    XCTAssertEqual(args, ["--install-extension", "Tenstorrent.tt-vscode-toolkit",
                          "--remote", "ssh-remote+u@h", "/home/u"])
}

func testVSCodeRemoteArgsWithoutToolkit() {
    let args = VSCodeLauncher.remoteArgs(user: "u", host: "h", path: "/home/u", installToolkit: false)
    XCTAssertEqual(args, ["--remote", "ssh-remote+u@h", "/home/u"])
}
```

- [ ] **Step 2: Run, expect FAIL.**

Run: `cd macos/TTStation && swift test --filter ProvisioningTests`

- [ ] **Step 3: Implement `Provisioning.swift`** with the constants + `brewInstallArgs`, and extend `VSCodeLauncher.remoteArgs` to prepend the install-extension flags when `installToolkit`. Keep the existing 3-arg `remoteArgs` as a convenience overload that calls the new one with `installToolkit: false` so nothing else breaks, OR update the sole call site in Task 12.

- [ ] **Step 4: Run all kit tests, expect PASS.**

Run: `cd macos/TTStation && swift test`

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/Provisioning.swift macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift macos/TTStation/Tests/TTStationKitTests/ProvisioningTests.swift macos/TTStation/Tests/TTStationKitTests/WorkbenchLaunchersTests.swift
git commit -m "feat(macos): brew install builders + VS Code toolkit args"
```

---

## Task 10: `TelemetryService` (Swift, WS I/O — owner-verified)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/TelemetryService.swift`

**Interfaces:**
- Consumes: `TelemetrySnapshot.decode` (Task 8).
- Produces: `@Observable @MainActor final class TelemetryService` with `var snapshot: TelemetrySnapshot?`, `var state: TelemetryService.ConnectionState` (`.idle/.connecting/.live/.stalled/.failed(String)`), `func start(host: String, ctrlPort: Int)`, `func stop()`.

- [ ] **Step 1: Implement the service.** Use `URLSessionWebSocketTask` against `ws://<host>:<ctrlPort>/telemetry`. On each received text message: `snapshot = TelemetrySnapshot.decode(text)`, `state = .live`, then recursively `receive` again. On error: `state = .failed(...)` then reconnect with backoff (e.g. 1s→2s→5s cap) unless `stop()` was called. `stop()` cancels the task and any pending reconnect. Guard against double-`start`. No auth header (unauthed route).

- [ ] **Step 2: Build the kit, expect success**

Run: `cd macos/TTStation && swift build`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TelemetryService.swift
git commit -m "feat(macos): read-only telemetry WebSocket service"
```

---

## Task 11: `TTTheme` (Swift)

**Files:**
- Create: `macos/TTStation/AppShell/Sources/TTTheme.swift`

**Interfaces:**
- Produces: `enum TTTheme` with `static let teal = Color(red: 0x4F/255, green: 0xD1/255, blue: 0xC5/255)`, `static let ground = Color(red: 0x0F/255, green: 0x2A/255, blue: 0x35/255)`, a `tempColor(_ c: Double) -> Color` ramp (teal < 55 → yellow ~70 → red ≥ 85), and `static let mono = Font.system(.caption, design: .monospaced)` (Berkeley Mono if available via `Font.custom("Berkeley Mono", size:)` with a monospaced fallback).

- [ ] **Step 1: Implement `TTTheme.swift`** with the palette constants, the temperature ramp (interpolate or bucket teal→yellow→red), and font helpers. Keep it a plain value enum, no state.

- [ ] **Step 2: Build the app target** (after Task 14 wires it in, this is just a compile check now):

Run: `cd macos/TTStation && swift build`
Expected: compiles (file is standalone until views consume it).

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/TTTheme.swift
git commit -m "feat(macos): TT brand theme (palette + temp ramp + mono font)"
```

---

## Task 12: Install-as-needed provisioning in `LaunchController`

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/LaunchController.swift`

**Interfaces:**
- Consumes: `Provisioning` (Task 9), existing `resolveBrewBinary`, `spawnDetached`, `runOsascript`.
- Produces: per-action provisioning that installs missing deps before launching; a `resolveBrewBinary("brew")`-style check so a missing Homebrew is reported (not auto-installed). Adds a `ProvisionPhase` enum surfaced per action so views can show `Installing… / Starting…`.

- [ ] **Step 1: Add a brew runner helper.** Add `static func runBrewInstall(formula: String) async -> Bool` that resolves the `brew` binary via `resolveBrewBinary("brew")` (probing `/opt/homebrew/bin/brew`, `/usr/local/bin/brew`), runs `brew install <formula>` via `Process` (capturing status), and returns success. If `brew` is absent, return `false` and set an error string that names brew.sh.

- [ ] **Step 2: opencode — install as needed.** In `openInOpenCode`, replace the hard `guard resolveBrewBinary("opencode") != nil else { error }` with: if absent, set a phase `installing`, `await runBrewInstall(formula: Provisioning.opencodeFormula)`; if that fails, set `openCodeError` (mention brew if brew was the problem) and return; then continue to the existing config-write + Terminal-open path.

- [ ] **Step 3: Open WebUI — install as needed.** In `openWebUI`, when `resolveBrewBinary("uvx") == nil`, `await runBrewInstall(formula: Provisioning.uvFormula)` (uv ships `uvx`); re-resolve; if still missing, set `webUIError` and return; else continue to the existing spawn+poll path. Keep the "already healthy → just open" fast path first.

- [ ] **Step 4: VS Code — toolkit install.** In `openVSCode`, call `VSCodeLauncher.remoteArgs(user:host:path:installToolkit: true)`. Wrap the toolkit failure as non-fatal: if the marketplace install fails, still open the Remote-SSH window (run remoteArgs with `installToolkit: false`) and set a soft note on `vscodeError` (non-blocking). Optionally probe `~/code/tt-vscode-toolkit` for a built `.vsix` and pass its path to `--install-extension` instead of the marketplace id when present (best-effort; keep it simple — marketplace id first).

- [ ] **Step 5: Add phase state (optional but recommended).** Add `var openCodePhase / webUIPhase: String?` (e.g. `"Installing opencode…"`, `"Starting Open WebUI…"`) set/cleared around each stage, so Task 13's Connect card can render progress. Clear on completion/failure.

- [ ] **Step 6: Build, expect success**

Run: `cd macos/TTStation && swift build`
Expected: compiles.

- [ ] **Step 7: Commit**

```bash
git add macos/TTStation/AppShell/Sources/LaunchController.swift
git commit -m "feat(macos): install-as-needed Connect + VS Code toolkit"
```

---

## Task 13: Card views (Swift, SwiftUI — owner-verified)

**Files:**
- Create: `macos/TTStation/AppShell/Sources/BoxHeaderView.swift`
- Create: `macos/TTStation/AppShell/Sources/DeviceStripView.swift`
- Create: `macos/TTStation/AppShell/Sources/ModelBrowserView.swift`
- Create: `macos/TTStation/AppShell/Sources/ConnectCardView.swift`
- Create: `macos/TTStation/AppShell/Sources/WorkbenchCardView.swift`
- Create: `macos/TTStation/AppShell/Sources/ServingCardView.swift`
- Create: `macos/TTStation/AppShell/Sources/CardContainer.swift` (a reusable titled card wrapper)

**Interfaces:**
- Consumes: `BoxViewModel`, `TelemetryService` (Task 10), `ModelRanking` (Task 6), `TTTheme` (Task 11), `LaunchController` (Task 12).
- Produces: six focused card views + a `CardContainer<Content>` wrapper (`GroupBox`-style: title + rounded material background + padding).

- [ ] **Step 1: `CardContainer.swift`** — a generic `View` taking a `title: String` and `@ViewBuilder content`, rendering a titled card (rounded rect, `.regularMaterial` or `Color(nsColor:).opacity`, subtle border, teal-tinted title). Every card uses it for visual consistency.

- [ ] **Step 2: `BoxHeaderView.swift`** — name (title font), `chips` humanized, a mesh badge (`box.record.deviceMesh?.uppercased()` in a teal capsule, mono font), and a reachability/status dot (green serving / amber starting / gray idle / red error) using `TTTheme`.

- [ ] **Step 3: `DeviceStripView.swift`** — takes a `TelemetryService`; renders `service.snapshot?.devices` as a row/grid of compact per-device tiles: `dev<i>`, a temp value in `TTTheme.mono` colored via `TTTheme.tempColor`, a thin utilization bar when present, and a small horizontal temp meter. Shows "telemetry unavailable" (secondary) when `state == .failed`/no snapshot. A trailing `Open tt-toplike ↗` button calling the launcher. `.task`/`.onDisappear` start/stop the service for `box.record.host` / `ctrlPort`.

- [ ] **Step 4: `ModelBrowserView.swift`** — replaces `ModelPickerView`'s flat family list with the tiered ranking: a search field (kept, plain `TextField`), then a **"Runs on this box"** section (from `ModelRanking.rankForHardware(...).compatible`, family-grouped, pinned headers) and a dimmed, collapsible **"Needs other hardware"** section (`.incompatible`, each row showing `ModelRanking.compatibilityLabel`). Selection still sets `box.selectedModel`. Search filters both tiers. Reuse the row style from `ModelPickerView` (checkmark + name + compat label). Keep `maxListHeight` for the popover reuse if needing it; the window uses uncapped.

- [ ] **Step 5: `ConnectCardView.swift`** — the serving-only front-end launchers (Open WebUI / opencode) with the install-as-needed phase text from `LaunchController` (`Installing opencode…`, `Starting Open WebUI…`), spinners, and errors. Shown only when `box.endpoint != nil`.

- [ ] **Step 6: `WorkbenchCardView.swift`** — Terminal / tt-toplike / VS Code as first-class buttons with SF Symbols + one-line subtitles, per-action spinner + error, calling `LaunchController`'s workbench methods (VS Code now installs the toolkit per Task 12).

- [ ] **Step 7: `ServingCardView.swift`** — the `box.serving` list (agent + `external` badge) with endpoint copy, extracted from the current inline block.

- [ ] **Step 8: Build, expect success**

Run: `cd macos/TTStation && swift build`
Expected: compiles (views not yet composed into the window — that's Task 14; compile-check each references only defined symbols).

- [ ] **Step 9: Commit**

```bash
git add macos/TTStation/AppShell/Sources/CardContainer.swift macos/TTStation/AppShell/Sources/BoxHeaderView.swift macos/TTStation/AppShell/Sources/DeviceStripView.swift macos/TTStation/AppShell/Sources/ModelBrowserView.swift macos/TTStation/AppShell/Sources/ConnectCardView.swift macos/TTStation/AppShell/Sources/WorkbenchCardView.swift macos/TTStation/AppShell/Sources/ServingCardView.swift
git commit -m "feat(macos): focused card views for the control-room window"
```

---

## Task 14: Compose the window + trim the popover + theme + icon

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift` (compose the cards)
- Modify: `macos/TTStation/AppShell/Sources/BoxDetailView.swift` (popover: quick actions + temp chip)
- Modify: `macos/TTStation/AppShell/Sources/BoxSidebarView.swift` / `BoxRowView.swift` (status dot colors via `TTTheme`)
- Modify: `macos/TTStation/AppShell/Sources/TTStationApp.swift` (`.tint(TTTheme.teal)`)
- Modify: `macos/TTStation/AppShell/Assets.xcassets/MenuBarIcon.imageset` (proper TT template icon per memory note)

**Interfaces:**
- Consumes: all Task 13 cards, `TTTheme`.

- [ ] **Step 1: Compose `BoxWorkspaceView`.** Replace the current monolithic body with a `VStack` of the cards in order: `BoxHeaderView`, pairing (kept inline for unpaired), `DeviceStripView`, `ModelBrowserView` + Run/Stop + starting/serving/endpoint, `ConnectCardView`, `WorkbenchCardView`, `ServingCardView`. Move each card into a `CardContainer`. Preserve the existing run/stop/pairing logic and `@State private var launcher`.

- [ ] **Step 2: Trim the popover `BoxDetailView`.** Keep pairing, Run/Stop on the smart default, endpoint + copy, and add a compact live temp chip (a small `TelemetryService` reading, or the max device temp) beside the serving line; keep "Open TTStation window". Remove anything now better served by the window.

- [ ] **Step 3: Theme the app.** Add `.tint(TTTheme.teal)` at the `MenuBarExtra`/`Window` content roots; apply `TTTheme` status-dot colors in the sidebar/rows/popover.

- [ ] **Step 4: Menu-bar icon.** Replace `MenuBarIcon` with a proper TT template PDF/PNG pulled from `tt-vscode-toolkit` / `tt-local-generator` (per memory `ttstation-menubar-icon-source`); mark it a template image so it tints for light/dark menu bars.

- [ ] **Step 5: Regenerate the Xcode project + build**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate \
  && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Expected: BUILD SUCCEEDED.

- [ ] **Step 6: Full kit test sweep**

Run: `cd macos/TTStation && swift test`
Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add macos/TTStation/AppShell
git commit -m "feat(macos): compose control-room window, trim popover, TT theme + icon"
```

---

## Task 15: Version bump, docs, live verification

**Files:**
- Modify: `macos/TTStation/AppShell/project.yml` (`MARKETING_VERSION: 0.3.0`)
- Modify: `macos/README.md`, `CLAUDE.md`

- [ ] **Step 1: Bump the version** to `0.3.0` in `project.yml`; regenerate + build once more (as Task 14 Step 5).

- [ ] **Step 2: Update docs.** `macos/README.md`: new window control-room, hardware-aware ranking, live telemetry, fast Connect, workbench+toolkit. `CLAUDE.md`: refresh the "Current state" macOS bullet to 0.3.0 and note the agent `device_mesh` addition.

- [ ] **Step 3: Live verification against QB2** (`qb2-lab.local:8765`, currently connected). Rebuild the agent (`cargo build --release -p tt-station-agentd`) and restart it via the box panel so `/status` carries `device_mesh`; rebuild `tt` (and clear quarantine / ad-hoc sign the `~/.local/bin` copy per memory `tt-cli-install-gatekeeper`). Then, in the app: confirm the mesh badge shows `P300X2`, models split into "Runs on this box" vs "Needs other hardware", the device strip shows live temps, and each workbench + Connect action launches (installing deps as needed).

- [ ] **Step 4: Commit**

```bash
git add macos/TTStation/AppShell/project.yml macos/README.md CLAUDE.md
git commit -m "chore(macos): TTStation 0.3.0 — docs + version bump"
```

---

## Self-review notes

- **Spec coverage:** Component 1 → Tasks 1–4; Component 2 → Tasks 5–7; Component 3 → Tasks 8, 10, 13(step 3); Component 4 → Tasks 11, 14; Component 5 → Tasks 9, 12(step 4), 13(step 6); Component 6 → Tasks 9, 12, 13(step 5). Versioning/docs → Task 15.
- **Type consistency:** `deviceMesh`/`device_mesh`, `ModelRanking.rankForHardware`/`RankedModels`, `TelemetrySnapshot.decode`, `Provisioning.brewInstallArgs`, `VSCodeLauncher.remoteArgs(...,installToolkit:)` are referenced consistently across tasks.
- **Owner-verified vs unit-tested:** pure logic (Tasks 1,5,6,7,8,9) is TDD; process/socket/SwiftUI (Tasks 10,12,13,14) is owner-verified, matching the repo convention (`LaunchController` is not unit-tested).
```
