// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "CrossflowMac",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "CrossflowMac", targets: ["CrossflowMac"])],
    targets: [
        .executableTarget(name: "CrossflowMac"),
        .testTarget(name: "CrossflowMacTests", dependencies: ["CrossflowMac"])
    ]
)
