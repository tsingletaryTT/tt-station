# Hardware-aware model catalog — experimental tier, P150 configs, toolchain messaging

**Date:** 2026-07-05
**Status:** approved design (in-session), ready for implementation plan
**Scope:** `crates/tt` (fetch + cache + merge + classify + `tt catalog` command),
`crates/libttstation` (catalog model types + HW_MAP), `crates/tt-station-agentd`
(`detect_device_mesh` P150 x1–x4), `macos/TTStation` (3-tier browser + workbench
messaging), `crates/mock-box`/fixtures (no-hardware test path).

---

## Problem

The macOS model browser shows only the box's **stable, vLLM-servable** models (from the
box's `model_spec.json` via `/models`), split into two tiers ("Runs on this box" / "Needs
other hardware") by the detected mesh. Three gaps:

1. **No experimental models.** Tenstorrent supports far more models (tt-forge, tt-metal,
   inference-server) than the box auto-runs; many are `Experimental`. The app never shows them.
2. **P150 configs missing.** Mesh detection only maps 4×P150 → `p150x4`; a 1/2/3-card P150
   box reports no mesh, and P150 models can't rank.
3. **Dead-end framing.** "Needs other hardware" is a wall. It should instead teach that
   **more models run once you use the full toolchain** (tt-vscode-toolkit, terminal,
   tt-inference-server) — the tools are how you go beyond the paved path.

## Goals

- Surface a **richer catalog** merged from the public Tenstorrent compatibility catalog +
  the box's live `/models`, classified **for this box's mesh** into three tiers.
- A first-class **Experimental** tier, driven by the catalog's real `status` field.
- **P150 x1–x4** mesh detection + ranking.
- **"Unlock more with the tools"** messaging on the Experimental / other-hardware tiers,
  linking to the Workbench.
- Keep the veneer: **all fetch/merge/classify lives in Rust (`tt`)**; the app renders.

## Non-goals

- No authenticated console (`console.tenstorrent.com`) integration — no browser MCP; the
  catalog is the **public, unauthenticated** CloudFront JSON.
- No new Swift network I/O (the telemetry WebSocket stays the only Swift I/O exception).
- No model *bring-up automation* — the app points users at the workbench tools; it doesn't
  script tt-forge/tt-metal builds.
- No change to how the box actually serves (`run.py`/`/run`).

---

## Data source

The public Tenstorrent compatibility catalog (the same one `~/code/tt-model-runner` uses,
per its `tt-model-data.url`):

```
https://d1oi7xemha0dsy.cloudfront.net/data/compatibility.json
```

Unauthenticated, ~1 MB, ~222 models, refreshed upstream ~daily. Shape:

```jsonc
{
  "metadata": { /* generation timestamp, source, schema version */ },
  "models": [
    {
      "id": "qwen3-8b",
      "display_name": "Qwen3-8B",
      "family": "Qwen",
      "tasks": ["Text Generation"],
      "model_size": "8B", "model_size_num": 8.0e9,
      "model_description": "…",
      "compatibility": [
        { "hardware": "Quietbox 2", "chip_set": "Blackhole",
          "hardware_family": "Quietbox", "status": "Supported",
          "software": ["tt-inference-server"] },
        { "hardware": "p150", "chip_set": "Blackhole",
          "hardware_family": "Card", "status": "Experimental",
          "software": ["tt-forge"] }
      ]
    }
  ]
}
```

`status` ∈ {`Supported`, `Experimental`, `Not Supported`}. `software` ∈ {`tt-forge`,
`tt-inference-server`, `tt-metal`}.

---

## Component 1 — catalog fetch + cache (`tt`, Rust)

- `tt` fetches `compatibility.json` and caches it at
  `~/.cache/tt-station/compatibility.json` with a **24 h TTL** (mirrors tt-model-runner:
  read fresh cache → else fetch → write cache). `TT_CONFIG_DIR`/an env override for the
  cache dir is honored if the crate already has that convention.
- **Offline-tolerant:** on fetch failure, fall back to a stale cache if present; if no
  cache at all, degrade to "catalog unavailable" (the command still returns the box's live
  `/models` as the runs-here tier — see Component 3).
