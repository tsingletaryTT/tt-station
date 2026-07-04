# TTStation Window Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a resizable `Window` (sidebar + detail) to the TTStation menu-bar app for the room-hungry model browser / `/serving` / Connect, while the `MenuBarExtra` popover stays as a trimmed glanceable entry point — both bound to the same `AppModel`.

**Architecture:** A new SwiftUI `Window(id: "main")` scene declared alongside the existing `MenuBarExtra`, sharing the one `AppModel` instance. The window is a `NavigationSplitView` (boxes sidebar + selected-box workspace). The workspace reuses the current full box detail with the model browser uncapped; the popover's detail is trimmed to quick actions + an "Open window" button. Selection syncs through `AppModel.selectedHostPort`.

**Tech Stack:** Swift 5, SwiftUI (`Window`, `NavigationSplitView`, `@Environment(\.openWindow)`), AppKit (`NSApp.setActivationPolicy`), Observation. Built with XcodeGen + `xcodebuild`; logic package unchanged.

## Global Constraints

- macOS 14; Swift 5 language mode; all code under `macos/TTStation/`.
- Additive only: the `MenuBarExtra` scene, `AppModel`, `BoxViewModel`, `LaunchController`, `ModelDefaults`, and the launchers are reused unchanged except where a task says otherwise.
- One shared `AppModel` backs both scenes; selection is `AppModel.selectedHostPort` (a `String?` = `BoxRecord.hostPort`).
- Window scene id is exactly `"main"`; opened via `@Environment(\.openWindow)`.
- Activation policy: flip `NSApp.setActivationPolicy(.regular)` while the window is open and `.accessory` when it closes (the app is `LSUIElement`).
- The popover no longer hosts the model browser or the `/serving` list — those move to the window. The popover Runs the current/smart-default `selectedModel` (set by `BoxViewModel.loadModels()` / `refresh()`).
- Verification is build (`swift test` stays green; app `xcodebuild` succeeds) + owner click-through; SwiftUI views are not unit-tested (matches the existing app).

---

## File Structure

```
macos/TTStation/AppShell/Sources/
  ModelPickerView.swift    # MODIFY: add optional uncapped height
  BoxWorkspaceView.swift   # NEW: window detail (full box workspace, uncapped browser)
  BoxSidebarView.swift     # NEW: window sidebar (boxes list)
  TTStationApp.swift       # MODIFY: add Window scene + WindowRootView (+ activation policy)
  BoxDetailView.swift      # MODIFY: trim to compact popover detail + "Open window" button
```

Build the app target after each task:
```
cd macos/TTStation/AppShell && xcodegen generate && \
xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | grep -iE "error:|BUILD SUCCEEDED|BUILD FAILED"
```

---

## Task 1: `ModelPickerView` — optional uncapped height

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/ModelPickerView.swift`

**Interfaces:**
- Consumes: nothing new.
- Produces: `ModelPickerView(box:)` gains a second parameter `maxListHeight: CGFloat? = 260`. Existing callers (no argument) keep the 260pt cap; the window passes `nil` for uncapped.

- [ ] **Step 1: Add the parameter and apply it**

In `ModelPickerView.swift`, add the stored property (right after `@Bindable var box: BoxViewModel`):
```swift
    /// Max height of the scrollable model list. `nil` = uncapped (used in the
    /// resizable window); the default keeps the compact popover bounded.
    var maxListHeight: CGFloat? = 260
```
Change the list frame line from:
```swift
                .frame(maxHeight: 260)
```
to:
```swift
                .frame(maxHeight: maxListHeight)
```
(`.frame(maxHeight: nil)` imposes no height limit, which is exactly what the window wants.)

- [ ] **Step 2: Build to verify**

Run the app build command (see File Structure).
Expected: `** BUILD SUCCEEDED **`. Existing popover behavior is unchanged (default 260).

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/ModelPickerView.swift
git commit -m "feat(macos): ModelPickerView optional uncapped list height"
```

---

## Task 2: Window subviews — `BoxWorkspaceView` + `BoxSidebarView`

