import XCTest
@testable import TTStationKit

final class ModelsTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let url = Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures")
        return try Data(contentsOf: try XCTUnwrap(url))
    }

    func testDecodeDiscover() throws {
        let boxes = try JSONDecoder().decode([BoxRecord].self, from: fixture("discover"))
        XCTAssertEqual(boxes.count, 1)
        XCTAssertEqual(boxes[0].name, "quietbox2")
        XCTAssertEqual(boxes[0].ctrlPort, 8080)
        XCTAssertEqual(boxes[0].status, .serving(model: "Qwen3-8B"))
    }

    func testDecodeModelsWithReleaseVersion() throws {
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: fixture("models"))
        XCTAssertEqual(resp.releaseVersion, "0.14.0")
        XCTAssertEqual(resp.models.map(\.name), ["Qwen3-8B", "Llama-3.1-8B-Instruct"])
        XCTAssertEqual(resp.models[1].devices, ["P300X2", "T3K"])
    }

    func testDecodeModelsNullReleaseVersion() throws {
        let data = Data(#"{"release_version":null,"models":[]}"#.utf8)
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: data)
        XCTAssertNil(resp.releaseVersion)
        XCTAssertTrue(resp.models.isEmpty)
    }

    func testHostPortStripsTrailingDot() {
        let dotted = BoxRecord(name: "b", host: "qb2-lab.local.", ctrlPort: 8765, chips: "x", statusRaw: "idle", apiver: 1)
        XCTAssertEqual(dotted.hostPort, "qb2-lab.local:8765")

        let plain = BoxRecord(name: "b", host: "qb2-lab.local", ctrlPort: 8765, chips: "x", statusRaw: "idle", apiver: 1)
        XCTAssertEqual(plain.hostPort, "qb2-lab.local:8765")
    }

    func testDecodeEndpoint() throws {
        let ep = try JSONDecoder().decode(Endpoint.self, from: fixture("endpoint"))
        XCTAssertEqual(ep.baseURL, "http://192.168.5.119:8000/v1")
        XCTAssertEqual(ep.model, "Qwen3-8B")
        XCTAssertFalse(ep.requiresKey)
    }
}
