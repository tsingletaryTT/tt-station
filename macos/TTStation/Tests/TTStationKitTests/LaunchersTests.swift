import XCTest
@testable import TTStationKit

/// Unit tests for the pure launcher builders (`OpenCodeLauncher`,
/// `OpenWebUILauncher`). These protect the exact JSON/argv/env/URL shapes the
/// front-ends depend on. The side-effecting glue (`LaunchController`) is
/// owner-verified by launching, not unit-tested.
final class LaunchersTests: XCTestCase {
    /// The endpoint under test, decoded from the shared fixture so these tests
    /// track the real `Endpoint` decoding path used everywhere else.
    /// (`Fixtures/endpoint.json` → base_url `http://192.168.5.119:8000/v1`,
    /// model `Qwen3-8B`.)
    private func endpoint() throws -> Endpoint {
        let url = Bundle.module.url(forResource: "endpoint", withExtension: "json", subdirectory: "Fixtures")
        let data = try Data(contentsOf: try XCTUnwrap(url))
        return try JSONDecoder().decode(Endpoint.self, from: data)
    }

    // MARK: OpenCodeLauncher

    func testOpenCodeConfigJSON() throws {
        let ep = try endpoint()
        let json = OpenCodeLauncher.configJSON(for: ep)
        let obj = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])

        XCTAssertEqual(obj["$schema"] as? String, "https://opencode.ai/config.json")
        XCTAssertEqual(obj["model"] as? String, "ttstation/\(ep.model)")

        let provider = try XCTUnwrap(obj["provider"] as? [String: Any])
        let tt = try XCTUnwrap(provider["ttstation"] as? [String: Any])
        XCTAssertEqual(tt["npm"] as? String, "@ai-sdk/openai-compatible")
        XCTAssertEqual(tt["name"] as? String, "TT Station")

        let options = try XCTUnwrap(tt["options"] as? [String: Any])
        XCTAssertEqual(options["baseURL"] as? String, ep.baseURL)

        let models = try XCTUnwrap(tt["models"] as? [String: Any])
        let entry = try XCTUnwrap(models[ep.model] as? [String: Any])
        XCTAssertEqual(entry["name"] as? String, "\(ep.model) (TT)")
    }

    func testOpenCodeTerminalCommand() {
        XCTAssertEqual(
            OpenCodeLauncher.terminalCommand(configDir: "/tmp/x"),
            "cd '/tmp/x' && opencode")
    }

    // MARK: OpenWebUILauncher

    func testOpenWebUIInvocation() throws {
        let ep = try endpoint()
        let inv = OpenWebUILauncher.invocation(for: ep)
        XCTAssertEqual(inv.executable, "uvx")
        XCTAssertEqual(inv.args, ["open-webui", "serve", "--port", "8080"])
        XCTAssertEqual(inv.env["OPENAI_API_BASE_URL"], ep.baseURL)
        XCTAssertEqual(inv.env["OPENAI_API_KEY"], "sk-none")
        XCTAssertEqual(inv.env["WEBUI_AUTH"], "false")
    }

    func testOpenWebUIURLs() {
        XCTAssertEqual(OpenWebUILauncher.url.absoluteString, "http://localhost:8080")
        XCTAssertEqual(OpenWebUILauncher.healthURL.absoluteString, "http://localhost:8080/health")
    }
}
