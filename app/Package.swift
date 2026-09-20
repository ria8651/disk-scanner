// swift-tools-version: 6.2
import PackageDescription
import Foundation

// The Rust staticlib lives in the cargo target dir one level up. Resolving it
// from the manifest's own location keeps `swift build` working from anywhere.
let rustLib = Context.packageDirectory + "/../target/release"

let package = Package(
    name: "DiskScanner",
    platforms: [.macOS(.v26)],
    targets: [
        // The C ABI surface. Headers only; `shim.c` exists because SwiftPM
        // requires at least one source file in a C target.
        .target(name: "DiskScannerFFI"),

        .executableTarget(
            name: "DiskScanner",
            dependencies: ["DiskScannerFFI"],
            linkerSettings: [
                // unsafeFlags is fine here: this is a root package, never a
                // dependency of anything else.
                .unsafeFlags(["-L\(rustLib)"]),
                .linkedLibrary("disk_scanner"),
                .linkedLibrary("iconv"),
            ]
        ),
    ]
)
