import Foundation
import Observation

@Observable @MainActor
public final class AppModel {
    public enum ScanState: Equatable { case idle, scanning, failed(String) }

    public var boxes: [BoxViewModel] = []
    public var selectedHostPort: String?
    public var scanState: ScanState = .idle

    private let commands: TTCommands
    private let discovery: DiscoveryService
    private let registry: HostRegistry

    public init(commands: TTCommands, discovery: DiscoveryService, registry: HostRegistry) {
        self.commands = commands
        self.discovery = discovery
        self.registry = registry
    }

    public var selectedBox: BoxViewModel? {
        boxes.first { $0.id == selectedHostPort }
    }

    /// Count of boxes currently `.serving` a model — drives the menu-bar
    /// icon's badge (Task 2 of "highlight running models in the toolbar").
    /// `.starting` deliberately does not count: a box mid-spin-up isn't
    /// "serving" yet, so the badge only lights once a model is actually up.
    /// Maps `boxes` to their `runningState` and delegates to the pure static
    /// helper below so the counting logic itself is unit-testable without
    /// constructing a full `BoxViewModel` (which needs a `TTCommands` and a
    /// `HostRegistry`).
    public var servingCount: Int {
        Self.servingCount(boxes.map(\.runningState))
    }

    /// True if any box is `.serving` — a thin convenience over `servingCount`
    /// for call sites that only care about presence, not the exact number.
    public var anyServing: Bool {
        servingCount > 0
    }

    /// Pure helper: counts how many of the given `RunningState`s are
    /// `.serving`. No I/O, no dependency on `BoxViewModel` — exercised
    /// directly by `AppModelTests` with a plain array of states.
    public static func servingCount(_ states: [RunningState]) -> Int {
        states.reduce(into: 0) { count, state in
            if case .serving = state { count += 1 }
        }
    }

    public func scan() async {
        guard scanState != .scanning else { return }
        scanState = .scanning
        let records = await discovery.scan()
        // Reconcile by hostPort: reuse the existing BoxViewModel for a box that's
        // still present (so the window/popover keep observing a stable instance and
        // its live state survives a rescan); make new ones only for new hosts.
        let existing = Dictionary(boxes.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        boxes = records.map { rec in
            existing[rec.hostPort] ?? BoxViewModel(record: rec, commands: commands, registry: registry)
        }
        if selectedHostPort == nil { selectedHostPort = boxes.first?.id }
        for box in boxes { await box.refresh() }
        scanState = .idle
    }

    public func addManualHost(_ host: String) {
        registry.addManualHost(host)
    }
}