- A `--refresh` flag on `tt catalog` bypasses the TTL. A `--catalog-file <path>` flag (or
  `TT_CATALOG_FILE` env) points at a local fixture — used by tests and offline dev.
- Fetch is `reqwest::blocking` (the crate already uses it for `manual_status_fetch`), no new
  async surface.

## Component 2 — catalog types + `HW_MAP` (`libttstation`, pure)

- Types mirroring the JSON (serde): `CompatCatalog { models: Vec<CompatModel> }`,
  `CompatModel { id, display_name, family, tasks, model_size, model_size_num,
  model_description, compatibility: Vec<HardwareCompat> }`,
  `HardwareCompat { hardware, chip_set, hardware_family, status, software }`,
  `enum CompatStatus { Supported, Experimental, NotSupported, Other(String) }` (tolerant —
  unknown status strings don't fail the parse).
- **`HW_MAP`**: catalog `hardware` (lowercased) → mesh label, seeded from tt-model-runner's
  map and extended for our mesh vocabulary:
  `n150→N150, n300→N300, p100→P100, p150→P150, p300→P300, galaxy→T3K,
  quietbox→P150X4, "quietbox 2"→P300X2, loudbox→P300X2, "2 x quietbox"→P150X8,
  "2 x galaxy"→GALAXY, "4 x galaxy"→GALAXY, quad_galaxy→GALAXY`. Unmapped hardware →
  passthrough of the uppercased raw string (so nothing is silently dropped). Pure + tested.

## Component 3 — merge + classify (`tt`, pure core)

Pure function `classify(catalog, live_models, box_mesh) -> BoxCatalog`:

- **`runs_here`**: every model in the box's live `/models` (always — it's literally
  servable now), UNIONED with catalog models that have a compatibility entry whose
  `HW_MAP(hardware)` case-insensitively equals `box_mesh` AND `status == Supported`.
- **`experimental`**: catalog models (not already in `runs_here`) with a compatibility entry
  mapping to `box_mesh` AND `status == Experimental`.
- **`other_hardware`**: remaining catalog models that are `Supported`/`Experimental` on some
  OTHER mesh — annotated with the distinct mapped meshes they need (e.g. `["T3K","P150X4"]`).
  Models that are `Not Supported` everywhere (or map nowhere) are omitted.
- **Merge key (live ↔ catalog):** normalize both sides to a comparable key —
  lowercase, take the segment after the last `/` (HF repo → model id), replace
  `[._ ]`/spaces with `-`, collapse repeats. A live model that matches a catalog entry is a
  single enriched row (catalog metadata + "available now"); a live model with no catalog
  match still appears in `runs_here` (un-enriched — never hidden). Fuzzy-match false
  negatives only cost enrichment, never availability; a rare false dup is low-harm.
- `box_mesh == None` (mesh unknown / older agent): no basis to split → everything the
  catalog knows goes to a single "all models" list (or `runs_here` degrades to just the live
  `/models`); no experimental/other split. Documented, never a crash.
- Result carries `catalog_available: bool` and `catalog_stale: bool` so the app can show a
  quiet "catalog offline / cached" note.

`BoxCatalog` (the `tt catalog --json` output):
```jsonc
{ "box_mesh": "p300x2",           // or null
  "catalog_available": true, "catalog_stale": false,
  "runs_here":      [CatalogEntry],
  "experimental":   [CatalogEntry],
  "other_hardware": [CatalogEntry] }
```
`CatalogEntry { id, display_name, family, size, software: [String],
meshes: [String] /* mapped meshes it runs on */, needed_hardware: [String] /* for other tier */,
available_now: bool /* in live /models */, status_here: "supported"|"experimental"|"other" }`.

## Component 4 — `tt catalog` CLI command

- `tt --json catalog --host <h> [--refresh] [--catalog-file <p>]` → the `BoxCatalog` JSON.
  Resolves the box mesh from `tt status`/discover (`device_mesh`), fetches the box's live
  `/models`, fetches/loads the compat catalog, runs `classify`, prints.
