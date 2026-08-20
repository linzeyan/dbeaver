import Foundation

/// Executable checks for browse state surviving a change of table, run by
/// `--verify-browse-restore`.
///
/// Separate from `BrowseStateChecks`, which pins the store as a rule with no
/// window behind it. These drive a real `AppModel`, because the defect this item
/// exists to fix is not in the store — it is in which of `selectionChanged`'s
/// assignments happens first, and no check on the store alone can see that.
///
/// No database is needed. `AppModel.run` returns without dispatching when there
/// is no connection, so selecting a relation performs the state half of
/// `selectionChanged` and none of the round trips.
enum BrowseRestoreChecks {
    private static var failures = 0

    static func run() -> Bool {
        // Point the config at a scratch directory before building a model, the
        // way `AppModelConnectionChecks` does. Without it the model reads the
        // user's saved connections, and asks the Keychain for the password of
        // the first — which blocks forever in a process with no GUI session, so
        // the symptom is not a failed check but a `make test-swift` that never
        // returns.
        guard let scratch = scratchDirectory() else { return false }
        defer { try? FileManager.default.removeItem(at: scratch) }
        setenv("XDG_CONFIG_HOME", scratch.path, 1)

        failures = 0
        checkAFreshTableOpensUnfiltered()
        checkComingBackBringsTheFilter()
        checkTablesDoNotShareFilters()
        checkVisitingTablesFillsThePath()
        checkSwitchingTabIsAMove()
        if failures == 0 {
            fputs("browse-restore: all checks passed\n", stderr)
        } else {
            fputs("browse-restore: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The behaviour that must not regress: a table nobody has filtered opens
    /// showing everything. Carrying the previous table's WHERE into it would
    /// name columns it does not have.
    private static func checkAFreshTableOpensUnfiltered() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            model.whereClause = "id > 10"
            model.selected = regions
            expect(model.whereClause, "", "a table opened for the first time has no filter")
        }
    }

    /// The item itself. Comparing two tables is the core loop of schema work,
    /// and until now every A→B→A round trip re-typed the filter.
    private static func checkComingBackBringsTheFilter() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            model.whereClause = "id > 10"
            model.orderClause = "\"name\" DESC"
            model.selected = regions
            model.selected = orders
            expect(model.whereClause, "id > 10", "coming back brings the WHERE")
            expect(model.orderClause, "\"name\" DESC", "and the ORDER BY")
        }
    }

    /// Each table keeps its own. One remembered filter applied to whichever
    /// table came next would be worse than the clearing this replaced.
    private static func checkTablesDoNotShareFilters() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            model.whereClause = "id > 10"
            model.selected = regions
            model.whereClause = "code IS NULL"
            model.selected = orders
            expect(model.whereClause, "id > 10", "the first table's filter is its own")
            model.selected = regions
            expect(model.whereClause, "code IS NULL", "and so is the second's")
        }
    }

    /// Selecting tables is what fills the path. These pin the recording rather
    /// than the walking: `BrowseHistoryChecks` already walks a path, and Back
    /// cannot be driven from here because it resolves the relation through the
    /// sidebar, which a model with no connection has none of.
    private static func checkVisitingTablesFillsThePath() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(model.canGoBack, false, "a window that has opened nothing cannot go back")
            model.selected = orders
            expect(model.canGoBack, false, "nor can one that has opened its first table")
            model.selected = regions
            expect(model.canGoBack, true, "the second table is what gives Back somewhere to go")
            expect(model.canGoForward, false, "with nothing ahead of it")
        }
    }

    /// Switching tab is moving too, so Back from a table's rows means that
    /// table's structure rather than the table before it.
    private static func checkSwitchingTabIsAMove() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            expect(model.canGoBack, false, "one table on one tab is one place")
            model.activeTab = .structure
            expect(model.canGoBack, true, "and the same table on another tab is a second")
        }
    }

    // MARK: - Fixture

    /// A directory of its own for the config this check must not read.
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-browse-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs(
                "browse-restore FAIL: a scratch directory could not be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: nil)
    private static let regions = RelationInfo(
        schema: "sales", name: "regions", kind: .table, estimatedRows: nil)

    /// A model with no connection, built the way `AppModelConnectionChecks`
    /// builds its own: a throwaway defaults suite, so that running the checks
    /// cannot read or write the history the user's windows share.
    @MainActor private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: UserDefaults(suiteName: UUID().uuidString)!)
        return AppModel(history: history, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("browse-restore FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
