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