- Human (non-`--json`) form: a readable three-section list.
- Reuses the existing unauthed `/models` + `/status` paths; no auth needed (catalog is public,
  `/models` + `/config` + `/status` are unauthed).

## Component 5 — agent P150 x1–x4 (`tt-station-agentd`)

Extend `detect_device_mesh`'s table:
```
("p150"|"p150c", 1) => "p150"
("p150"|"p150c", 2) => "p150x2"
("p150"|"p150c", 3) => "p150x3"
("p150"|"p150c", 4) => "p150x4"   // unchanged
```
Add unit tests for each count. Everything downstream (`/status`, mDNS TXT, `tt`) already
carries `device_mesh` unchanged.

## Component 6 — Swift 3-tier browser + toolchain messaging

- `TTClient.catalog(host:) async throws -> BoxCatalog` (shells `tt --json catalog --host <h>`),
  decoded into Swift `BoxCatalog`/`CatalogEntry` (Codable). Added to `TTCommands` + FakeTTClient.
- `BoxViewModel` loads it (unauthed, non-fatal, in `refresh()`), exposes the three tiers.
- `ModelBrowserView` renders three sections:
  1. **Runs on this box** (prominent; `available_now` rows get a subtle "ready" mark).
  2. **Experimental** — header copy: *"Bring these up with the tools"* + a short line that
     the workbench (VS Code + tt-vscode-toolkit, Terminal, tt-inference-server) is how you
     run beyond the paved path; each row shows its `software` tags. A **"Set up in
     Workbench →"** affordance scrolls/links to the Workbench card.
  3. **Needs other hardware** — dimmed, labeled with `needed_hardware`; same
     "go-beyond-with-the-tools" framing (these teach what hardware unlocks them).
  Search filters all three. Selection/Run still only enabled for `runs_here` models (the box
  can actually serve those); experimental/other rows are informational + point to the tools.
- Smart-default (`pickDefaultModel`) selects from `runs_here`.
- Graceful states: `catalog_available == false` → show only `runs_here` (live models) with a
  quiet "model catalog offline" note; `catalog_stale` → a subtle "cached" hint.

## Testing

- **Rust (pure, TDD):** `HW_MAP` mappings (incl. multi-unit + passthrough); `classify` tier
  assignment (supported→runs_here, experimental→experimental, other-mesh→other_hardware,
  not-supported→omitted; live-models always in runs_here; box_mesh None degrade); merge-key
  normalization (HF repo ↔ catalog id). Fixture `compatibility.json` in the test tree.
- **Rust (owner-verified):** the fetch/cache path (fixture fast-path via `--catalog-file`);
  `detect_device_mesh` P150 x1–x4 (pure, TDD).
- **Swift (TDD/decode):** `BoxCatalog` decode; tier rendering is owner-verified (build).
- **No-hardware e2e:** `tt catalog --host <mock> --catalog-file <fixture>` against mock-box's
  `/models` — full tiering without hardware or network.

## Versioning & docs

- App version bump (0.5.0 — a substantial browser change).
- `macos/README.md` + `CLAUDE.md`: the catalog source, the three tiers, P150 configs, the
  `tt catalog` command + `tt --json` contract row, and the offline/stale behavior.

## Risks / open questions

- **Live↔catalog merge fuzziness.** HF-repo vs catalog-id naming won't always match; the
  fallback (live models always shown, unmatched = un-enriched) makes this cosmetic, not
  functional. If mismatches are common in practice, add a small alias map later.
- **Catalog hardware granularity for P150.** The catalog expresses P150 by card/box
  (`p150`, `quietbox`→P150X4); it may not distinguish P150X2/X3. A 2–3 card P150 box will
  match `P150`-card entries but not box-level ones — acceptable; documented, not wrong.
- **Catalog schema drift.** Tolerant parsing (`CompatStatus::Other`, passthrough HW_MAP,
  optional fields) keeps an upstream change from breaking the command.
- **CloudFront availability.** Cache + stale-fallback + graceful "unavailable" keep the app
  working offline / if the URL moves.
