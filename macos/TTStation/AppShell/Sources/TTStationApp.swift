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
        MenuBarExtra("TTStation", systemImage: "cpu") {
            MenuContentView(model: model)
                .frame(width: 340)
        }
        .menuBarExtraStyle(.window)
    }
}
