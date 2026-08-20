import Foundation

/// Executable checks for the statement history, run by `--verify-query-history`.
///
/// The list itself has been checked by captures until now — `--history-store`
/// and `--history-pick` drive it through a real window. What captures cannot see
/// is which entry survives when the list is full, and that is the whole of the
/// two caps: a rule about eviction shows nothing on screen until the day it
/// evicts the wrong thing.
///
/// No database and no window. `QueryHistory` is a store over a defaults suite,
/// and a scratch suite is all it takes to drive.
enum QueryHistoryChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        MainActor.assumeIsolated {
            checkAnEntryKeepsWhatCausedItAndWhatItTook()
            checkTheSameStatementFromOneOriginReplacesItself()
            checkTheSameStatementFromTwoOriginsIsTwoEntries()
            checkBrowsesCannotEvictATypedStatement()
            checkTheOldestUntypedGoesFirst()
            checkTheWholeListIsStillCapped()
            checkAnEmptyStatementIsNotAStatement()
            checkTheScriptSaysWhatCausedEachStatement()
            checkTheScriptTerminatesEveryStatementExactlyOnce()
            checkAnUnmeasuredStatementGetsNoDuration()
            checkTheLogIsWhatThePanelIsShowing()
        }
        if failures == 0 {
            fputs("query-history: all checks passed\n", stderr)
        } else {
            fputs("query-history: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// Both new facts survive the round trip. The duration is the one most
    /// likely to be dropped silently, because nothing refuses to draw a zero.
    @MainActor private static func checkAnEntryKeepsWhatCausedItAndWhatItTook() {
        let history = make()
        history.record("SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 12.5)
        expect(history.entries.first?.origin, .browse, "the entry says what caused it")
        expect(history.entries.first?.milliseconds, 12.5, "and what the server took")
    }

    /// The rule that was already here: ⌘R four times while fixing a table leaves
    /// one entry, not four.
    @MainActor private static func checkTheSameStatementFromOneOriginReplacesItself() {
        let history = make()
        history.record("SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record("SELECT 1", from: .query, outcome: .rows(2), milliseconds: 2)
        expect(history.entries.count, 1, "the second run replaces the first")
        expect(history.entries.first?.outcome, .rows(2), "with the newer answer")
    }

    /// And the part the origin adds to it. The same SELECT can be typed and can
    /// be what the browse sends, and folding them together would answer "did I
    /// run this or did the sidebar" with whichever came second.
    @MainActor private static func checkTheSameStatementFromTwoOriginsIsTwoEntries() {
        let history = make()
        history.record("SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record("SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 1)
        expect(history.entries.count, 2, "one statement from two places is two entries")
    }

    /// The reason the untyped cap exists. A browse runs every time a table is
    /// picked, so without it an afternoon in the sidebar would push out the
    /// statement somebody typed — the one thing this store exists to give back.
    @MainActor private static func checkBrowsesCannotEvictATypedStatement() {
        let history = make()
        history.record("SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1)
        for i in 0..<(QueryHistory.untypedLimit + 20) {
            history.record("SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.contains { $0.sql == "SELECT typed" }, true,
            "the typed statement is still there")
        expect(
            history.entries.filter { $0.origin != .query }.count, QueryHistory.untypedLimit,
            "and the untyped ones are held at their own cap")
    }

    /// Which untyped entry goes is the other half of the rule: the oldest, so
    /// the list stays a record of what just happened.
    @MainActor private static func checkTheOldestUntypedGoesFirst() {
        let history = make()
        for i in 0..<(QueryHistory.untypedLimit + 1) {
            history.record("SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.contains { $0.sql == "SELECT browse 0" }, false,
            "the first browse is the one that went")
        expect(
            history.entries.contains { $0.sql == "SELECT browse \(QueryHistory.untypedLimit)" },
            true, "and the newest is the one that stayed")
    }

    /// The total cap is still a cap. Typed statements are protected from
    /// browses, not from each other.
    @MainActor private static func checkTheWholeListIsStillCapped() {
        let history = make()
        for i in 0..<(QueryHistory.limit + 10) {
            history.record("SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1)
        }
        expect(history.entries.count, QueryHistory.limit, "the list stops at its limit")
        expect(history.entries.first?.sql, "SELECT \(QueryHistory.limit + 9)", "newest first")
    }

    /// Whitespace is not a statement. It was true before and it is the kind of
    /// thing a rewrite of `record` drops.
    @MainActor private static func checkAnEmptyStatementIsNotAStatement() {
        let history = make()
        history.record("   \n ", from: .query, outcome: .rows(0), milliseconds: 0)
        expect(history.entries.count, 0, "nothing was recorded")
    }

    /// The comment above each statement is the whole reason this is a file and
    /// not a paste of the SQL: without it there is no way to tell the SELECT
    /// somebody typed from the one the sidebar sent.
    @MainActor private static func checkTheScriptSaysWhatCausedEachStatement() {
        let history = make()
        history.record("SELECT 1", from: .browse, outcome: .rows(3), milliseconds: 12)
        let script = QueryHistory.script(history.entries)
        expect(script.contains("-- browse ·"), true, "the comment names the origin")
        expect(script.contains("12 ms"), true, "and what it took")
        expect(script.contains("3 rows"), true, "and what came back")
    }

    /// A step may or may not arrive carrying its own semicolon, and both `;;` and
    /// a bare statement are ways for the file to fail to run.
    @MainActor private static func checkTheScriptTerminatesEveryStatementExactlyOnce() {
        let history = make()
        history.record("SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record("SELECT 2;", from: .query, outcome: .rows(1), milliseconds: 1)
        let script = QueryHistory.script(history.entries)
        expect(script.contains("SELECT 1;"), true, "the bare statement gains one")
        expect(
            script.contains("SELECT 2;;"), false, "and the terminated one does not gain a second")
    }

    /// Zero means nobody measured it — an edit's statements are not timed one by
    /// one — so the file leaves it out rather than claiming the fastest run on
    /// the list.
    @MainActor private static func checkAnUnmeasuredStatementGetsNoDuration() {
        let history = make()
        history.record(
            "DELETE FROM t WHERE id = 1", from: .edit, outcome: .affected(1),
            milliseconds: 0)
        expect(QueryHistory.script(history.entries).contains("0 ms"), false, "no duration is shown")
    }

    /// The file is the panel, not the store. Both narrowings decide what gets
    /// written, and `canExportHistory` has to agree with them or the menu item
    /// stays live over a log with nothing in it.
    @MainActor private static func checkTheLogIsWhatThePanelIsShowing() {
        let model = makeModel()
        model.history.record("SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1)
        model.history.record("SELECT browsed", from: .browse, outcome: .rows(1), milliseconds: 1)
        expect(model.shownHistory.count, 1, "the browse is out while All is off")
        expect(model.canExportHistory, true, "and there is still something to write")

        model.showsAllStatements = true
        expect(model.shownHistory.count, 2, "All puts it back")

        model.historyFilter = "BROWSED"
        expect(model.shownHistory.count, 1, "the filter matches whatever the case")

        model.historyFilter = "nothing matches this"
        expect(model.canExportHistory, false, "and a filter that hides everything empties the log")
    }

    // MARK: - Fixture

    /// A model over throwaway suites, built the way `FilterRowChecks` builds its
    /// own. Needed by the one case that is about the panel rather than about the
    /// store — the two narrowings live on the model.
    @MainActor private static func makeModel() -> AppModel {
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-query-history"))
        return AppModel(history: make(), favorites: favorites, preferences: Preferences())
    }

    /// A store over a throwaway defaults suite, so running the checks cannot
    /// read or write the history the user's windows share.
    @MainActor private static func make() -> QueryHistory {
        QueryHistory(defaults: ScratchDefaults.store("verify-query-history"))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("query-history FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
