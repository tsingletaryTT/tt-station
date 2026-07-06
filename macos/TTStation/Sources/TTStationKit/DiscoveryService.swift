import Foundation

public protocol DiscoveryService {
    func scan() async -> [BoxRecord]
}

public final class MDNSDiscoveryService: DiscoveryService {
    // `TTCommands` (not the concrete `TTClient`) so tests can substitute
    // `FakeTTClient` and assert what `scan()` seeds discovery with.
    private let client: TTCommands
    private let registry: HostRegistry
    public init(client: TTCommands, registry: HostRegistry) {
        self.client = client; self.registry = registry
    }

    public func scan() async -> [BoxRecord] {
        // Known hosts = manually-added AND previously-paired. Passing paired hosts
        // as manual `--host` seeds makes the CLI probe them DIRECTLY (HTTP /status,
        // mDNS-independent), so a box you've paired with shows up deterministically
        // even when the racy mDNS browse misses it (e.g. right after the pairing
        // handshake, when the agent re-publishes its mDNS record). merge() then
        // synthesizes a placeholder for any known host discovery still didn't return,
        // so a paired box never vanishes from the list on a transient miss.
        let known = Array(Set(registry.manualHosts).union(registry.pairedHosts)).sorted()
        let discovered = (try? await client.discover(manualHosts: known, noMdns: false)) ?? []
        return Self.merge(discovered: discovered, manualHosts: known)
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
