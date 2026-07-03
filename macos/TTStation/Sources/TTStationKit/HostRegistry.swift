import Foundation

public protocol KeyValueStore {
    func stringArray(_ key: String) -> [String]
    func setStringArray(_ value: [String], _ key: String)
}

extension UserDefaults: KeyValueStore {
    public func stringArray(_ key: String) -> [String] { stringArray(forKey: key) ?? [] }
    public func setStringArray(_ value: [String], _ key: String) { set(value, forKey: key) }
}

/// Persists manually-added hosts and which hosts the CLI has paired. The app
/// never reads the Keychain; "paired" is tracked here after a successful pair
/// and cleared on an auth error.
public final class HostRegistry {
    private let store: KeyValueStore
    private let manualKey = "tt.manualHosts"
    private let pairedKey = "tt.pairedHosts"

    public init(store: KeyValueStore) { self.store = store }

    public var manualHosts: [String] { store.stringArray(manualKey) }
    public func addManualHost(_ host: String) {
        var hosts = manualHosts
        guard !hosts.contains(host) else { return }
        hosts.append(host)
        store.setStringArray(hosts, manualKey)
    }
    public func removeManualHost(_ host: String) {
        store.setStringArray(manualHosts.filter { $0 != host }, manualKey)
    }

    public var pairedHosts: Set<String> { Set(store.stringArray(pairedKey)) }
    public func markPaired(_ host: String) {
        store.setStringArray(Array(pairedHosts.union([host])), pairedKey)
    }
    public func markUnpaired(_ host: String) {
        store.setStringArray(Array(pairedHosts.subtracting([host])), pairedKey)
    }
}
