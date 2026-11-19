// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WeirnotePythonAccount",
    platforms: [.macOS(.v14)],
    dependencies: [.package(path: "../WeirnoteAccount")],
    targets: [
        .executableTarget(name: "WeirnotePythonAccount",
                          dependencies: [.product(name: "DeviceEnrollment", package: "WeirnoteAccount")]),
        .testTarget(name: "WeirnotePythonAccountTests", dependencies: ["WeirnotePythonAccount"]),
    ]
)