**Files:**
- Create: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift`
- Create: `macos/TTStation/AppShell/Sources/BoxSidebarView.swift`

**Interfaces:**
- Consumes: `BoxViewModel`, `AppModel`, `ModelPickerView(box:maxListHeight:)`, `LaunchController`, `ManualHostSheet`, `ServingEntry` (all existing).
- Produces: `struct BoxWorkspaceView: View { @Bindable var box: BoxViewModel }` (the window's detail pane) and `struct BoxSidebarView: View { @Bindable var model: AppModel }` (the window's sidebar). Both compile now; wired into the window in Task 3.

- [ ] **Step 1: Create `BoxWorkspaceView` (the full box detail, uncapped browser)**

`BoxWorkspaceView` is the window's roomy detail. Its body is the **current `BoxDetailView` body copied verbatim** — the whole `VStack` (pairing branch, model browser, Run/Stop, `starting` message, serving-line + endpoint copy, Connect buttons + launcher errors, the `/serving` list, and the trailing `errorText`) — with exactly one change: the `ModelPickerView(box: box)` call becomes `ModelPickerView(box: box, maxListHeight: nil)` so the browser fills the window. Keep its `@State private var code`, `@State private var launcher`, and the `.task { if box.models.isEmpty { await box.loadModels() } }` on the picker.

Create `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift`:
```swift
import SwiftUI
import TTStationKit

/// The window's detail pane: the full box workspace with the model browser
/// given room (uncapped). Identical in behavior to the pre-window
/// `BoxDetailView`; the popover keeps only a trimmed version (see BoxDetailView).
struct BoxWorkspaceView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""
    @State private var launcher = LaunchController()

    var body: some View {
        // PASTE the current BoxDetailView `VStack(alignment: .leading, spacing: 8) { … }`
        // body here verbatim, changing only the ModelPickerView call to:
        //     ModelPickerView(box: box, maxListHeight: nil)
        //         .task { if box.models.isEmpty { await box.loadModels() } }
        // (Everything else — pairing branch, Run/Stop, starting, endpoint+copy,
        //  Connect, /serving list, errorText — is copied unchanged.)
    }
}
```
Do the copy from the current `macos/TTStation/AppShell/Sources/BoxDetailView.swift` (read it first). The result must compile with no other changes.

- [ ] **Step 2: Create `BoxSidebarView` (window sidebar)**

`macos/TTStation/AppShell/Sources/BoxSidebarView.swift`:
```swift
import SwiftUI
import TTStationKit

/// The window's sidebar: the discovered boxes as a selectable List, kept in
/// sync with the popover via `AppModel.selectedHostPort`. Refresh + Add host
/// live in a bottom bar.
struct BoxSidebarView: View {
    @Bindable var model: AppModel
    @State private var showAddHost = false

