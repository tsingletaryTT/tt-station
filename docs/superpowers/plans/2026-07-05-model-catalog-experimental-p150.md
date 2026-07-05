# Hardware-aware Model Catalog (Experimental + P150 + Toolchain Messaging) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Merge the public Tenstorrent compatibility catalog with a box's live `/models` in `tt` (Rust), classify every model for the box's mesh into Runs-here / Experimental / Needs-other-hardware, add P150 x1–x4 mesh detection, and render the three tiers in the macOS app with "bring more up with the workbench" messaging.

**Architecture:** All fetch/merge/classify lives in Rust — `libttstation` gets pure catalog types + `HW_MAP` + a `classify()` function; `tt` gets fetch+cache + a `tt catalog` command. The app is a veneer: `TTClient.catalog(host:)` shells `tt --json catalog` and renders. The agent's `detect_device_mesh` gains P150 counts.

**Tech Stack:** Rust (`reqwest::blocking`, serde, clap), Swift 5 / SwiftUI, XcodeGen, `cargo test` / `swift test`.

## Global Constraints

- **Veneer rule:** fetch/merge/classify live in Rust (`tt`/`libttstation`); the app shells out to `tt --json catalog`. NO new Swift network I/O (telemetry WebSocket stays the only exception).
- **Catalog source:** `https://d1oi7xemha0dsy.cloudfront.net/data/compatibility.json` (public, unauthenticated). Cache at `~/.cache/tt-station/compatibility.json`, **24 h TTL**. Offline-tolerant: stale cache → then "unavailable" (never crash).
- **`status` values:** `Supported` | `Experimental` | `Not Supported` (tolerant of unknown → `Other`).
- **HW_MAP (verbatim seed, lowercased keys → mesh):** `n150→N150, n300→N300, p100→P100, p150→P150, p300→P300, galaxy→T3K, quietbox→P150X4, "quietbox 2"→P300X2, loudbox→P300X2, "2 x quietbox"→P150X8, "2 x galaxy"→GALAXY, "4 x galaxy"→GALAXY, quad_galaxy→GALAXY`. Unmapped → uppercased passthrough.
- **P150 mesh table (add):** `("p150"|"p150c", 1)→p150`, `2→p150x2`, `3→p150x3`, `4→p150x4` (x4 already exists).
- **Tiers:** runs_here = live `/models` ∪ catalog `Supported` on box mesh; experimental = catalog `Experimental` on box mesh (not already runs_here); other_hardware = catalog Supported/Experimental only on other meshes. `Not Supported`-everywhere omitted. `box_mesh == None` → no split (everything → runs_here / a single list).
- **Merge key:** lowercase, segment after last `/`, replace `[._ ]`→`-`, collapse repeats. Live models ALWAYS shown (unmatched = un-enriched, never hidden).
- **App version → 0.5.0** on completion.
- Pure logic is TDD; process/network/SwiftUI is owner-verified (fixture fast-path where possible).

---

## Task 1: Catalog types + `HW_MAP` (libttstation, pure)

**Files:**
- Create: `crates/libttstation/src/catalog.rs`
- Modify: `crates/libttstation/src/lib.rs` (`pub mod catalog;`)

**Interfaces:**
- Produces: `CompatCatalog { models: Vec<CompatModel> }`; `CompatModel { id, display_name, family, tasks: Vec<String>, model_size: Option<String>, model_size_num: Option<f64>, model_description: Option<String>, compatibility: Vec<HardwareCompat> }`; `HardwareCompat { hardware: String, chip_set: String, hardware_family: String, status: CompatStatus, software: Vec<String> }`; `enum CompatStatus { Supported, Experimental, NotSupported, Other(String) }`; `pub fn hw_to_mesh(hardware: &str) -> String`.

