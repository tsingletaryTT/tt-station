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
