// swift-tools-version:5.5
import PackageDescription

let package = Package(
    name: "tauri-plugin-peckboard-native",
    platforms: [
        .macOS(.v10_13),
        .iOS(.v14),
    ],
    products: [
        .library(
            name: "tauri-plugin-peckboard-native",
            type: .static,
            targets: ["tauri-plugin-peckboard-native"])
    ],
    dependencies: [
        .package(name: "Tauri", path: "../.tauri/tauri-api")
    ],
    targets: [
        .target(
            name: "tauri-plugin-peckboard-native",
            dependencies: [
                .byName(name: "Tauri")
            ],
            path: "Sources",
            linkerSettings: [
                .linkedFramework("LocalAuthentication")
            ])
    ]
)
