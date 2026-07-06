# Window redesign: persistent action bar + elegant, tt-inference-server-focused model list

**Date:** 2026-07-06
**Status:** approved design (in-session, recommendations accepted), ready for implementation plan
**Scope:** `macos/TTStation` (window layout + model browser + de-dup), `crates/libttstation`
(catalog `classify` — focus runs_here on tt-inference-server). Demo-facing polish.

---

## Problem

For the demo, the control-room window has three rough edges:

1. **Run/Stop scroll away.** They live *inside* the Model card, below the model list. Scroll
   down the list (or into Workbench/Serving) and the primary action disappears — you can't
   start/stop from wherever you are.
2. **The model list isn't elegant enough**, and it mixes what the box can actually serve
   (tt-inference-server) with tt-forge/tt-metal entries in the same "Runs on this box" tier.
3. **Serving state is duplicated** — the "Serving \<model\>" line + endpoint URL renders in the
   Model card, and the endpoint again in the Connect card, and the `/serving` list in the
   Serving card. Three places, overlapping.

## Goals

- **Persistent action bar**: Run/Stop/Cancel + the selected/serving model + endpoint are
  always visible in the window, regardless of scroll position.
- **Elegant, focused model list**: the primary list is the **tt-inference-server**-servable
  models (what the box actually runs), rendered cleanly; tt-forge/tt-metal and
  other-hardware models remain as collapsed, de-emphasized asides ("bring up with the tools").
