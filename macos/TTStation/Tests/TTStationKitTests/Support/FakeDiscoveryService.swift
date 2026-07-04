import Foundation
@testable import TTStationKit

/// A `DiscoveryService` test double whose `scan()` suspends until the test
/// explicitly calls `resume()`. Used to prove `AppModel.scan()`'s
/// re-entrancy guard: a second `scan()` call made while the first is still
/// suspended inside `discovery.scan()` must not trigger a second discovery
/// pass. `scanCount` records how many times the discovery pass actually ran.
///
/// Implemented as an actor so the counter and continuation are safe to touch
/// from both the in-flight `scan()` call (suspended on `MainActor`) and the
/// test body driving it.
actor FakeDiscoveryService: DiscoveryService {
    private(set) var scanCount = 0
    private var continuation: CheckedContinuation<Void, Never>?

    func scan() async -> [BoxRecord] {
        scanCount += 1
        await withCheckedContinuation { continuation in
            self.continuation = continuation
        }
        return []
    }

    /// Lets a suspended `scan()` call return. No-op if nothing is waiting.
    func resume() {
        continuation?.resume()
        continuation = nil
    }
}
