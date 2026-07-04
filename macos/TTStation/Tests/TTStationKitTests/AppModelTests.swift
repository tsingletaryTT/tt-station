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

    /// Regression test for the "window stops live-updating" bug: `scan()`
    /// used to rebuild every `BoxViewModel` from scratch on each pass, which
    /// swapped out the very instance a second surface (e.g. the detail
    /// window) was observing. This proves a box whose `hostPort` is still
    /// present across two scans keeps the *same* `BoxViewModel` instance.
    func testScanReusesExistingBoxViewModelInstanceForUnchangedHost() async {
        let discovery = FakeDiscoveryService()
        let record = BoxRecord(name: "qb2", host: "qb2-lab.local", ctrlPort: 8765, chips: "p300x2", statusRaw: "idle", apiver: 1)
        await discovery.setRecords([record])

        let model = AppModel(
            commands: FakeTTClient(),
            discovery: discovery,
            registry: HostRegistry(store: InMemoryStore())
        )

        await runScan(model, discovery: discovery)
        XCTAssertEqual(model.boxes.count, 1)
        let firstInstanceID = ObjectIdentifier(model.boxes[0])

        await runScan(model, discovery: discovery)
        XCTAssertEqual(model.boxes.count, 1)
        XCTAssertEqual(
            ObjectIdentifier(model.boxes[0]), firstInstanceID,
            "the BoxViewModel for an unchanged host must be reused, not rebuilt"
        )
    }

    /// Complements the reuse test above: a box that disappears from
    /// discovery must be dropped, and a newly-discovered host must get a
    /// brand-new `BoxViewModel` (there is nothing to reuse).
    func testScanDropsGoneBoxAndCreatesFreshInstanceForNewHost() async {
        let discovery = FakeDiscoveryService()
        let recordA = BoxRecord(name: "qb2-a", host: "qb2-a.local", ctrlPort: 8765, chips: "p300x2", statusRaw: "idle", apiver: 1)
        let recordB = BoxRecord(name: "qb2-b", host: "qb2-b.local", ctrlPort: 8765, chips: "p300x2", statusRaw: "idle", apiver: 1)
        await discovery.setRecords([recordA])

        let model = AppModel(
            commands: FakeTTClient(),
            discovery: discovery,
            registry: HostRegistry(store: InMemoryStore())
        )

        await runScan(model, discovery: discovery)
        XCTAssertEqual(model.boxes.map(\.id), [recordA.hostPort])
        let instanceA = ObjectIdentifier(model.boxes[0])

        // recordA drops off the network; recordB newly appears.
        await discovery.setRecords([recordB])
        await runScan(model, discovery: discovery)

        XCTAssertEqual(model.boxes.map(\.id), [recordB.hostPort], "the gone box should be dropped")
        XCTAssertNotEqual(
            ObjectIdentifier(model.boxes[0]), instanceA,
            "a newly-discovered host must get a fresh BoxViewModel, not a stale one"
        )
    }

    /// Drives one `scan()` pass through the fake's suspend/resume
    /// choreography: waits for `discovery.scan()` to actually be in flight,
    /// resumes it, then waits for `scan()` to return.
    private func runScan(_ model: AppModel, discovery: FakeDiscoveryService) async {
        let countBefore = await discovery.scanCount
        let task = Task { await model.scan() }
        while await discovery.scanCount == countBefore {
            await Task.yield()
        }
        await discovery.resume()
        await task.value
    }
}
