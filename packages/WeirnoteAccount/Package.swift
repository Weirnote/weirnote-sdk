// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WeirnoteAccount",
    platforms: [.macOS(.v13), .iOS(.v16)],
    products: [
        .library(name: "DeviceEnrollment", targets: ["DeviceEnrollment"]),
    ],
    targets: [
        .target(name: "DeviceEnrollment"),
        .testTarget(name: "DeviceEnrollmentTests", dependencies: ["DeviceEnrollment"]),
    ]
)
