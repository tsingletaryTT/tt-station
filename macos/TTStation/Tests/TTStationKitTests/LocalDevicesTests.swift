import XCTest
@testable import TTStationKit

/// `LocalReport` against real `tt-station --json local` output (Fixtures/local.json, captured
/// 2026-09-28 from a P100A in a Razer Core X V2 on this project's owner's Mac), plus the
/// AppModel wiring that shows/hides "This Mac".
final class LocalDevicesTests: XCTestCase {
    private func fixture() throws -> LocalReport {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "local", withExtension: "json", subdirectory: "Fixtures"))
        return try JSONDecoder().decode(LocalReport.self, from: Data(contentsOf: url))
    }

    func testDecodesRealReport() throws {
        let r = try fixture()
        XCTAssertEqual(r.deviceMesh, "p100")
        XCTAssertEqual(r.officialTT?.version, "1.0.1")
        XCTAssertEqual(r.models?.map(\.name), ["Llama-3.1-8B", "Llama-3.1-8B-Instruct"])
        XCTAssertNil(r.modelsError)
        let c = try XCTUnwrap(r.cards.first)
        XCTAssertEqual(c.pciID, "1e52:b140")
        XCTAssertEqual(c.subsystemLabel, "0x43")
        XCTAssertEqual(c.boardLabel, "P100A")
        XCTAssertEqual(c.chipLabel, "Blackhole")
        XCTAssertTrue(c.tunnelled)
        XCTAssertEqual(c.link, PCIeLink(gen: 4, width: 4))
        XCTAssertEqual(c.memorySummary, "512 MiB · 1 MiB · 16 B")
        XCTAssertFalse(c.hasLargeWindows, "BAR4 is unassigned over Thunderbolt: no 4 GiB windows")
    }

    func testLabels() throws {
        let r = try fixture()
        XCTAssertEqual(r.summary, "P100A via Thunderbolt · 2 right-sized models")
        XCTAssertEqual(r.cards[0].linkLabel, "PCIe Gen4 ×4 (≈ 7.9 GB/s raw)")
        XCTAssertEqual(r.models?.first?.contextLabel, "64K context")
        XCTAssertEqual(LocalCard.formatBytes(16), "16 B")
    }

    func testCapabilitiesAreHonestAboutWhatNeedsTheDriver() throws {
        let caps = LocalCapability.list(for: try fixture())
        XCTAssertEqual(caps.count, 4)
        guard case .detected = caps[0] else { return XCTFail("first should be detection") }
        guard case let .rightSized(s) = caps[1] else { return XCTFail("second should be right-sizing") }
        XCTAssertTrue(s.contains("tt 1.0.1"))
        let notYet = caps.compactMap { if case let .notYet(t, _) = $0 { t } else { nil } }
        XCTAssertEqual(notYet, ["Live telemetry", "Serving on this Mac"])
    }

    func testRightSizingFailureIsSurfacedNotHidden() {
        let r = LocalReport(cards: [sampleCard()], deviceMesh: "p100", officialTT: nil, models: nil,
                            modelsError: "`tt` is not the official Tenstorrent CLI")
        let caps = LocalCapability.list(for: r)
        guard case let .rightSizingUnavailable(why) = caps[1] else { return XCTFail("expected unavailable") }
        XCTAssertTrue(why.contains("not the official"))
        XCTAssertEqual(r.summary, "P100A via Thunderbolt")
    }

    func testUnclaimedCardSaysTelemetryIsNotYet() throws {
        let r = try fixture()                    // the real capture: no driver attached
        XCTAssertNil(r.cards[0].driver)
        XCTAssertEqual(r.cards[0].driverLabel, "none (unclaimed)")
        XCTAssertFalse(r.hasOurDriver)
        XCTAssertNil(r.telemetry)
    }

    func testTelemetryFromOurDriverDecodesAndFlipsTheCapability() throws {
        // What `tt-station --json local` emits once TTStationDriver holds the card (fields per
        // crates/tt-station/src/local.rs; values from the 2026-09-29 silicon run).
        let json = #"""
        {"cards":[{"name":"pci1e52,b140","location":"3:0:0","vendor_id":7762,"device_id":45376,
          "subsystem_vendor_id":7762,"subsystem_id":67,"chip":"blackhole","board_type":"p100a",
          "tunnelled":true,"link":{"gen":4,"width":4},"memory_ranges":[536870912,1048576,16],
          "driver":"ttstation-blackhole"}],
         "device_mesh":"p100","official_tt":null,"models":null,"models_error":null,
         "telemetry":{"abi":2,"boot_status":5,"arc_ready":true,"asic_temp_c":46.176,"power_w":15,
                      "vcore_mv":727,"current_a":22,"aiclk_mhz":800},
         "telemetry_error":null}
        """#
        let r = try JSONDecoder().decode(LocalReport.self, from: Data(json.utf8))
        XCTAssertTrue(r.hasOurDriver)
        XCTAssertEqual(r.cards[0].driverLabel, "TTStationDriver")
        XCTAssertEqual(r.telemetry?.aiclkMHz, 800)
        XCTAssertEqual(r.telemetry?.arcReady, true)
        let caps = LocalCapability.list(for: r)
        XCTAssertTrue(caps.contains { if case .telemetry = $0 { true } else { false } })
        XCTAssertFalse(caps.contains { if case let .notYet(t, _) = $0 { t == "Live telemetry" } else { false } })
    }

    func testOurDriverWithoutTelemetryIsSurfaced() {
        let card = LocalCard(name: "pci1e52,b140", location: "3:0:0", vendorID: 0x1e52, deviceID: 0xb140,
                             subsystemVendorID: 0x1e52, subsystemID: 0x43, chip: "blackhole", boardType: "p100a",
                             tunnelled: true, link: PCIeLink(gen: 4, width: 4), memoryRanges: [], driver: "ttstation-blackhole")
        let r = LocalReport(cards: [card], deviceMesh: "p100", officialTT: nil, models: nil, modelsError: nil,
                            telemetry: nil, telemetryError: "`TTStationDriver telemetry` printed no JSON")
        let caps = LocalCapability.list(for: r)
        XCTAssertTrue(caps.contains { if case let .telemetryUnavailable(why) = $0 { why.contains("no JSON") } else { false } })
    }

    func testNoCardsMeansNoCapabilities() {
        let r = LocalReport(cards: [], deviceMesh: nil, officialTT: nil, models: nil, modelsError: nil)
        XCTAssertFalse(r.hasCards)
        XCTAssertTrue(LocalCapability.list(for: r).isEmpty)
    }

    // MARK: AppModel wiring

    @MainActor
    func testRefreshLocalStoresReportAndDropsStaleSelectionOnUnplug() async throws {
        let fake = FakeTTClient()
        fake.localResult = try fixture()
        let model = AppModel(commands: fake, discovery: FakeDiscoveryService(), registry: HostRegistry(store: InMemoryStore()))

        await model.refreshLocal()
        XCTAssertEqual(model.localReport?.deviceMesh, "p100")
        model.selectedHostPort = AppModel.localSelectionID
        XCTAssertTrue(model.isLocalSelected)

        // Enclosure unplugged: next refresh reports no cards → the selection must not dangle.
        fake.localResult = LocalReport(cards: [], deviceMesh: nil, officialTT: nil, models: nil, modelsError: nil)
        await model.refreshLocal()
        XCTAssertFalse(model.isLocalSelected)
    }

    @MainActor
    func testRefreshLocalFailureIsNonFatal() async {
        let fake = FakeTTClient()
        fake.localError = .commandFailed(command: ["--json", "local"], exitCode: 2, stderr: "unrecognized subcommand 'local'")
        let model = AppModel(commands: fake, discovery: FakeDiscoveryService(), registry: HostRegistry(store: InMemoryStore()))
        await model.refreshLocal()
        XCTAssertNil(model.localReport)
        XCTAssertNotNil(model.localError)
    }

    private func sampleCard() -> LocalCard {
        LocalCard(name: "pci1e52,b140", location: "3:0:0", vendorID: 0x1e52, deviceID: 0xb140,
                  subsystemVendorID: 0x1e52, subsystemID: 0x43, chip: "blackhole", boardType: "p100a",
                  tunnelled: true, link: PCIeLink(gen: 4, width: 4), memoryRanges: [1 << 29, 1 << 20, 16])
    }
}
