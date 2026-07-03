import SwiftUI
import TTStationKit

@main
struct TTStationApp: App {
    var body: some Scene {
        MenuBarExtra("TTStation", systemImage: "cpu") {
            Text("TTStation").padding()
        }
        .menuBarExtraStyle(.window)
    }
}
