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
        CLIInstaller.runFirstRunIfNeeded()
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContentView(model: model)
                .frame(width: 340)
                .tint(TTTheme.teal)
        } label: {
            MenuBarLabel(model: model)
        }
        .menuBarExtraStyle(.window)

        Window("TTStation", id: "main") {
            WindowRootView(model: model)
                .tint(TTTheme.teal)
        }
        .windowResizability(.contentMinSize)
    }
}

/// The `MenuBarExtra` label: the app's tray icon, with a small colored dot
/// badge overlaid in the top-trailing corner whenever any discovered box is
/// actively `.serving` a model — so activity at a glance doesn't require
/// opening the popover ("highlight running models in the toolbar", Task 2).
///
/// `@Bindable` (not a plain `let`) so this view's own `body` is what reads
/// `model.servingCount` — that's what makes SwiftUI's Observation machinery
/// register the dependency and re-evaluate the label as boxes start/stop
/// serving. `MenuBarExtra`'s label closure itself is not a tracked View
/// context, so reading the count directly in `TTStationApp.body` (rather
/// than delegating to a real child `View`) would not have re-rendered on
/// change — this indirection is required, not just tidiness.
///
/// The icon stays the existing template image (see
/// `MenuBarIcon.imageset/Contents.json`'s `template-rendering-intent`) so it
/// keeps tinting correctly for light/dark menu bars; the badge is a small
/// opaque `Circle` layered on top and is deliberately *not* part of the
/// template, so it always renders in its serving color regardless of menu
/// bar appearance. `servingCount` (not `.starting`) gates it — a box mid
/// spin-up isn't "serving" yet, so the badge only lights once a model is
/// actually up.
struct MenuBarLabel: View {
    @Bindable var model: AppModel

    var body: some View {
        Image("MenuBarIcon")
            .overlay(alignment: .topTrailing) {
                if model.servingCount > 0 {
                    Circle()
                        .fill(TTTheme.statusServing)
                        .frame(width: 6, height: 6)
                        // Nudge slightly past the icon's own bounds so the
                        // badge reads as a corner overlay, not a bite taken
                        // out of the icon.
                        .offset(x: 2, y: -2)
                }
            }
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
                VStack(spacing: 0) {
                    ScrollView { BoxWorkspaceView(box: box).padding() }
                    if box.isPaired {
                        // RunStopBar owns its own top Divider()/material (see
                        // RunStopBar.swift), so no extra divider here.
                        RunStopBar(box: box).id(box.id)
                    }
                }
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
