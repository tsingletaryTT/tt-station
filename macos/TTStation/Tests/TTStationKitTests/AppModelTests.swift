import XCTest
@testable import TTStationKit

@MainActor
final class AppModelTests: XCTestCase {
    /// Regression test for the final-review re-entrancy finding: `scan()`
    /// now fires from three call sites (popover `.task`, window `.task`,
    /// sidebar Refresh) with `await` suspension points and no guard, so
    /// overlapping calls could double the mDNS discovery pass and the
    /// per-box `refresh()` writes to the registry.
    ///
    /// This proves a second `scan()` invoked while the first is still
    /// suspended inside `discovery.scan()` returns immediately without
    /// starting a second discovery pass.
    func testConcurrentScanDoesNotDoubleDiscover() async {
        let discovery = FakeDiscoveryService()
        let model = AppModel(
            commands: FakeTTClient(),
            discovery: discovery,
            registry: HostRegistry(store: InMemoryStore())
        )

        // Kick off the first scan; it will suspend inside discovery.scan()
        // until we call resume(). Since AppModel is @MainActor, scanState is
        // already set to .scanning by the time this Task's first await point
        // (discovery.scan()) yields control back — no race with the second
        // call below.
        let firstScan = Task { await model.scan() }

        // Wait for the first scan to actually be in flight (its counter
        // incremented) before racing the second call against it.
        while await discovery.scanCount == 0 {
            await Task.yield()
        }
        XCTAssertEqual(model.scanState, .scanning)

        // A second scan started now must be a no-op: the guard should see
        // .scanning and return before ever calling discovery.scan() again.
        await model.scan()
        let countWhileFirstStillInFlight = await discovery.scanCount
        XCTAssertEqual(countWhileFirstStillInFlight, 1, "guard should have short-circuited the second scan()")

        await discovery.resume()
        await firstScan.value

        let finalCount = await discovery.scanCount
        XCTAssertEqual(finalCount, 1, "only one discovery pass should have run in total")
        XCTAssertEqual(model.scanState, .idle)
    }
}
