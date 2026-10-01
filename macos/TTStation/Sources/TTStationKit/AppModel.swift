import Foundation
import Observation

@Observable @MainActor
public final class AppModel {
    public enum ScanState: Equatable { case idle, scanning, failed(String) }

    public var boxes: [BoxViewModel] = []
    public var selectedHostPort: String?
    public var scanState: ScanState = .idle

    /// Sidebar/popover selection id for "This Mac" (the locally attached card). A `hostPort` can
    /// never look like this (no port), so it can't collide with a box.
    public static let localSelectionID = "local:this-mac"

    /// What `tt-station local` last reported; `nil` until the first scan finishes.
    public var localReport: LocalReport?
    /// Why the last local scan failed (e.g. an old `tt-station` without `local`); shown quietly.
    public var localError: String?

    /// True when "This Mac" is the current selection.
    public var isLocalSelected: Bool { selectedHostPort == Self.localSelectionID }

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
        await refreshLocal()
        // With nothing selected yet, a card physically attached to this Mac is the most
        // immediate thing to show; otherwise the first box, as before.
        if selectedHostPort == nil {
            selectedHostPort = (localReport?.hasCards ?? false) ? Self.localSelectionID : boxes.first?.id
        }
        for box in boxes { await box.refresh() }
        scanState = .idle
    }

    /// Re-read locally attached cards. Never fatal: an error is recorded and the "This Mac"
    /// entry just hides (an older `tt-station` without `local` fails here harmlessly).
    public func refreshLocal() async {
        do {
            localReport = try await commands.local()
            localError = nil
        } catch {
            localReport = nil
            localError = String(describing: error)
        }
        // Drop a stale "This Mac" selection if the card went away (enclosure unplugged).
        if isLocalSelected, !(localReport?.hasCards ?? false) { selectedHostPort = boxes.first?.id }
    }

    public func addManualHost(_ host: String) {
        registry.addManualHost(host)
    }
}
