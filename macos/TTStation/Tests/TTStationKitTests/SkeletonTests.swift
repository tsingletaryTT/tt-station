import XCTest
@testable import TTStationKit

final class SkeletonTests: XCTestCase {
    func testPackageBuilds() {
        XCTAssertEqual(TTStationKit.marker, "ttstation")
    }
}
