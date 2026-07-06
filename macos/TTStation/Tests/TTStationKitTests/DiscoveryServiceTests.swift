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

    /// A box you've PAIRED with (but never manually added) must not vanish
    /// from the list on a transient mDNS miss: `scan()` should pass its
    /// host:port to `discover()` as a direct-probe seed so the CLI's manual
    /// `--host` path probes it over HTTP regardless of the mDNS browse
    /// result, and `merge()` synthesizes a placeholder if discovery still
    /// comes back empty for it.
    func testScanSeedsPairedHostsSoTheyPersistThroughAnMdnsMiss() async {
        let reg = HostRegistry(store: InMemoryStore())
        reg.markPaired("1.2.3.4:8765")
        let fake = FakeTTClient()
        fake.discoverResult = [] // simulate an mDNS browse miss
        let service = MDNSDiscoveryService(client: fake, registry: reg)

        let result = await service.scan()

        XCTAssertTrue(result.contains { $0.hostPort == "1.2.3.4:8765" }, "paired host vanished from the list on an mDNS miss")
        XCTAssertTrue(fake.discoverManualHostsSeen.contains("1.2.3.4:8765"), "paired host was not passed to discover() as a direct-probe seed")
    }

    /// When discovery DOES find the paired host, the real discovered record
    /// (with real chips/status) should win over the synthetic placeholder --
    /// no duplicate entries for the same host:port.
    func testScanPrefersDiscoveredRecordOverSyntheticForPairedHost() async {
        let reg = HostRegistry(store: InMemoryStore())
        reg.markPaired("1.2.3.4:8765")
        let fake = FakeTTClient()
        fake.discoverResult = [
            BoxRecord(name: "qb2", host: "1.2.3.4", ctrlPort: 8765, chips: "4xBH", statusRaw: "idle", apiver: 1)
        ]
        let service = MDNSDiscoveryService(client: fake, registry: reg)

        let result = await service.scan()

        let matches = result.filter { $0.hostPort == "1.2.3.4:8765" }
        XCTAssertEqual(matches.count, 1)
        XCTAssertEqual(matches.first?.chips, "4xBH")
    }
}
