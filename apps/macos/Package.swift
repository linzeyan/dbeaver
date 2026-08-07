// swift-tools-version: 6.0
import PackageDescription

// The Rust staticlib is built separately (see the Makefile) and linked from the
// workspace target directory. RUST_PROFILE lets a debug build link the debug
// staticlib instead of silently picking up a stale release one.
let rustProfile = Context.environment["RUST_PROFILE"] ?? "release"
let rustLibDir = "../../target/\(rustProfile)"

let package = Package(
    name: "DbClient",
    platforms: [.macOS(.v14)],
    targets: [
        .systemLibrary(name: "CDbFfi"),
        .executableTarget(
            name: "DbClient",
            dependencies: ["CDbFfi"],
            // Phase 0 measures; it does not settle the concurrency model.
            // Swift 6 strict checking rejects passing raw Arrow pointers across
            // queues, which is correct — the fix is the event-queue FFI design
            // scheduled for phase 1, not annotations bolted onto a harness.
            // Tracked as debt, not silently accepted.
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .unsafeFlags(["-L\(rustLibDir)"]),
                .linkedLibrary("dbffi"),
                .linkedFramework("AppKit"),
                .linkedFramework("Metal"),
                .linkedFramework("MetalKit"),
                .linkedFramework("CoreText"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ]
)
