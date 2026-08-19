import Foundation

/// Executable checks for per-relation browse state, run by `--verify-browse-state`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum BrowseStateChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkATableNobodyVisitedReadsAsFresh()
        checkAnEmptyStateIsNotStored()
        checkTablesDoNotShareState()
        checkSavingOverATableReplacesIt()
        checkASelectionPastTheLoadedRowsIsDropped()
        checkConnectingElsewhereForgetsEverything()
        if failures == 0 {
            fputs("browse-state: all checks passed\n", stderr)
        } else {
            fputs("browse-state: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// Asking about a table that has never been opened answers, rather than
    /// making the caller handle a nil it would only turn into this anyway.
    private static func checkATableNobodyVisitedReadsAsFresh() {
        let store = BrowseStore()
        expect(store.state(for: "public.orders"), BrowseState(), "an unvisited table is fresh")
        expect(store.count, 0, "and asking did not create it")
    }

    /// The store holds tables somebody did something to, not tables somebody
    /// clicked. Without this every table looked at once would be remembered
    /// forever, for the sake of restoring two empty fields.
    private static func checkAnEmptyStateIsNotStored() {
        var store = BrowseStore()
        store.save(BrowseState(), for: "public.orders")
        expect(store.count, 0, "saving an empty state stores nothing")
    }

    /// The point of the whole type: two tables, two filters, neither
    /// overwriting the other.
    private static func checkTablesDoNotShareState() {
        var store = BrowseStore()
        store.save(BrowseState(whereClause: "id > 10"), for: "public.orders")
        store.save(BrowseState(whereClause: "name IS NULL"), for: "sales.regions")
        expect(store.state(for: "public.orders").whereClause, "id > 10", "the first kept its own")
        expect(
            store.state(for: "sales.regions").whereClause, "name IS NULL",
            "and so did the second")
        expect(store.count, 2, "two tables, two entries")
    }

    /// Saving replaces rather than merges — and clearing every field is a save
    /// that forgets, not a save that stores an empty one. Otherwise a user who
    /// deleted their filter would find it still counted as state.
    private static func checkSavingOverATableReplacesIt() {
        var store = BrowseStore()
        store.save(BrowseState(whereClause: "id > 10"), for: "public.orders")
        store.save(BrowseState(orderClause: "\"name\" DESC"), for: "public.orders")
        expect(store.state(for: "public.orders").whereClause, "", "the old WHERE is gone")
        expect(
            store.state(for: "public.orders").orderClause, "\"name\" DESC",
            "the new ORDER BY is here")
        store.save(BrowseState(), for: "public.orders")
        expect(store.count, 0, "and emptying it forgets the table")
    }

    /// A restored selection has to be a row that came back. The browse fetches a
    /// page at a time, so this is the ordinary case on any large table, not an
    /// edge one.
    private static func checkASelectionPastTheLoadedRowsIsDropped() {
        let state = BrowseState(selection: GridSelection(row: 5000, column: 2))
        expect(state.selection(within: 1000), nil, "row 5000 is not among the first 1000")
        expect(
            state.selection(within: 6000), GridSelection(row: 5000, column: 2),
            "and is restored once it is")

        // The row at the boundary, because "5000 rows loaded" means rows 0…4999.
        let last = BrowseState(selection: GridSelection(row: 999, column: 0))
        expect(
            last.selection(within: 1000), GridSelection(row: 999, column: 0),
            "the last loaded row counts as loaded")
    }

    /// `schema.name` names a different table on a different server, so the store
    /// cannot survive a reconnection.
    private static func checkConnectingElsewhereForgetsEverything() {
        var store = BrowseStore()
        store.save(BrowseState(whereClause: "id > 10"), for: "public.orders")
        store.clear()
        expect(store.count, 0, "reconnecting forgets every table")
        expect(
            store.state(for: "public.orders"), BrowseState(),
            "and the table reads as never visited")
    }

    // MARK: - Fixture

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("browse-state FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
