import XCTest
@testable import TTStationKit

final class TTClientTests: XCTestCase {
    func testDiscoverBuildsArgsAndDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(
            stdout: Data(#"[{"name":"b","host":"h","ctrl_port":8080,"chips":"4xBH","status":"idle","apiver":1}]"#.utf8),
            stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let boxes = try await client.discover(manualHosts: ["h:8080"], noMdns: true)
        XCTAssertEqual(fake.lastArgs, ["--json", "discover", "--host", "h:8080", "--no-mdns"])
        XCTAssertEqual(boxes.first?.status, .idle)
    }

    func testStatusDecodesWrappedString() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"status":"serving:Qwen3-8B"}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let status = try await client.status(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "status", "--host", "h:8080"])
        XCTAssertEqual(status, .serving(model: "Qwen3-8B"))
    }

    func testModelsDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"release_version":null,"models":[{"name":"Qwen3-8B","devices":["P300X2"]}]}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let models = try await client.models(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "models", "--host", "h:8080"])
        XCTAssertEqual(models.map(\.name), ["Qwen3-8B"])
    }

    func testServingBuildsArgsAndDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(
            stdout: Data(#"{"serving":[{"model":"Qwen3-8B","base_url":"http://h:8000/v1","host_port":8000,"container":"agent-c","source":"agent"},{"model":"Llama","base_url":"http://h:8001/v1","host_port":8001,"container":"tt-studio-c","source":"external"}]}"#.utf8),
            stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let serving = try await client.serving(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "serving", "--host", "h:8080"])
        XCTAssertEqual(serving.count, 2)
        XCTAssertEqual(serving.map(\.model), ["Qwen3-8B", "Llama"])
        XCTAssertEqual(serving.map(\.source), ["agent", "external"])
        XCTAssertEqual(serving[1].hostPort, 8001)
    }

    func testConfigBuildsArgsAndDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(
            stdout: Data(#"{"active_profile":"stable","available_profiles":["stable","bleeding"],"backend":"runpy","serving_host":"qb2-lab.local","serving_port":8003,"serving_image":"ghcr.io/x:0.14.0","tt_inference_repo":"/home/ttuser/code/tt-inference-server","tt_device":"p300x2"}"#.utf8),
            stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let cfg = try await client.config(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "config", "--host", "h:8080"])
        XCTAssertEqual(cfg.activeProfile, "stable")
        XCTAssertEqual(cfg.availableProfiles, ["stable", "bleeding"])
        XCTAssertEqual(cfg.backend, "runpy")
        XCTAssertEqual(cfg.servingHost, "qb2-lab.local")
        XCTAssertEqual(cfg.servingPort, 8003)
        XCTAssertEqual(cfg.ttDevice, "p300x2")
    }

    func testNonZeroExitThrowsWithStderr() async {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(), stderr: "no token stored for h:8080", exitCode: 1)
        let client = TTClient(runner: fake)
        do {
            _ = try await client.endpoint(host: "h:8080")
            XCTFail("expected throw")
        } catch {
            XCTAssertEqual(error as? TTError,
                .commandFailed(command: ["--json", "endpoint", "--host", "h:8080"], exitCode: 1, stderr: "no token stored for h:8080"))
        }
    }
}

extension TTClientTests {
    func testPairBuildsArgs() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"host":"h:8080","paired":true,"token":"deadbeef"}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let result = try await client.pair(host: "h:8080", code: "042817")
        XCTAssertEqual(fake.lastArgs, ["--json", "pair", "h:8080", "--code", "042817"])
        XCTAssertTrue(result.paired)
    }

    func testRunBuildsArgsModelFirst() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"base_url":"http://h:8000/v1","model":"Qwen3-8B","requires_key":false}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let ep = try await client.run(host: "h:8080", model: "Qwen3-8B")
        XCTAssertEqual(fake.lastArgs, ["--json", "run", "Qwen3-8B", "--host", "h:8080"])
        XCTAssertEqual(ep.model, "Qwen3-8B")
    }

    func testStopBuildsArgsAndIgnoresBody() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data("{}".utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        try await client.stop(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "stop", "--host", "h:8080"])
    }

    func testIsAuthError() {
        let client = TTClient(runner: FakeProcessRunner())
        XCTAssertTrue(client.isAuthError(.commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080; run `tt pair`")))
        XCTAssertFalse(client.isAuthError(.commandFailed(command: [], exitCode: 1, stderr: "connection refused")))
    }
}
