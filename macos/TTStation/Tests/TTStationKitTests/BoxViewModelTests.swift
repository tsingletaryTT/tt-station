import XCTest
@testable import TTStationKit

@MainActor
final class BoxViewModelTests: XCTestCase {
    private func makeVM(
        paired: Bool = true,
        client: FakeTTClient = FakeTTClient(),
        statusRaw: String = "idle"
    ) -> (BoxViewModel, HostRegistry) {
        let reg = HostRegistry(store: InMemoryStore())
        let rec = BoxRecord(name: "b", host: "h", ctrlPort: 8080, chips: "4xBH", statusRaw: statusRaw, apiver: 1)
        if paired { reg.markPaired(rec.hostPort) }
        return (BoxViewModel(record: rec, commands: client, registry: reg), reg)
    }

    func testRefreshLoadsStatus() async {
        let client = FakeTTClient(); client.statusResult = .serving(model: "Qwen3-8B")
        let (vm, _) = makeVM(client: client)
        await vm.refresh()
        XCTAssertEqual(vm.status, .serving(model: "Qwen3-8B"))
    }

    func testPairSuccessMarksPairedAndLoadsModels() async {
        let (vm, reg) = makeVM(paired: false)
        await vm.pair(code: "042817")
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertEqual(vm.models.map(\.name), ["Qwen3-8B"])
    }

    func testRunSetsEndpoint() async {
        let (vm, _) = makeVM()
        vm.selectedModel = "Qwen3-8B"
        await vm.run()
        XCTAssertEqual(vm.endpoint?.baseURL, "http://h:8000/v1")
        XCTAssertFalse(vm.inFlight)
    }

    func testAuthErrorOnRunFlipsToUnpaired() async {
        let client = FakeTTClient()
        client.runError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored")
        let (vm, reg) = makeVM(client: client)
        vm.selectedModel = "Qwen3-8B"
        await vm.run()
        XCTAssertFalse(vm.isPaired)
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
        XCTAssertNotNil(vm.errorText)
    }

    func testInitSeedsStatusFromRecord() {
        let (vm, _) = makeVM(paired: false, statusRaw: "serving:Foo")
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
    }

    func testUnpairedRefreshDoesNotCallStatusAndKeepsSeededStatus() async {
        let client = FakeTTClient()
        let (vm, _) = makeVM(paired: false, client: client, statusRaw: "serving:Foo")
        await vm.refresh()
        XCTAssertFalse(client.statusCalled)
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
    }

    func testPairedServingRefreshFetchesEndpoint() async {
        let client = FakeTTClient()
        client.statusResult = .serving(model: "Foo")
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertNotNil(vm.endpoint)
        XCTAssertEqual(vm.endpoint?.baseURL, client.runEndpoint.baseURL)
    }
}