- [ ] **Step 1: Write failing tests** in `catalog.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_map_known_and_passthrough() {
        assert_eq!(hw_to_mesh("Quietbox 2"), "P300X2");
        assert_eq!(hw_to_mesh("quietbox"), "P150X4");
        assert_eq!(hw_to_mesh("Galaxy"), "T3K");
        assert_eq!(hw_to_mesh("p150"), "P150");
        assert_eq!(hw_to_mesh("p300"), "P300");
        assert_eq!(hw_to_mesh("2 x Quietbox"), "P150X8");
        assert_eq!(hw_to_mesh("something-new"), "SOMETHING-NEW"); // passthrough uppercased
    }

    #[test]
    fn status_parses_tolerantly() {
        let j = r#"{"hardware":"p150","chip_set":"Blackhole","hardware_family":"Card","status":"Experimental","software":["tt-forge"]}"#;
        let hc: HardwareCompat = serde_json::from_str(j).unwrap();
        assert_eq!(hc.status, CompatStatus::Experimental);
        let j2 = r#"{"hardware":"x","chip_set":"","hardware_family":"","status":"Weird","software":[]}"#;
        let hc2: HardwareCompat = serde_json::from_str(j2).unwrap();
        assert_eq!(hc2.status, CompatStatus::Other("Weird".to_string()));
    }

    #[test]
    fn catalog_parses_full_entry() {
        let j = r#"{"models":[{"id":"qwen3-8b","display_name":"Qwen3-8B","family":"Qwen","tasks":["Text Generation"],"model_size":"8B","compatibility":[{"hardware":"Quietbox 2","chip_set":"Blackhole","hardware_family":"Quietbox","status":"Supported","software":["tt-inference-server"]}]}]}"#;
        let c: CompatCatalog = serde_json::from_str(j).unwrap();
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.models[0].display_name, "Qwen3-8B");
        assert_eq!(c.models[0].compatibility[0].status, CompatStatus::Supported);
    }
}
```

- [ ] **Step 2: Register module** — add `pub mod catalog;` to `crates/libttstation/src/lib.rs`.

- [ ] **Step 3: Run, expect FAIL** — `cargo test -p libttstation catalog`.

- [ ] **Step 4: Implement `catalog.rs`.** serde structs with `#[serde(default)]` on optional/absent fields (`tasks`, `software`, `model_size*`, `model_description`) so partial entries parse. `CompatStatus` with a custom `Deserialize` from a string: `"Supported"→Supported`, `"Experimental"→Experimental`, `"Not Supported"→NotSupported`, else `Other(s)`; derive `PartialEq, Clone, Debug`. `hw_to_mesh`: `match hardware.to_lowercase().as_str()` over the HW_MAP table (Global Constraints), default `hardware.to_uppercase()`.

- [ ] **Step 5: Run, expect PASS** — `cargo test -p libttstation catalog`.

- [ ] **Step 6: Commit**

```bash
git add crates/libttstation/src/catalog.rs crates/libttstation/src/lib.rs
git commit -m "feat(lib): compatibility catalog types + HW_MAP (hw_to_mesh)"
```

---

## Task 2: `classify()` + `BoxCatalog` (libttstation, pure)

**Files:**
- Modify: `crates/libttstation/src/catalog.rs` (add classify + output types)

**Interfaces:**
- Consumes: `CompatCatalog`, `CompatStatus`, `hw_to_mesh` (Task 1); `ModelInfo` (existing, `model.rs`).
- Produces: `BoxCatalog { box_mesh: Option<String>, catalog_available: bool, catalog_stale: bool, runs_here: Vec<CatalogEntry>, experimental: Vec<CatalogEntry>, other_hardware: Vec<CatalogEntry> }`; `CatalogEntry { id, display_name, family, size: Option<String>, software: Vec<String>, meshes: Vec<String>, needed_hardware: Vec<String>, available_now: bool, status_here: String }` (both `Serialize, Deserialize, Clone, PartialEq`); `pub fn normalize_key(s: &str) -> String`; `pub fn classify(catalog: Option<&CompatCatalog>, live_models: &[ModelInfo], box_mesh: Option<&str>, catalog_stale: bool) -> BoxCatalog`.

- [ ] **Step 1: Write failing tests** (append to `catalog.rs` tests):

