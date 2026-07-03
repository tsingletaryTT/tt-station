import XCTest
@testable import TTStationKit

final class BinaryLocatorTests: XCTestCase {
    func testPrefersOverrideWhenItExists() throws {
        let loc = TTBinaryLocator(override: "/custom/tt", candidates: ["/a/tt"]) { $0 == "/custom/tt" }
        XCTAssertEqual(try loc.locate(), "/custom/tt")
    }
    func testFallsBackToFirstExistingCandidate() throws {
        let loc = TTBinaryLocator(override: nil, candidates: ["/a/tt", "/b/tt"]) { $0 == "/b/tt" }
        XCTAssertEqual(try loc.locate(), "/b/tt")
    }
    func testSkipsMissingOverride() throws {
        let loc = TTBinaryLocator(override: "/missing/tt", candidates: ["/b/tt"]) { $0 == "/b/tt" }
        XCTAssertEqual(try loc.locate(), "/b/tt")
    }
    func testThrowsListingAllTriedWhenNoneExist() {
        let loc = TTBinaryLocator(override: "/x/tt", candidates: ["/a/tt", "/b/tt"]) { _ in false }
        XCTAssertThrowsError(try loc.locate()) { error in
            XCTAssertEqual(error as? TTError, .binaryNotFound(triedPaths: ["/x/tt", "/a/tt", "/b/tt"]))
        }
    }
}
