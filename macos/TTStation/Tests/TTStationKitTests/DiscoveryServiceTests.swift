import XCTest
@testable import TTStationKit

final class DiscoveryServiceTests: XCTestCase {
    func testRegistryPersistsManualAndPaired() {
        let reg = HostRegistry(store: InMemoryStore())
        reg.addManualHost("h:8080")
        reg.addManualHost("h:8080") // dedupe
        reg.markPaired("h:8080")
        XCTAssertEqual(reg.manualHosts, ["h:8080"])
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        reg.markUnpaired("h:8080")
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
    }

    func testMergeAddsManualHostsNotDiscovered() {
        let discovered = [BoxRecord(name: "b", host: "1.2.3.4", ctrlPort: 8080, chips: "4xBH", statusRaw: "idle", apiver: 1)]
        let merged = MDNSDiscoveryService.merge(discovered: discovered, manualHosts: ["1.2.3.4:8080", "9.9.9.9:8080"])
        XCTAssertEqual(merged.count, 2) // discovered 1.2.3.4 kept once; 9.9.9.9 added
        XCTAssertTrue(merged.contains { $0.hostPort == "9.9.9.9:8080" })
        XCTAssertEqual(merged.filter { $0.hostPort == "1.2.3.4:8080" }.count, 1)
    }
}