```rust
#[test]
fn normalize_key_forms() {
    assert_eq!(normalize_key("Qwen/Qwen3-8B"), "qwen3-8b");
    assert_eq!(normalize_key("Qwen3-8B"), "qwen3-8b");
    assert_eq!(normalize_key("bge_large en.v1.5"), "bge-large-en-v1-5");
}

#[test]
fn classify_tiers_by_box_mesh() {
    use crate::model::ModelInfo;
    let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
      {"id":"a","display_name":"A","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
      {"id":"b","display_name":"B","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Experimental","software":["tt-forge"]}]},
      {"id":"c","display_name":"C","family":"F","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":["tt-metal"]}]},
      {"id":"d","display_name":"D","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Not Supported","software":[]}]}
    ]}"#).unwrap();
    let live = vec![]; // no live models
    let bc = classify(Some(&cat), &live, Some("p300x2"), false);
    assert_eq!(bc.runs_here.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec!["a"]);
    assert_eq!(bc.experimental.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec!["b"]);
    assert_eq!(bc.other_hardware.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec!["c"]);
    // d is Not Supported everywhere -> omitted
    assert!(!bc.runs_here.iter().chain(&bc.experimental).chain(&bc.other_hardware).any(|e| e.id == "d"));
    // c is annotated with the mesh it needs
    assert_eq!(bc.other_hardware[0].needed_hardware, vec!["T3K"]);
    assert_eq!(bc.catalog_available, true);
}

#[test]
fn classify_live_model_always_runs_here_and_marks_available() {
    use crate::model::ModelInfo;
    let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
      {"id":"qwen3-8b","display_name":"Qwen3-8B","family":"Qwen","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
    ]}"#).unwrap();
    let live = vec![ModelInfo { name: "Qwen/Qwen3-8B".into(), devices: vec!["P300X2".into()] }];
    let bc = classify(Some(&cat), &live, Some("p300x2"), false);
    // live model wins -> runs_here, available_now, deduped with the catalog entry (matched by normalize_key)
    assert_eq!(bc.runs_here.len(), 1);
    assert!(bc.runs_here[0].available_now);
    assert!(bc.other_hardware.is_empty()); // not double-listed
}

#[test]
fn classify_unavailable_catalog_returns_live_only() {
    use crate::model::ModelInfo;
    let live = vec![ModelInfo { name: "X/Y".into(), devices: vec![] }];
    let bc = classify(None, &live, Some("p300x2"), false);
    assert_eq!(bc.catalog_available, false);
    assert_eq!(bc.runs_here.len(), 1);
    assert!(bc.experimental.is_empty() && bc.other_hardware.is_empty());
}

#[test]
fn classify_unknown_mesh_no_split() {
    use crate::model::ModelInfo;
    let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
      {"id":"a","display_name":"A","family":"F","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
    ]}"#).unwrap();
    let bc = classify(Some(&cat), &[], None, false);
    // no box mesh -> nothing goes to experimental/other; catalog models land in runs_here as a flat list
    assert!(bc.experimental.is_empty() && bc.other_hardware.is_empty());
    assert_eq!(bc.runs_here.len(), 1);
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p libttstation catalog`.

- [ ] **Step 3: Implement** `normalize_key`, `BoxCatalog`, `CatalogEntry`, `classify`:
  - `normalize_key(s)`: lowercase; take substring after the last `/`; replace each of `. _ ` (dot, underscore, space) with `-`; collapse consecutive `-`.
  - `classify`:
    - If `catalog` is `None`: `runs_here` = live models mapped to `CatalogEntry` (id=name, display_name=name, family via existing `ModelDefaults`-style split or just name, available_now=true, status_here="supported", meshes/needed empty), `catalog_available=false`, empty other tiers. Return.
    - Build a set of live normalized keys. For each catalog model, compute mapped meshes = distinct `hw_to_mesh(c.hardware)` over compatibility entries; determine `status_here`: if any entry maps to `box_mesh` (case-insensitive) with `Supported` → runs_here; else if any maps to `box_mesh` with `Experimental` → experimental; else if it has ANY Supported/Experimental entry (other mesh) → other_hardware (needed_hardware = distinct mapped meshes with a Supported/Experimental status, excluding box_mesh); else omit (Not Supported everywhere).
    - If `box_mesh` is `None`: every catalog model with any Supported/Experimental entry → runs_here (flat), no experimental/other.
    - `available_now` = normalized(catalog id or display_name) ∈ live keys. Live models NOT matched to any catalog entry are appended to `runs_here` as their own entries (available_now=true, status_here="supported").
    - Dedup: a catalog model whose key matches a live model is a single runs_here entry (available_now=true) — do not also emit it in other tiers.
    - `catalog_available=true`, `catalog_stale` passed through.

- [ ] **Step 4: Run, expect PASS** — `cargo test -p libttstation`.

- [ ] **Step 5: Commit**

```bash
git add crates/libttstation/src/catalog.rs
git commit -m "feat(lib): classify() + BoxCatalog (3-tier merge of catalog + live models)"
```

---

## Task 3: Catalog fetch + cache (`tt`, owner-verified + pure freshness)

