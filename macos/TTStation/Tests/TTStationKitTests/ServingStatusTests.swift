import XCTest
@testable import TTStationKit

final class ServingStatusTests: XCTestCase {
    func testIdle() throws {
        XCTAssertEqual(try ServingStatus(raw: "idle"), .idle)
    }
    func testServing() throws {
        XCTAssertEqual(try ServingStatus(raw: "serving:Qwen3-8B"), .serving(model: "Qwen3-8B"))
    }
    func testServingKeepsColonsInModel() throws {
        XCTAssertEqual(try ServingStatus(raw: "serving:a:b"), .serving(model: "a:b"))
    }
    func testInvalidThrows() {
        XCTAssertThrowsError(try ServingStatus(raw: "bogus"))
    }
    func testIsServing() throws {
        XCTAssertFalse(try ServingStatus(raw: "idle").isServing)
        XCTAssertTrue(try ServingStatus(raw: "serving:x").isServing)
    }
}
