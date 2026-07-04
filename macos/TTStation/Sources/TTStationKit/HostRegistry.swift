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
    /// Per-host last-run model is stored under `tt.lastModel.<host:port>` as a
    /// single-element string array — reusing the existing `KeyValueStore`
    /// string-array API rather than widening the protocol, so persistence
    /// stays additive and non-breaking.
    private let lastModelKeyPrefix = "tt.lastModel."

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

    /// The model most recently run on `host` via this app, or `nil` if none
    /// has been recorded. Feeds `ModelDefaults.pickDefaultModel` so a box
    /// re-opens on the user's last choice.
    public func lastModel(forHost host: String) -> String? {
        store.stringArray(lastModelKeyPrefix + host).first
    }

    /// Remember `model` as the last one run on `host`. Mirrors `markPaired`'s
    /// UserDefaults-via-`store` approach.
    public func setLastModel(_ model: String, forHost host: String) {
        store.setStringArray([model], lastModelKeyPrefix + host)
    }
}