**Files:**
- Create: `crates/tt/src/catalog.rs`
- Modify: `crates/tt/src/main.rs` (`mod catalog;`)

**Interfaces:**
- Consumes: `libttstation::catalog::CompatCatalog` (Task 1).
- Produces: `pub fn is_fresh(mtime_secs: u64, now_secs: u64, ttl_secs: u64) -> bool` (pure); `pub fn load_catalog(refresh: bool, file_override: Option<&std::path::Path>) -> (Option<CompatCatalog>, bool)` (catalog, `stale`); `fn cache_path() -> PathBuf`.

- [ ] **Step 1: Write the pure freshness test** in `catalog.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freshness_ttl() {
        assert!(is_fresh(1000, 1000 + 86399, 86400));   // within TTL
        assert!(!is_fresh(1000, 1000 + 86401, 86400));  // expired
        assert!(!is_fresh(2000, 1000, 86400));          // future mtime -> treat as not fresh
    }
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p tt catalog::tests::freshness_ttl`.

- [ ] **Step 3: Implement.**
  - `const URL: &str = "https://d1oi7xemha0dsy.cloudfront.net/data/compatibility.json";`
  - `const TTL: u64 = 86400;`
  - `is_fresh(mtime, now, ttl)`: `now >= mtime && now - mtime < ttl`.
  - `cache_path()`: `dirs`-style — `$HOME/.cache/tt-station/compatibility.json` (respect the crate's existing config-dir convention if one exists; else `$HOME/.cache/tt-station`).
  - `load_catalog(refresh, file_override)`:
    - If `file_override`: read+parse that file; return `(Some, false)` or `(None, false)` on error.
    - Else: if `!refresh` and cache exists and `is_fresh(cache mtime, now, TTL)` → parse cache → `(Some, false)`.
    - Else fetch via `reqwest::blocking::get(URL)` (short timeout ~10s); on success write cache (create dirs) and return `(Some(parsed), false)`.
    - On fetch failure: if a cache file exists (even stale), parse it → `(Some, true)` (stale); else `(None, false)`.
    - Any parse error → treat as unavailable for that source.

- [ ] **Step 4: Run freshness test, expect PASS** — `cargo test -p tt catalog::tests::freshness_ttl`. Build: `cargo build -p tt`.

- [ ] **Step 5: Owner-verify the fetch** (manual, note in report): `TT_CATALOG_FILE`-style `--catalog-file` fast path is exercised by Task 4's e2e; a real network fetch is owner-verified. Confirm `cargo build -p tt` clean.

- [ ] **Step 6: Commit**

```bash
git add crates/tt/src/catalog.rs crates/tt/src/main.rs
git commit -m "feat(tt): compatibility.json fetch + 24h cache (offline-tolerant)"
```

---

## Task 4: `tt catalog` command

**Files:**
- Modify: `crates/tt/src/main.rs` (Command enum + handler)

**Interfaces:**
- Consumes: `catalog::load_catalog` (Task 3), `libttstation::catalog::classify` (Task 2), `agent_client::{get_status, list_models}` (existing).
- Produces: `tt --json catalog --host <h> [--refresh] [--catalog-file <p>]` → `BoxCatalog` JSON; human form prints the three tiers.

- [ ] **Step 1: Study** the existing `Config`/`Models` command handlers in `main.rs` (arg parsing, `--json` vs human print, host resolution, `agent_client` calls). Add a `Catalog { host, refresh, catalog_file }` variant matching that style.

- [ ] **Step 2: Implement the handler.**
  - Resolve `box_mesh`: `agent_client::get_status(base)` → `StatusInfo.device_mesh` (unauthed; `None` on failure).
  - `live_models`: `agent_client::list_models(base)` → `.models` (empty on failure — non-fatal).
  - `(catalog, stale) = catalog::load_catalog(refresh, catalog_file.as_deref())`.
  - `let bc = classify(catalog.as_ref(), &live_models, box_mesh.as_deref(), stale);`
  - `--json`: print `serde_json::to_string(&bc)`. Human: print three sections (Runs on this box / Experimental / Needs other hardware) with model names + `needed_hardware` where relevant + a "catalog offline/cached" note when `!catalog_available`/`catalog_stale`.

- [ ] **Step 3: No-hardware e2e** — extend/add to `crates/tt/tests/e2e_mock.rs` (or a manual smoke noted in the report): with a fixture `compatibility.json` and mock-box's `/models`+`/status`, run `tt --json catalog --host 127.0.0.1:<port> --catalog-file <fixture>` and assert the JSON has `runs_here`/`experimental`/`other_hardware` and `box_mesh`. Add a fixture `crates/tt/tests/fixtures/compatibility.json` (a trimmed 3–4 model sample covering Supported/Experimental/other-mesh).

- [ ] **Step 4: Run tests + build** — `cargo test -p tt && cargo build -p tt`.

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/main.rs crates/tt/tests/
git commit -m "feat(tt): tt catalog command (merged 3-tier model catalog --json)"
```

---

## Task 5: Agent P150 x1–x4 mesh detection

**Files:**
- Modify: `crates/tt-station-agentd/src/device.rs` (the `match (board_type, count)` table + tests)

**Interfaces:**
- Produces: `detect_device_mesh` maps `("p150"|"p150c", 1|2|3|4) → p150 / p150x2 / p150x3 / p150x4`.

- [ ] **Step 1: Write failing tests** in `device.rs`:

```rust
#[test]
fn maps_p150_counts() {
    let f = |n: usize| {
        let entry = r#"{"board_info":{"board_type":"p150"}}"#;
        let arr = std::iter::repeat(entry).take(n).collect::<Vec<_>>().join(",");
        format!(r#"{{"device_info":[{arr}]}}"#)
    };
    assert_eq!(detect_device_mesh(&f(1)).as_deref(), Some("p150"));
    assert_eq!(detect_device_mesh(&f(2)).as_deref(), Some("p150x2"));
    assert_eq!(detect_device_mesh(&f(3)).as_deref(), Some("p150x3"));
    assert_eq!(detect_device_mesh(&f(4)).as_deref(), Some("p150x4"));
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p tt-station-agentd device`.

- [ ] **Step 3: Implement** — add to the match table:
```rust
("p150" | "p150c", 1) => "p150",
("p150" | "p150c", 2) => "p150x2",
("p150" | "p150c", 3) => "p150x3",
("p150" | "p150c", 4) => "p150x4",
```
(replace the single `("p150"|"p150c", 4)` arm; keep the others intact).

- [ ] **Step 4: Run, expect PASS** — `cargo test -p tt-station-agentd device`.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/device.rs
git commit -m "feat(agent): detect P150 x1-x4 device meshes"
```

---

## Task 6: Swift `BoxCatalog` decode + `TTClient.catalog` + VM load

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/Models.swift` (add `BoxCatalog`, `CatalogEntry`)
- Modify: `macos/TTStation/Sources/TTStationKit/TTClient.swift` (+ `TTCommands`)
- Modify: `macos/TTStation/Tests/TTStationKitTests/Support/FakeTTClient.swift`
- Modify: `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift`
- Test: `ModelsTests.swift`, `BoxViewModelTests.swift`

**Interfaces:**
- Produces: `BoxCatalog { boxMesh: String?, catalogAvailable: Bool, catalogStale: Bool, runsHere: [CatalogEntry], experimental: [CatalogEntry], otherHardware: [CatalogEntry] }` and `CatalogEntry { id, displayName, family, size: String?, software: [String], meshes: [String], neededHardware: [String], availableNow: Bool, statusHere: String }` (Codable, public init, CodingKeys mapping snake_case: `box_mesh, catalog_available, catalog_stale, runs_here, experimental, other_hardware`; entry `display_name, needed_hardware, available_now, status_here`); `TTCommands.catalog(host:) async throws -> BoxCatalog`; `BoxViewModel.catalog: BoxCatalog?`.

- [ ] **Step 1: Write failing decode test** in `ModelsTests.swift` — a JSON with `box_mesh:"p300x2"`, one `runs_here` entry (`available_now:true, status_here:"supported"`), one `experimental`, one `other_hardware` (`needed_hardware:["T3K"]`) → asserts the tiers + field mapping. Add a `BoxViewModelTests` test that `refresh()` populates `catalog` via `FakeTTClient` (unauthed, non-fatal).

- [ ] **Step 2: Run, expect FAIL** — `swift test --filter ModelsTests`.

- [ ] **Step 3: Implement** `BoxCatalog`/`CatalogEntry` (public init + CodingKeys), `TTClient.catalog(host:)` shelling `["--json","catalog","--host",host]` and decoding `BoxCatalog` (match the existing `config`/`status` decode pattern), add to `TTCommands` + `FakeTTClient` (canned `BoxCatalog` with one entry per tier), and load in `BoxViewModel.refresh()`: `catalog = try? await commands.catalog(host: record.hostPort)` (before the pairing gate, non-fatal).

- [ ] **Step 4: Run, expect PASS** — `swift test`.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources macos/TTStation/Tests
git commit -m "feat(macos): BoxCatalog model + TTClient.catalog + VM load"
```

---

## Task 7: Swift 3-tier browser + toolchain messaging + version bump

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/ModelBrowserView.swift`
- Modify: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift` (if the browser needs the workbench link/anchor)
- Modify: `macos/TTStation/AppShell/project.yml` (`MARKETING_VERSION: 0.5.0`)

**Interfaces:**
- Consumes: `BoxViewModel.catalog` (Task 6), `TTTheme`, existing `ModelBrowserView` selection wiring.

- [ ] **Step 1: Render three sections** in `ModelBrowserView` from `box.catalog` (fall back to the existing `/models`-based `ModelRanking` view only when `box.catalog == nil`):
  - **Runs on this box** (`catalog.runsHere`) — prominent; `availableNow` rows get a subtle "ready" mark; these are the ONLY selectable/runnable rows (tapping sets `box.selectedModel` to the model id). Keep search + family grouping.
  - **Experimental** (`catalog.experimental`) — a section header with copy: "Bring these up with the tools — the Workbench (VS Code + tt-vscode-toolkit, Terminal, tt-inference-server) is how you run beyond the paved path." Each row shows its `software` tags; rows are informational (not run-enabled) with a **"Set up in Workbench →"** affordance.
  - **Needs other hardware** (`catalog.otherHardware`) — dimmed; each row labeled with `neededHardware`; same go-beyond framing.
  - Search filters all three. A quiet footer note when `!catalog.catalogAvailable` ("model catalog offline — showing this box's models") or `catalogStale` ("catalog cached").

- [ ] **Step 2: Smart default from runs_here** — ensure `pickDefaultModel`/selection seeds from `catalog.runsHere` (available models) when the catalog is present; otherwise the existing `/models` default path.

- [ ] **Step 3: Workbench link** — the "Set up in Workbench →" affordance either scrolls to the existing Workbench card or is a small button invoking the same launchers; keep it simple (a labeled button near the Experimental header is fine).

- [ ] **Step 4: Version bump** → `MARKETING_VERSION: 0.5.0` in `project.yml`.

- [ ] **Step 5: Build + test** — `cd AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -3` → BUILD SUCCEEDED; `cd macos/TTStation && swift test` all pass.

- [ ] **Step 6: Commit**

```bash
git add macos/TTStation/AppShell
git commit -m "feat(macos): 3-tier catalog browser (experimental + other-hw) + workbench messaging, v0.5.0"
```

---

## Task 8: Docs

**Files:** `macos/README.md`, `CLAUDE.md`

- [ ] **Step 1: Document** the catalog source (CloudFront `compatibility.json`, 24h cache in `tt`, offline-tolerant), the three tiers + Experimental (status-driven), P150 x1–x4, the `tt catalog` command + a `tt --json` contract row, and the "go beyond with the workbench" framing. Bump the version mention to 0.5.0.

- [ ] **Step 2: Commit**

```bash
git add macos/README.md CLAUDE.md
git commit -m "docs: hardware-aware model catalog (experimental + P150 + tt catalog)"
```

---

## Self-review notes

- **Spec coverage:** source/fetch/cache → Tasks 1,3; classify/tiers → Task 2; `tt catalog` → Task 4; P150 → Task 5; Swift decode/VM → Task 6; 3-tier UI + messaging + version → Task 7; docs → Task 8.
- **Type consistency:** `CompatCatalog`/`CompatModel`/`HardwareCompat`/`CompatStatus`/`hw_to_mesh` (Task 1) → `classify`/`BoxCatalog`/`CatalogEntry`/`normalize_key` (Task 2) → consumed by `tt catalog` (Task 4) → mirrored by Swift `BoxCatalog`/`CatalogEntry` (Task 6) → rendered (Task 7). Wire keys snake_case on both sides.
- **TDD vs owner-verified:** pure (Tasks 1,2,5, freshness in 3, decode in 6) TDD; fetch/network (3), CLI wiring (4), SwiftUI (7) owner-verified with fixture fast-paths.
- **Veneer preserved:** all fetch/merge/classify in Rust; the app only decodes `tt --json catalog`.
