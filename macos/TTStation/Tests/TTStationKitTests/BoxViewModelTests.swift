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

    func testCompletePairingWithSSHEnabledCallsAuthorizeAndSetsMessage() async {
        let client = FakeTTClient()
        client.sshAuthorizeResult = SshAuthorizeInfo(authorized: true, sshUser: "ttuser", alreadyPresent: false)
        let (vm, _) = makeVM(paired: false, client: client)
        vm.enableSSH = true
        await vm.startPairing()
        await vm.completePairing(code: "123456")
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(client.sshAuthorizeCalled)
        XCTAssertEqual(vm.sshMessage, "SSH enabled — connect as ttuser.")
    }

    func testCompletePairingWithSSHDisabledSkipsAuthorize() async {
        let client = FakeTTClient()
        let (vm, _) = makeVM(paired: false, client: client)
        vm.enableSSH = false
        await vm.startPairing()
        await vm.completePairing(code: "123456")
        XCTAssertTrue(vm.isPaired)
        XCTAssertFalse(client.sshAuthorizeCalled)
        XCTAssertNil(vm.sshMessage)
    }

    func testCompletePairingSSHFailureIsNonFatalToPairing() async {
        let client = FakeTTClient()
        client.sshAuthorizeError = .commandFailed(command: [], exitCode: 1, stderr: "no local SSH key found")
        let (vm, _) = makeVM(paired: false, client: client)
        vm.enableSSH = true
        await vm.startPairing()
        await vm.completePairing(code: "123456")
        // Pairing itself must still succeed — the SSH step failing doesn't
        // undo it.
        XCTAssertTrue(vm.isPaired)
        XCTAssertNil(vm.pairId)
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.sshMessage, "SSH setup failed: no local SSH key found")
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
        // Renamed in spirit twice over now: `status()` is unauthed (never a
        // pairing signal, see the pairing-fix at the top of this file) but
        // is still always probed for display and falls back to the
        // discovery-seeded status on failure. Paired-state instead comes
        // from the authed `endpoint()` probe below returning a 401.
        let client = FakeTTClient()
        client.statusError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080")
        client.endpointError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080")
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
        client.endpointError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080")
        let (vm, reg) = makeVM(paired: true, client: client, statusRaw: "serving:Foo")
        await vm.refresh()
        XCTAssertFalse(vm.isPaired)
        XCTAssertNil(vm.errorText)
        XCTAssertEqual(vm.status, .serving(model: "Foo"))
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
    }

    func testRefreshTimeoutShowsErrorNotUnpaired() async {
        // `status()` is display-only and no longer feeds `errorText` on
        // failure (see the pairing fix); the pairing probe (`endpoint()`)
        // timing out is what should surface an error without flipping
        // `isPaired`.
        let client = FakeTTClient()
        client.endpointError = .timedOut(command: [], seconds: 20)
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

    func testRefreshPopulatesConfig() async {
        let client = FakeTTClient()
        let (vm, _) = makeVM(paired: false, client: client)
        await vm.refresh()
        XCTAssertEqual(vm.config, client.configResult)
    }

    func testRefreshConfigFailureYieldsNilNotFatal() async {
        let client = FakeTTClient()
        client.configError = .commandFailed(command: [], exitCode: 1, stderr: "boom")
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertNil(vm.config)
        XCTAssertNil(vm.errorText)
    }

    func testPairedServingRefreshFetchesEndpoint() async {
        let client = FakeTTClient()
        client.statusResult = .serving(model: "Foo")
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertNotNil(vm.endpoint)
        XCTAssertEqual(vm.endpoint?.baseURL, client.runEndpoint.baseURL)
    }

    // MARK: - Pairing derives from the authed endpoint() probe, not status()

    /// `GET /status` is unauthed and answers 200 for any reachable box
    /// regardless of pairing, so it can never signal "unpaired" -- that's the
    /// bug this fix addresses. `endpoint()` IS bearer-guarded, so a 401
    /// (surfaced here as an `isAuthError`-matching `.commandFailed`) is the
    /// real "this Mac holds no valid token" signal.
    func testRefreshUnpairedWhenEndpointAuthError() async {
        let client = FakeTTClient()
        client.endpointError = .commandFailed(
            command: ["endpoint"], exitCode: 1,
            stderr: "error: request to ... failed: HTTP status 401 Unauthorized")
        let (vm, reg) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertFalse(vm.isPaired)
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
    }

    /// A 409 from `endpoint()` means "authed fine, nothing is serving right
    /// now" -- it must NOT be misread as an auth failure, or an idle-but-
    /// paired box would incorrectly flip to unpaired on every refresh.
    func testRefreshPairedWhenEndpointIdleConflict() async {
        let client = FakeTTClient()
        client.endpointError = .commandFailed(
            command: ["endpoint"], exitCode: 1,
            stderr: "no model is currently serving on this agent (409)")
        let (vm, reg) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertNil(vm.endpoint)
    }

    /// The common case: authed, and something is actually serving -- 200
    /// carries the `Endpoint` straight through.
    func testRefreshPairedAndServingWhenEndpointReturns() async {
        let client = FakeTTClient()
        let (vm, reg) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertNotNil(vm.endpoint)
        XCTAssertEqual(vm.endpoint?.baseURL, client.runEndpoint.baseURL)
    }

    /// A network hang/timeout on the `endpoint()` probe is not an auth
    /// signal -- a slow or wedged box must not get bounced to "unpaired"
    /// just because it's slow right now.
    func testRefreshTimeoutLeavesIsPairedUntouched() async {
        let client = FakeTTClient()
        client.endpointError = .timedOut(command: ["endpoint"], seconds: 5)
        let (vm, reg) = makeVM(paired: true, client: client)
        let wasPaired = vm.isPaired
        await vm.refresh()
        XCTAssertEqual(vm.isPaired, wasPaired)
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertNotNil(vm.errorText)
    }

    func testRefreshPopulatesCatalog() async {
        let client = FakeTTClient()
        let (vm, _) = makeVM(paired: false, client: client)
        await vm.refresh()
        XCTAssertEqual(vm.catalog, client.catalogResult)
    }

    func testRefreshCatalogFailureYieldsNilNotFatal() async {
        let client = FakeTTClient()
        client.catalogError = .commandFailed(command: [], exitCode: 1, stderr: "boom")
        let (vm, _) = makeVM(paired: true, client: client)
        await vm.refresh()
        XCTAssertNil(vm.catalog)
        XCTAssertNil(vm.errorText)
    }

    // MARK: - Cancel-a-load

    /// The only real way to abort an in-progress `run()` is to tell the agent
    /// to `stop` -- that makes the container spin-up fail fast. This test
    /// drives a genuinely in-flight `run()` (gated by `FakeTTClient.gateRun`),
    /// calls `cancelStart()` while it's still awaiting, and confirms: (a) it
    /// calls through to `stop`, (b) the load, once released with an
    /// agent-abort-shaped error, unwinds into a clean idle state rather than
    /// surfacing the abort as a user-facing error.
    func testCancelStartAbortsLoadViaStopAndLandsIdle() async {
        let client = FakeTTClient()
        client.gateRun = true
        let (vm, _) = makeVM(client: client)
        vm.selectedModel = "Qwen3-8B"

        let runTask = Task { await vm.run() }

        // Spin until the fake's run() is actually suspended, i.e. `starting`
        // is genuinely true because of an in-flight await, not just a race.
        while !client.runIsWaiting {
            await Task.yield()
        }
        XCTAssertTrue(vm.starting)
        XCTAssertTrue(vm.canStopOrCancel)

        await vm.cancelStart()
        XCTAssertTrue(client.stopCalled)
        XCTAssertTrue(vm.cancelling)
        XCTAssertEqual(vm.status, .idle)
        // While a cancel is genuinely in progress, the primary control must
        // be disabled -- no double-firing stop/cancel on top of it.
        XCTAssertFalse(vm.canStopOrCancel)

        // Simulate the agent aborting the spin-up: the in-flight `run()`
        // fails fast with a command-failure once `stop` has killed the
        // container being spun up.
        client.runError = .commandFailed(command: ["run"], exitCode: 1, stderr: "aborted")
        client.releaseRun()
        await runTask.value

        XCTAssertEqual(vm.status, .idle)
        XCTAssertNil(vm.endpoint)
        XCTAssertNil(vm.errorText)
        XCTAssertFalse(vm.cancelling)
        XCTAssertFalse(vm.starting)
        XCTAssertFalse(vm.inFlight)
    }

    /// Race between a user-initiated cancel and the load actually succeeding:
    /// `cancelStart()` fires `stop()` while `run()` is still in flight, but
    /// the fake's `run()` resolves with a SUCCESSFUL endpoint (not an error)
    /// once released. `run()`'s success branch must still honor `cancelling`
    /// -- landing on clean idle -- rather than unconditionally reporting
    /// `.serving`, which would silently override the user's cancel while
    /// `stop()` is tearing the container down underneath it.
    func testCancelHonoredWhenRunSucceedsDuringCancel() async {
        let client = FakeTTClient()
        client.gateRun = true
        let (vm, _) = makeVM(client: client)
        vm.selectedModel = "Qwen3-8B"

        let runTask = Task { await vm.run() }

        while !client.runIsWaiting {
            await Task.yield()
        }
        XCTAssertTrue(vm.starting)

        await vm.cancelStart()
        XCTAssertTrue(client.stopCalled)
        XCTAssertTrue(vm.cancelling)

        // Cancel is genuinely in progress: the primary control must be
        // disabled so the user can't double-fire stop/cancel.
        XCTAssertFalse(vm.canStopOrCancel)

        // Release the gated run with a SUCCESS outcome (no runError) -- the
        // load actually completed just as/after cancel fired.
        client.releaseRun()
        await runTask.value

        // Cancel must win: idle, not serving, no leftover endpoint/error.
        XCTAssertEqual(vm.status, .idle)
        XCTAssertNil(vm.endpoint)
        XCTAssertNil(vm.errorText)
        XCTAssertFalse(vm.cancelling)
        XCTAssertFalse(vm.starting)
        XCTAssertFalse(vm.inFlight)
    }

    func testCancelStartNoOpWhenNotStarting() async {
        let client = FakeTTClient()
        let (vm, _) = makeVM(client: client)
        XCTAssertFalse(vm.starting)
        await vm.cancelStart()
        // Fresh box: no load in flight, so cancelStart must do nothing.
        XCTAssertFalse(client.stopCalled)
        XCTAssertFalse(vm.cancelling)
    }

    func testCanStopOrCancelGating() async {
        let (vm, _) = makeVM()
        XCTAssertFalse(vm.canStopOrCancel)
        vm.status = .serving(model: "m")
        XCTAssertTrue(vm.canStopOrCancel)
    }

    // MARK: - Shared ref-counted telemetry subscription

    /// Two views (DeviceStripView, BoxDetailView) will both call
    /// `subscribeTelemetry()`/`unsubscribeTelemetry()` on the same
    /// `BoxViewModel` -- this must collapse to exactly one underlying
    /// `TelemetryService.start()`/`stop()` pair no matter how many
    /// subscribers stack up, and must never double-stop once the count
    /// floors at zero.
    func testTelemetrySubscribeStartsOnceRefCounted() async {
        let (vm, _) = makeVM()
        var starts = 0, stops = 0
        vm.telemetry.onStart = { _, _, _ in starts += 1 }
        vm.telemetry.onStop = { stops += 1 }

        vm.subscribeTelemetry()          // 0->1: start
        vm.subscribeTelemetry()          // 1->2: no new start
        XCTAssertEqual(starts, 1)

        vm.unsubscribeTelemetry()        // 2->1: no stop
        XCTAssertEqual(stops, 0)

        vm.unsubscribeTelemetry()        // 1->0: stop
        XCTAssertEqual(stops, 1)

        vm.unsubscribeTelemetry()        // floor at 0: no extra stop
        XCTAssertEqual(stops, 1)
    }

    /// The shared subscription is what carries the thin-telemetry feature:
    /// it must request the lite stream (`?view=lite`), not the full
    /// `tt-smi -s` mirror.
    func testTelemetrySubscribeRequestsLite() async {
        let (vm, _) = makeVM()
        var liteSeen: Bool?
        vm.telemetry.onStart = { _, _, lite in liteSeen = lite }
        vm.subscribeTelemetry()
        XCTAssertEqual(liteSeen, true)
    }
}
