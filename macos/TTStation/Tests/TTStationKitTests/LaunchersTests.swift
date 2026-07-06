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
        let inv = OpenWebUILauncher.invocation(for: ep, dataDir: "/tmp/ttstation-owui")
        XCTAssertEqual(inv.executable, "uvx")
        // `--python 3.11` is pinned BEFORE the tool name so uv runs open-webui
        // on 3.11 (its supported version), which has prebuilt wheels for every
        // heavy dep (pyarrow/Arrow, chromadb). Without the pin, uv picks the
        // newest Python (e.g. 3.14) that lacks those wheels → source builds
        // (cmake + Apache Arrow) → the install stops being "fast".
        XCTAssertEqual(inv.args, ["--python", "3.11", "open-webui", "serve", "--port", "8080"])
        XCTAssertEqual(inv.env["OPENAI_API_BASE_URL"], ep.baseURL)
        XCTAssertEqual(inv.env["OPENAI_API_KEY"], "sk-none")
        XCTAssertEqual(inv.env["WEBUI_AUTH"], "false")
        // App-owned DATA_DIR + a matching sqlite DATABASE_URL so an ambient
        // DATABASE_URL in the user's shell can't be read as Open WebUI's DB
        // (which crashes startup with "Could not parse SQLAlchemy URL").
        XCTAssertEqual(inv.env["DATA_DIR"], "/tmp/ttstation-owui")
        XCTAssertEqual(inv.env["DATABASE_URL"], "sqlite:////tmp/ttstation-owui/webui.db")
    }

    func testOpenWebUIURLs() {
        XCTAssertEqual(OpenWebUILauncher.url.absoluteString, "http://localhost:8080")
        XCTAssertEqual(OpenWebUILauncher.healthURL.absoluteString, "http://localhost:8080/health")
    }
}
