// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TTStationKit",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "TTStationKit", targets: ["TTStationKit"]),
    ],
    targets: [
        .target(name: "TTStationKit"),
        .testTarget(
            name: "TTStationKitTests",
            dependencies: ["TTStationKit"],
            resources: [.copy("Fixtures")]
        ),
    ]
)