    var body: some View {
        List(selection: Binding(
            get: { model.selectedHostPort },
            set: { model.selectedHostPort = $0 }
        )) {
            Section("Boxes") {
                ForEach(model.boxes) { box in
                    HStack(spacing: 6) {
                        Image(systemName: "circle.fill")
                            .font(.system(size: 7))
                            .foregroundStyle((box.status?.isServing ?? false) ? Color.green : Color.secondary)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(box.record.name)
                            Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .tag(box.id as String?)
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button { Task { await model.scan() } } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                Spacer()
                Button("Add host…") { showAddHost = true }
            }
            .controlSize(.small)
            .padding(8)
        }
        .sheet(isPresented: $showAddHost) {
            ManualHostSheet { host in
                model.addManualHost(host)
                Task { await model.scan() }
            }
        }
    }
}
```

- [ ] **Step 3: Build to verify both compile**

Run the app build command. Expected: `** BUILD SUCCEEDED **` (the new views are unused so far).

- [ ] **Step 4: Commit**

```bash
git add macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift macos/TTStation/AppShell/Sources/BoxSidebarView.swift
git commit -m "feat(macos): window subviews — BoxWorkspaceView (uncapped) + BoxSidebarView"
```

---

## Task 3: Window scene + activation policy + open-window wiring

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/TTStationApp.swift`

**Interfaces:**
- Consumes: `AppModel`, `BoxSidebarView(model:)`, `BoxWorkspaceView(box:)` (Task 2), `MenuContentView`.
- Produces: a `Window("TTStation", id: "main")` scene backed by `WindowRootView(model:)`; the app becomes `.regular` while the window is open and `.accessory` when closed.

- [ ] **Step 1: Add `WindowRootView` and the `Window` scene**

Replace the contents of `macos/TTStation/AppShell/Sources/TTStationApp.swift` with:
```swift
import AppKit
import SwiftUI
import TTStationKit

@main
struct TTStationApp: App {
    @State private var model: AppModel

    init() {
        let registry = HostRegistry(store: UserDefaults.standard)
        let client = TTClient(runner: RealProcessRunner(locator: .standard()))
        let discovery = MDNSDiscoveryService(client: client, registry: registry)
        _model = State(initialValue: AppModel(commands: client, discovery: discovery, registry: registry))
    }

    var body: some Scene {
        MenuBarExtra("TTStation", image: "MenuBarIcon") {
            MenuContentView(model: model)
                .frame(width: 340)
        }
        .menuBarExtraStyle(.window)

        Window("TTStation", id: "main") {
            WindowRootView(model: model)
        }
        .windowResizability(.contentMinSize)
    }
}

/// Root of the resizable window: boxes sidebar + selected-box workspace.
/// Flips the app to a normal (`.regular`) activation policy while open so a
/// menu-bar-only (`LSUIElement`) app can present a focused window, and back to
/// `.accessory` on close so the Dock icon doesn't linger.
struct WindowRootView: View {
    @Bindable var model: AppModel

    var body: some View {
        NavigationSplitView {
            BoxSidebarView(model: model)
                .navigationSplitViewColumnWidth(min: 200, ideal: 240)
        } detail: {
            if let box = model.selectedBox {
                ScrollView { BoxWorkspaceView(box: box).padding() }
            } else {
                ContentUnavailableView("Select a box", systemImage: "cpu")
            }
        }
        .frame(minWidth: 680, minHeight: 480)
        .task { await model.scan() }
        .onAppear {
            NSApp.setActivationPolicy(.regular)
            NSApp.activate(ignoringOtherApps: true)
        }
        .onDisappear {
            NSApp.setActivationPolicy(.accessory)
        }
    }
}
```

- [ ] **Step 2: Build to verify**

Run the app build command. Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 3: Launch-check the window opens**

Build, locate, and launch the app, then open the window via a tiny AppleScript menu-bar-free check isn't possible; instead verify the scene exists by confirming the build embeds a `main` window scene (the `openWindow` button added in Task 4 is what a human clicks). For now just confirm `** BUILD SUCCEEDED **` and that `WindowRootView`/`BoxSidebarView`/`BoxWorkspaceView` are all referenced (no dead-code warning). No commit gate beyond build.

- [ ] **Step 4: Commit**

```bash
git add macos/TTStation/AppShell/Sources/TTStationApp.swift
git commit -m "feat(macos): resizable Window scene (sidebar+workspace) with activation-policy flip"
```

---

## Task 4: Trim the popover `BoxDetailView` + add "Open window"

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/BoxDetailView.swift`

**Interfaces:**
- Consumes: `BoxViewModel`, `LaunchController`, `@Environment(\.openWindow)`.
- Produces: a compact popover detail — pairing flow (unchanged), quick Run/Stop, serving-line + endpoint copy + Connect, and an "Open TTStation window" button. The model browser and `/serving` list are gone from the popover (they live in the window now).

- [ ] **Step 1: Replace `BoxDetailView` with the trimmed version**

Replace the contents of `macos/TTStation/AppShell/Sources/BoxDetailView.swift` with:
```swift
import SwiftUI
import TTStationKit

/// Compact popover detail: pairing + quick actions for the selected box, plus
/// an "Open window" affordance. Model browsing and the full `/serving` list
/// moved to the resizable window (`BoxWorkspaceView`); this view Runs the
/// current/smart-default `selectedModel`.
struct BoxDetailView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""
    @State private var launcher = LaunchController()
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !box.isPaired {
                if box.pairId == nil {
                    Text("Pair to control this box.").font(.caption)
                    HStack {
                        Button("Start pairing") { Task { await box.startPairing() } }
                            .disabled(box.inFlight)
                        if box.inFlight { ProgressView().scaleEffect(0.6) }
                    }
                } else {
                    Text("Enter the 6-digit code shown on the box:").font(.caption)
                    HStack {
                        TextField("000000", text: $code)
                            .textFieldStyle(.roundedBorder).frame(width: 100)
                        Button("Pair") { Task { await box.completePairing(code: code) } }
                            .disabled(code.count != 6 || box.inFlight)
                        Button("Start over") { box.cancelPairing() }
                            .disabled(box.inFlight)
                        if box.inFlight { ProgressView().scaleEffect(0.6) }
                    }
                }
            } else {
                HStack(spacing: 8) {
                    Button { Task { await box.run() } } label: {
                        Label("Run", systemImage: "play.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(box.selectedModel == nil || box.inFlight)
                    .help("Run the selected model. Browse/choose models in the window.")

                    Button(role: .destructive) { Task { await box.stop() } } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.bordered)
                    .disabled(box.inFlight)
                    .help("Stop the model currently serving on this box.")

                    if box.inFlight { ProgressView().scaleEffect(0.6) }
                }
                .controlSize(.small)
                // Keep the smart-default `selectedModel` populated even though
                // the browser now lives in the window, so Run is enabled here.
                .task { if box.models.isEmpty { await box.loadModels() } }

                if box.starting {
                    HStack(spacing: 6) {
                        ProgressView().scaleEffect(0.6)
                        Text("Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }

                if let ep = box.endpoint {
                    HStack(spacing: 4) {
                        Image(systemName: "circle.fill").font(.system(size: 7)).foregroundStyle(.green)
                        Text("Serving \(ep.model)").font(.caption.weight(.semibold))
                            .lineLimit(1).truncationMode(.middle)
                    }
                    HStack {
                        Text(ep.baseURL).font(.system(.caption, design: .monospaced))
                            .lineLimit(1).truncationMode(.middle)
                        Button {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(ep.baseURL, forType: .string)
                        } label: { Image(systemName: "doc.on.doc") }
                        .buttonStyle(.borderless).help("Copy endpoint URL")
                    }
                    HStack(spacing: 8) {
                        Text("Connect:").font(.caption).foregroundStyle(.secondary)
                        Button { Task { await launcher.openWebUI(endpoint: ep) } } label: {
                            Label("Open Web UI", systemImage: "globe")
                        }
                        .disabled(launcher.isLaunchingWebUI)
                        Button { Task { await launcher.openInOpenCode(endpoint: ep) } } label: {
                            Label("opencode", systemImage: "terminal")
                        }
                        .disabled(launcher.isLaunchingOpenCode)
                        if launcher.isLaunchingWebUI || launcher.isLaunchingOpenCode {
                            ProgressView().scaleEffect(0.6)
                        }
                    }
                    if let e = launcher.webUIError ?? launcher.openCodeError {
                        Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                    }
                }

                Button { openWindow(id: "main") } label: {
                    Label("Open TTStation window", systemImage: "macwindow")
                }
                .controlSize(.small)
            }
            if let err = box.errorText {
                Text(err).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
    }
}
```

- [ ] **Step 2: Build to verify**

Run the app build command. Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/BoxDetailView.swift
git commit -m "feat(macos): trim popover to quick actions + Open-window button (browser/serving move to window)"
```

---

## Task 5: Build-verify + owner click-through

**Files:** none (verification).

- [ ] **Step 1: Full package tests still green**

Run: `cd macos/TTStation && swift test 2>&1 | tail -3`
Expected: all tests pass (the package layer is untouched by this feature).

- [ ] **Step 2: App builds clean**

Run the app build command. Expected: `** BUILD SUCCEEDED **`, no warnings from our sources.

- [ ] **Step 3: Owner click-through (box must be discoverable)**

Launch the built app. From the menu-bar popover: confirm the box appears and pairing/quick-run still work. Click **Open TTStation window** → a resizable window opens and comes to the front (Dock icon appears). In the window: the **sidebar** lists boxes; selecting one updates the **workspace**; the **model browser** shows full-height (no 260pt cap) and is searchable; **Run** works; the **`/serving`** list and **Connect** buttons work; the endpoint copies. Selecting a box in the sidebar updates the popover's selection too (and vice-versa). Close the window → the Dock icon goes away (back to `.accessory`).
Capture anything broken and fix in the owning task.

- [ ] **Step 4: Commit any doc update (if the README/CLAUDE.md notes the window)**

Optional: note the new window surface in `macos/README.md`. Commit if changed.

---

## Self-Review Notes

- **Spec coverage:** trimmed popover (Task 4) ✓; window scene + sidebar + workspace detail (Tasks 2–3) ✓; uncapped model browser via `maxListHeight: nil` (Tasks 1–2) ✓; `/serving` + Connect + endpoint in the workspace (Task 2, copied from current detail) ✓; selection sync via `selectedHostPort` (Task 2 sidebar binding + Task 3) ✓; activation-policy flip (Task 3) ✓; shared `AppModel` (Task 3) ✓; deferred items (window-first, per-box windows, size persistence) correctly absent ✓.
- **Placeholder scan:** the one "paste the current body verbatim" instruction (Task 2, `BoxWorkspaceView`) references concrete existing code the implementer reads and names the single change (`maxListHeight: nil`) — not a vague placeholder. All other steps show complete code.
- **Type consistency:** `AppModel.selectedHostPort: String?`, `AppModel.selectedBox`, `BoxViewModel.{status,isServing via status,record.name,record.chips,selectedModel,endpoint,serving,starting,run,stop,loadModels,startPairing,completePairing,cancelPairing}`, `ModelPickerView(box:maxListHeight:)`, window id `"main"`, and `LaunchController.{openWebUI,openInOpenCode,isLaunchingWebUI,isLaunchingOpenCode,webUIError,openCodeError}` are used identically across tasks and match the current sources.
- **Blind spot to verify on the Mac (Task 5):** `@Environment(\.openWindow)` firing from inside a `MenuBarExtra`, and `.onDisappear` reliably restoring `.accessory` on window close. If `openWindow` from the popover proves flaky, fall back to `NSApp.sendAction`/a menu command — but verify first.