- **De-dup serving state**: one authoritative place for "what's serving + its endpoint" (the
  action bar); the Connect card keeps only the launchers; the Serving card keeps only the
  broader `/serving` list (external/tt-studio endpoints the action bar doesn't cover).
- Keep the veneer: the tt-inference-server focus is decided in Rust (`classify`), not the app.

## Non-goals

- No change to pairing, telemetry, workbench, or the catalog fetch/cache.
- No new model-run capability — experimental/other rows stay informational.
- Not touching the menu-bar popover's compact Run/Stop (`BoxDetailView`) — this is the window.

---

## Component 1 — tt-inference-server focus in `classify` (Rust)

Today `classify` puts every catalog model that's `Supported` on the box mesh into `runs_here`,
regardless of `software`. But the box serves via `run.py` = **tt-inference-server**. So:

- **`runs_here`** = live `/models` (always — those ARE tt-inference-server-servable) ∪ catalog
  models that are `Supported` on the box mesh **AND** whose matching (on-mesh) compatibility
  entry lists `tt-inference-server` in `software`.
- A catalog model `Supported` on the box mesh but **only** via tt-forge/tt-metal (no
  tt-inference-server) drops to **`experimental`** instead of `runs_here` — it's "bring it up
  with the tools," not "run it now."
- `experimental` (Experimental-on-mesh) and `other_hardware` are otherwise unchanged.
- Software matching is case-insensitive and tolerant of the exact string
  (`tt-inference-server`; accept a `tt_inference_server`/`tt-inference-server`/`inference-server`
  fold to be safe — one small normalized compare, documented).

This keeps the primary list honest (only what the box can actually serve now) and de-dupes the
"is this really runnable?" ambiguity at the data layer. `CatalogEntry` already carries
`software`; no wire-shape change. Pure, TDD'd.

Fallback (no catalog) mode is unaffected — it ranks the live `/models` (already TIS) by mesh.

## Component 2 — persistent action bar (Swift)

A new `RunStopBar` view, pinned in the window **outside** the scrolling detail pane
(`WindowRootView`'s detail column becomes `VStack { ScrollView { BoxWorkspaceView } ;
RunStopBar }` so the bar is always on screen). Shown only when a box is selected and paired.

Contents (one row, compact, brand-styled):
- **Status dot** (green serving / amber starting / gray idle / red error) + the model name it
  applies to: the *serving* model if serving, else the *selected* model, else "No model
  selected."
- **Run** (primary, `borderedProminent`) — disabled unless a model is selected and not
  in-flight. **Stop/Cancel** — the same `starting ? Cancel : Stop` logic already in
  `BoxViewModel` (`cancelStart()`/`stop()`, gated by `canStopOrCancel`).
- When serving: the **endpoint URL** (mono, truncated) + a copy button.
- When starting/canceling: the inline "Starting \<model\>… (first run can take a few minutes)"
  / "Canceling…" progress text moves here (from the Model card).

The bar reads its state entirely from `BoxViewModel` (selectedModel, endpoint, status,
starting, cancelling, canStopOrCancel) — no new state. It's the single owner of the
serving/endpoint display.

## Component 3 — model list polish (Swift, `ModelBrowserView`)

- The primary tier header becomes **"Models"** with a subtle "tt-inference-server" caption
  (these are the runnable ones). Rows get a cleaner, more spacious treatment: model name
  (medium weight), a right-aligned size chip (e.g. `8B`), the "ready" dot for `availableNow`,
  and a clear selected state (accent check + subtle selected-row background). Keep family
  grouping with pinned headers but lighten the visual weight.
- **Experimental** (now includes the demoted tt-forge/tt-metal supported models) and **Needs
  other hardware** stay as collapsed, dimmed asides with the existing "bring up with the
  tools" / workbench framing.
- Run/Stop and the serving line are **removed** from the Model card body (they moved to the
  action bar). The Model card becomes just the browser.
- Keep search filtering all tiers.

## Component 4 — de-dup pass

- **Model card**: no longer renders Run/Stop or "Serving \<model\>"/endpoint (→ action bar).
- **Connect card**: unchanged in purpose (Open WebUI / opencode launchers) — it already takes
  the endpoint; it no longer visually competes with a second endpoint display since the Model
  card's is gone. (Its own endpoint line, if any, stays as the launch context.)
- **Serving card**: keep — it's the full `/serving` list (agent + external like tt-studio),
  which is broader than the action bar's single agent endpoint. To avoid echoing the exact
  same agent endpoint the action bar shows, the Serving card renders only when there's
  content beyond the agent's own current endpoint (or clearly labels agent vs external as it
  already does — keep the `external` badge; drop a row that exactly duplicates the action
  bar's agent endpoint). Minimal: keep as-is if it already distinguishes source; the action
  bar owning the agent endpoint is the primary de-dup win.

## Data flow

```
classify (Rust, TIS-focused) → tt catalog --json → BoxCatalog.runsHere (TIS only)
                                                  → .experimental (+ demoted tt-forge/metal)
BoxViewModel {selectedModel, endpoint, status, starting, cancelling}
   → RunStopBar (pinned, always visible)   ← single serving/endpoint owner
   → ModelBrowserView (selection only)      ← primary "Models" list = runsHere
```

## Testing

- **Rust (TDD):** `classify` — a Supported-on-mesh model with `software:["tt-inference-server"]`
  → runs_here; Supported-on-mesh with only `["tt-forge"]`/`["tt-metal"]` → experimental (NOT
  runs_here); a live `/models` entry → runs_here regardless (already TIS); Experimental-on-mesh
  → experimental unchanged. Update existing classify tests for the new runs_here rule.
- **Swift:** `RunStopBar` state logic is drawn from `BoxViewModel` (already unit-tested:
  run/stop/cancel/canStopOrCancel) — the bar is owner-verified (builds, renders). The browser
  changes are visual (owner-verified via xcodebuild). No new pure logic beyond what exists.
- **No-hardware:** mock-box `/models` + a fixture catalog with mixed software → `tt catalog`
  shows the TIS-focused runs_here; click-through the window shows the pinned bar.

## Versioning & docs

- App → **0.6.0** (a visible window redesign).
- `macos/README.md`: note the persistent action bar + tt-inference-server-focused Models list.

## Risks / open questions

- **A box whose only servable models are tt-forge/tt-metal** (no tt-inference-server on its
  mesh): runs_here could be empty while experimental has entries. Acceptable — the action bar
  shows "No model selected," and the Experimental aside + workbench framing tells the story.
  The live `/models` safety net means anything the agent actually reports stays runnable.
- **`software` field absent/empty on a catalog entry** that's Supported-on-mesh: treat "no
  software listed" as NOT tt-inference-server (→ experimental), so the primary list stays
  strictly what we can vouch for. Documented; live `/models` still rescues truly servable ones.
- **Pinned bar height** on small windows: keep it single-row, compact; the scroll pane takes
  the remaining space (`minHeight` unchanged).
