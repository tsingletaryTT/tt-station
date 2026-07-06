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

    func testOpenWebUIDockerCommand() {
        // Open WebUI runs ON THE BOX as a docker container wired to the box's
        // local vLLM on the given serving port. The command must be idempotent
        // (reuse a running container), publish the host port, reach the host
        // vLLM via host.docker.internal, and persist a named data volume.
        let cmd = OpenWebUILauncher.dockerCommand(servingPort: 8003)
        XCTAssertTrue(cmd.contains("docker run -d --name ttstation-openwebui"), cmd)
        // Idempotent reuse: bail if the container is already running.
        XCTAssertTrue(cmd.contains("docker inspect -f '{{.State.Running}}' ttstation-openwebui"), cmd)
        // Publish host :3000 → container :8080.
        XCTAssertTrue(cmd.contains("-p 3000:8080"), cmd)
        // Reach the box's vLLM from inside the container.
        XCTAssertTrue(cmd.contains("--add-host=host.docker.internal:host-gateway"), cmd)
        XCTAssertTrue(cmd.contains("OPENAI_API_BASE_URL=http://host.docker.internal:8003/v1"), cmd)
        XCTAssertTrue(cmd.contains("WEBUI_AUTH=false"), cmd)
        XCTAssertTrue(cmd.contains("-v ttstation-openwebui:/app/backend/data"), cmd)
        XCTAssertTrue(cmd.contains("ghcr.io/open-webui/open-webui:main"), cmd)
        // First-run pull is retried (ghcr.io over IPv6 was observed flaky), and
        // only when the image isn't already local.
        XCTAssertTrue(cmd.contains("docker image inspect"), cmd)
        XCTAssertTrue(cmd.contains("docker pull ghcr.io/open-webui/open-webui:main"), cmd)
    }

    func testOpenWebUIURLs() {
        // URLs are keyed to the box host and the published host port (3000).
        XCTAssertEqual(
            OpenWebUILauncher.url(host: "qb2-lab.local").absoluteString,
            "http://qb2-lab.local:3000")
        XCTAssertEqual(
            OpenWebUILauncher.healthURL(host: "qb2-lab.local").absoluteString,
            "http://qb2-lab.local:3000/health")
    }
}
