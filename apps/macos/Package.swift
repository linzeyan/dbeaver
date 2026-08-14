// swift-tools-version: 6.0
import PackageDescription

// The Rust staticlib is built separately (see the Makefile) and linked from the
// workspace target directory. RUST_PROFILE lets a debug build link the debug
// staticlib instead of silently picking up a stale release one.
let rustProfile = Context.environment["RUST_PROFILE"] ?? "release"
let rustLibDir = "../../target/\(rustProfile)"

let package = Package(
    name: "DbClient",
    // v15 for Int128, which decimal formatting needs to stay exact. There is no
    // reason for a new native client to target older than that.
    platforms: [.macOS(.v15)],
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
                // The Keychain, which is where connection passwords live. See
                // Connection.swift for why they are not in UserDefaults.
                .linkedFramework("Security"),
                // DuckDB is C++ compiled into the staticlib, and a Rust
                // staticlib does not record that its contents need the C++
                // runtime. Without this the link fails on `___cxa_throw` and
                // friends, from a Swift executable that contains no C++ at all.
                .linkedLibrary("c++"),
            ]
        ),
    ]
)
