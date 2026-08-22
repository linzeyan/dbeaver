import Foundation

/// Executable checks for the driver marks, run by `--verify-driver-badge`.
///
/// The load-bearing one is `checkEveryDriverInTheCatalogIsNamed`. `DriverBadge`
/// is a table written by hand beside a table the core generates, and the way
/// that goes wrong is silent: a sixteenth driver lands in
/// `crates/ffi/src/registry.rs`, every connection to it works, and it draws a
/// cylinder and its own scheme in a column of two-letter marks. Nothing else in
/// this build would notice.
///
/// What is not pinned here is where the marks are drawn — that needs a window.
enum DriverBadgeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEveryDriverInTheCatalogIsNamed()
        checkAnUnknownSchemeFallsBackVisibly()
        checkTheFamiliesAreTheOnesTheTreeAndPanesBranchOn()
        if failures == 0 {
            fputs("driver badge: all checks passed\n", stderr)
        } else {
            fputs("driver badge: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// Every scheme the core reports has a mark of its own.
    ///
    /// Reads the catalogue rather than a list written here, which is the whole
    /// point: a second hand-written list would have the same drift problem one
    /// step further along.
    private static func checkEveryDriverInTheCatalogIsNamed() {
        let catalogue = DriverCatalog.all
        guard !catalogue.isEmpty else {
            failures += 1
            fputs(
                "driver badge FAIL: the core reported no drivers, so this suite proved nothing\n",
                stderr)
            return
        }
        for driver in catalogue where !DriverBadge.isMapped(scheme: driver.scheme) {
            failures += 1
            fputs(
                "driver badge FAIL: \(driver.scheme) (\(driver.label)) has no abbreviation"
                    + " — add it to DriverBadge\n", stderr)
        }
    }

    /// An unknown scheme is legible, not blank and not a placeholder.
    ///
    /// The abbreviation is the scheme itself, which is longer than two
    /// characters in a column of two — it is meant to look wrong.
    private static func checkAnUnknownSchemeFallsBackVisibly() {
        expect(
            DriverBadge.abbreviation(forScheme: "neo4j"), "neo4j",
            "an unmapped scheme is drawn under its own name")
        expect(
            DriverBadge.familySymbol(forScheme: "neo4j"), "cylinder",
            "and gets the generic shape rather than nothing")
        expect(
            DriverBadge.abbreviation(forScheme: ""), "",
            "and a connection with no scheme at all does not crash on the way to a mark")
        expect(
            DriverBadge.isMapped(scheme: "neo4j"), false,
            "the fallback is reported as a fallback, which is what the check above reads")
    }

    /// The shape is a claim about which family, and the families are the ones
    /// that decide the tree's levels and the pane set (ui-spec §3, §5.1). A
    /// mapping that put Redis under a cylinder would be promising a Structure
    /// tab that the driver refuses by name.
    private static func checkTheFamiliesAreTheOnesTheTreeAndPanesBranchOn() {
        expect(
            DriverBadge.familySymbol(forScheme: "postgres"), "cylinder",
            "a relational database over a socket is a cylinder")
        expect(
            DriverBadge.familySymbol(forScheme: "sqlite"),
            DriverBadge.familySymbol(forScheme: "duckdb"),
            "the two file-backed engines share a shape, because the form asks them both for a path"
        )
        expect(
            DriverBadge.familySymbol(forScheme: "sqlite")
                != DriverBadge.familySymbol(
                    forScheme: "postgres"), true,
            "and it is not the socket shape, which is the distinction that changes the form")
        expect(
            DriverBadge.familySymbol(forScheme: "redis"), "key",
            "key-value is not a table and does not draw as one")
        expect(
            DriverBadge.familySymbol(forScheme: "mongodb"), "curlybraces",
            "nor is a document store")
        expect(
            DriverBadge.familySymbol(forScheme: "trino"),
            DriverBadge.familySymbol(forScheme: "flightsql"),
            "the two engines that own none of what they read share a shape")
        expect(
            DriverBadge.familySymbol(forScheme: "snowflake"), "cloud",
            "and the warehouses share theirs")
    }

    // MARK: - Fixture

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("driver badge FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
