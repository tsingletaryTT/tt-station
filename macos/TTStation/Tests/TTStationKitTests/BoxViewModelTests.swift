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

    func testStartPairingSetsPairId() async {
        let client = FakeTTClient()
        client.pairInitResult = "pid-123"
        let (vm, _) = makeVM(paired: false, client: client)
        await vm.startPairing()
        XCTAssertEqual(vm.pairId, "pid-123")
        XCTAssertNil(vm.errorText)
    }

    func testCompletePairingSuccessMarksPairedAndLoadsModels() async {
        let (vm, reg) = makeVM(paired: false)
        await vm.startPairing()
        await vm.completePairing(code: "123456")
        XCTAssertTrue(vm.isPaired)
        XCTAssertNil(vm.pairId)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertEqual(vm.models.map(\.name), ["Qwen3-8B"])
        XCTAssertNil(vm.errorText)
    }

    func testCompletePairingFailureShowsErrorAndClearsPairId() async {
        let client = FakeTTClient()
        client.pairCompleteError = .commandFailed(command: [], exitCode: 1, stderr: "invalid or expired code")
        let (vm, _) = makeVM(paired: false, client: client)
        await vm.startPairing()
        await vm.completePairing(code: "000000")
        XCTAssertFalse(vm.isPaired)
        XCTAssertEqual(vm.errorText, "invalid or expired code")
        XCTAssertNil(vm.pairId)
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
        // Renamed in spirit (fix #7): refresh now ALWAYS probes status and
        // derives paired-state from the result — a "no token" failure is the
        // normal unpaired signal, not an error. Kept the seeded-status
        // assertion; dropped the "does not call status" assertion since that
        // behavior is intentionally reversed.
        let client = FakeTTClient()
        client.statusError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080")
        let (vm, _) = makeVM(paired: false, client: client, statusRaw: "serving:Foo")
        await vm.refresh()
        XCTAssertTrue(client.statusCalled)
        XCTAssertFalse(vm.isPaired)
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
    }

    func testRefreshWithValidTokenReconcilesToPaired() async {
        let client = FakeTTClient()
        client.statusResult = .serving(model: "Foo")
        let (vm, reg) = makeVM(paired: false, client: client)
        XCTAssertFalse(vm.isPaired)
        await vm.refresh()
        XCTAssertTrue(vm.isPaired)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
        XCTAssertNil(vm.errorText)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
    }

    func testUnpairedRefreshSurfacesNoError() async {
        let client = FakeTTClient()
        client.statusError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080")
        let (vm, reg) = makeVM(paired: true, client: client, statusRaw: "serving:Foo")
        await vm.refresh()
        XCTAssertFalse(vm.isPaired)
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
    }

    func testRefreshTimeoutShowsErrorNotUnpaired() async {
        let client = FakeTTClient()
        client.statusError = .timedOut(command: [], seconds: 20)
        let (vm, reg) = makeVM(paired: true, client: client)
        let wasPaired = vm.isPaired
        await vm.refresh()
        XCTAssertNotNil(vm.errorText)
        XCTAssertEqual(vm.isPaired, wasPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
    }

    func testRefreshPopulatesServingList() async {
        let client = FakeTTClient()
        client.serving_ = [
            ServingEntry(model: "Qwen3-8B", baseURL: "http://h:8000/v1", hostPort: 8000, container: "agent-c", source: "agent"),
            ServingEntry(model: "Llama", baseURL: "http://h:8001/v1", hostPort: 8001, container: "tt-studio-c", source: "external"),
        ]
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertEqual(vm.serving.map(\.model), ["Qwen3-8B", "Llama"])
        XCTAssertEqual(vm.serving.map(\.source), ["agent", "external"])
    }

    func testRefreshServingFailureYieldsEmptyNotFatal() async {
        let client = FakeTTClient()
        client.statusResult = .serving(model: "Foo")
        client.servingError = .commandFailed(command: [], exitCode: 1, stderr: "boom")
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertTrue(vm.serving.isEmpty)
        // A serving-read failure must not surface an error or disturb status.
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
    }

    func testLoadModelsSeedsSmartDefault() async {
        let client = FakeTTClient()
        client.models_ = [
            ModelInfo(name: "meta-llama/Llama-3.3-70B-Instruct", devices: ["T3K"]),
            ModelInfo(name: "Qwen/Qwen3-8B", devices: ["P300X2"]),
            ModelInfo(name: "Qwen/Qwen2.5-7B-Instruct", devices: ["P300X2"]),
        ]
        let (vm, _) = makeVM(client: client)
        await vm.loadModels()
        // Best score: instruct + 7B sweet spot beats base-8B and huge-70B.
        XCTAssertEqual(vm.selectedModel, "Qwen/Qwen2.5-7B-Instruct")
    }

    func testLoadModelsHonoursRememberedLastModel() async {
        let client = FakeTTClient()
        client.models_ = [
            ModelInfo(name: "Qwen/Qwen3-8B", devices: ["P300X2"]),
            ModelInfo(name: "Qwen/Qwen2.5-7B-Instruct", devices: ["P300X2"]),
        ]
        let reg = HostRegistry(store: InMemoryStore())
        let rec = BoxRecord(name: "b", host: "h", ctrlPort: 8080, chips: "4xBH", statusRaw: "idle", apiver: 1)
        reg.markPaired(rec.hostPort)
        reg.setLastModel("Qwen/Qwen3-8B", forHost: rec.hostPort)
        let vm = BoxViewModel(record: rec, commands: client, registry: reg)
        await vm.loadModels()
        XCTAssertEqual(vm.selectedModel, "Qwen/Qwen3-8B")
    }

    func testRunPersistsLastModelAndClearsStarting() async {
        let (vm, reg) = makeVM()
        vm.selectedModel = "Qwen3-8B"
        await vm.run()
        XCTAssertEqual(reg.lastModel(forHost: "h:8080"), "Qwen3-8B")
        XCTAssertFalse(vm.starting)
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
