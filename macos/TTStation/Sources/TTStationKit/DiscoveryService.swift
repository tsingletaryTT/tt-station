import Foundation

public protocol DiscoveryService {
    func scan() async -> [BoxRecord]
}

public final class MDNSDiscoveryService: DiscoveryService {
    private let client: TTClient
    private let registry: HostRegistry
    public init(client: TTClient, registry: HostRegistry) {
        self.client = client; self.registry = registry
    }

    public func scan() async -> [BoxRecord] {
        let manual = registry.manualHosts
        let discovered = (try? await client.discover(manualHosts: manual, noMdns: false)) ?? []
        return Self.merge(discovered: discovered, manualHosts: manual)
    }

    /// Dedupe by `host:port`; append a synthetic idle record for any manual
    /// host the discovery pass didn't already return.
    public static func merge(discovered: [BoxRecord], manualHosts: [String]) -> [BoxRecord] {
        var byHostPort: [String: BoxRecord] = [:]
        for box in discovered { byHostPort[box.hostPort] = box }
        var result = discovered
        for host in manualHosts where byHostPort[host] == nil {
            let parts = host.split(separator: ":")
            guard parts.count == 2, let port = Int(parts[1]) else { continue }
            result.append(BoxRecord(name: host, host: String(parts[0]), ctrlPort: port, chips: "?", statusRaw: "idle", apiver: 1))
        }
        return result
    }
}
