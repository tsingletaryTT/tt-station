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
