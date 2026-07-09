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

    func testStandardCandidatesAppendsBundledPathLast() {
        let c = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: "/App/TTStation.app/Contents/Resources/bin/tt")
        XCTAssertEqual(c, [
            "/Users/x/.local/bin/tt",
            "/opt/homebrew/bin/tt",
            "/usr/local/bin/tt",
            "/App/TTStation.app/Contents/Resources/bin/tt",
        ])
    }

    func testStandardCandidatesOmitsBundledPathWhenNil() {
        let c = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: nil)
        XCTAssertEqual(c, [
            "/Users/x/.local/bin/tt",
            "/opt/homebrew/bin/tt",
            "/usr/local/bin/tt",
        ])
    }

    func testBundledPathUsedOnlyWhenPATHCandidatesAbsent() throws {
        let candidates = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: "/App/tt")
        // Only the bundled path exists → it is returned.
        let onlyBundled = TTBinaryLocator(override: nil, candidates: candidates) { $0 == "/App/tt" }
        XCTAssertEqual(try onlyBundled.locate(), "/App/tt")
        // A PATH candidate exists → it wins over the bundled path.
        let pathWins = TTBinaryLocator(override: nil, candidates: candidates) { $0 == "/opt/homebrew/bin/tt" }
        XCTAssertEqual(try pathWins.locate(), "/opt/homebrew/bin/tt")
    }
}
