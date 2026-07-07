import XCTest
@testable import TTStationKit

/// The mesh → product-artwork mapping. Only the QuietBox 2 (`p300x2`) has art
/// today; everything else (and nil/empty) has none.
final class DeviceArtworkTests: XCTestCase {
    func testQuietBox2MeshMapsToArtwork() {
        XCTAssertEqual(DeviceArtwork.assetName(forMesh: "p300x2"), "QuietBox2")
        XCTAssertEqual(DeviceArtwork.assetName(forMesh: "P300X2"), "QuietBox2")
        // A bare p300 (no card-count suffix) still reads as QuietBox 2.
        XCTAssertEqual(DeviceArtwork.assetName(forMesh: "p300"), "QuietBox2")
    }

    func testOtherMeshesHaveNoArtwork() {
        XCTAssertNil(DeviceArtwork.assetName(forMesh: "n300x4"))
        XCTAssertNil(DeviceArtwork.assetName(forMesh: "p150x4"))
        XCTAssertNil(DeviceArtwork.assetName(forMesh: "T3K"))
        XCTAssertNil(DeviceArtwork.assetName(forMesh: nil))
        XCTAssertNil(DeviceArtwork.assetName(forMesh: ""))
    }
}
