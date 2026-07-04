# TTStation window surface (Variant A + sidebar) — design

**Date:** 2026-07-04
**Status:** approved (brainstorming), pending implementation plan
**Context:** The whole app lives in one `MenuBarExtra(.window)` popover fixed at 340pt wide with
the model browser capped at 260pt tall (`ModelPickerView`). The searchable/grouped browser is
cramped. This adds a bigger surface **without losing the menu-bar entry point.**

## Goal

Keep the menu-bar popover as the glanceable entry point, and add a **dedicated, resizable
`Window`** (sidebar + detail) for the room-hungry work: the model browser, the `/serving` list,
Run, and Connect. Both scenes bind to the same `AppModel`/`BoxViewModel` — additive, no logic
reimplemented.

## The two scenes

### 1. `MenuBarExtra(.window)` popover — trimmed (`MenuContentView`, `BoxDetailView`)
- Discovered boxes list with status dots; select; scan/refresh (unchanged).
- Selected box, compact: status line (`serving <model>` / `idle` / `starting`), **Run** (runs the
  current/smart-default model), **Stop**, **Copy endpoint**, **Connect ▾** (Open WebUI / opencode).
- **New:** an **"⤢ Open TTStation window"** button (`openWindow(id: "main")`).
- Add host… / Quit (unchanged).
- **Removed from the popover:** the `ModelPickerView` browser. Changing the model now happens in
  the window; the popover Runs whatever model is currently selected (smart default, or the last
  pick made in the window). This is what relieves the popover crunch.

### 2. `Window("TTStation", id: "main")` — resizable workspace (new)
A `NavigationSplitView`:
- **Sidebar (`BoxSidebarView`, new):** the boxes list (status dot, name, chips), selectable;
  scan/refresh; **Add host…**. Selection is bound to `AppModel.selectedHostPort`, so it stays in
  sync with the popover (pick a box in either surface, both reflect it).
- **Detail (`BoxWorkspaceView`, new):** the selected box's full workspace, top to bottom:
  - **Pairing** when unpaired (the existing two-step **Start pairing → enter code** flow).
  - **Model browser:** the existing `ModelPickerView`, given room — no 260pt cap, wider column,
    search + family-grouped sections, device badges, smart-default highlighted.
  - **Run bar:** primary **Run `<model>`** + **Stop**, reflecting HIG run states
    (idle → amber "starting" → green "serving").
  - **`/serving` list:** every live `tt-inference-server` `/v1` on the box (agent + external, with
    the source/tt-studio badge), each with **Copy** and **Connect** (Open WebUI / opencode).
  - **Endpoint** row (base_url) with Copy.

## Architecture / reuse

- New `Window` scene declared alongside the existing `MenuBarExtra` in `TTStationApp`. Opened via
  `@Environment(\.openWindow)` from the popover's button (and it can also be reopened from the
  standard Window menu the `.regular` policy provides).
- **Shared state:** the same `AppModel` instance backs both scenes (it already owns `boxes`,
  `selectedHostPort`, scan). The window's sidebar and the popover's list are two views of the same
  `boxes`; selection is the same `selectedHostPort`.
- **`ModelPickerView` gains a height option:** add a parameter (e.g. `maxListHeight: CGFloat?`,
  `nil` = uncapped) so the window renders it full-height while any compact reuse stays bounded.
  No behavior change to its pure grouping/filtering.
- **`LaunchController`, `BoxViewModel`, `ModelDefaults`, launchers** are reused unchanged — the
  window is new *views* over existing logic.
- **Activation policy (the one non-trivial mechanic):** the app is `LSUIElement` (accessory, no
  Dock icon). To present a normal, focused window, transition `NSApp.setActivationPolicy(.regular)`
  when the window opens and back to `.accessory` when it closes (tracked via the window's
  appear/disappear or an `NSApplicationDelegate`/window-count check). This gives a real window +
  Dock icon *while open*, without permanently changing the menu-bar-only nature.

## Data flow

Popover and window both render `AppModel`. Selecting a box in either sets `selectedHostPort`;
both update. Picking a model in the window sets `box.selectedModel`; the popover's Run uses it.
Run/Stop/refresh/pair/launch all go through the existing `BoxViewModel`/`LaunchController`
methods — the window adds no new backend calls.

## Error handling

Unchanged model: `BoxViewModel.errorText` and the launchers' error strings surface inline in
whichever surface triggered them (popover or window detail). The window shows the same honest
empty/error states (no box selected, unpaired, launch failures, timeouts).

## Testing

- **Pure logic** (`ModelDefaults` grouping/defaults, launchers' builders) is already unit-tested;
  the `maxListHeight` parameter on `ModelPickerView` is a rendering-only change.
- **Views** (the new window scenes) follow the app's existing convention: verified by building
  (`swift test` for the package must stay green; `xcodebuild` for the app target must succeed) and
  an owner-run click-through — open the window, browse/pick a model, Run, watch states, Connect,
  and confirm sidebar↔popover selection stays in sync.

## Deferred (not now)

- Full "window-first" demotion of the menu bar (Variant B) — the menu bar stays the entry point.
- Multi-window / per-box windows — one shared `main` window focused on the selected box.
- Persisting window size/position beyond SwiftUI's default scene restoration.
- Toolbar customization, search-scope filters, or a models-grid layout — start with the grouped
  list given room.
