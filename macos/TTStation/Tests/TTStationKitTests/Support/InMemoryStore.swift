@testable import TTStationKit

final class InMemoryStore: KeyValueStore {
    private var storage: [String: [String]] = [:]
    func stringArray(_ key: String) -> [String] { storage[key] ?? [] }
    func setStringArray(_ value: [String], _ key: String) { storage[key] = value }
}
