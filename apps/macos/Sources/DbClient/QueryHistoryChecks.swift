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
            checkTheCapIsWhateverTheStoreSays()
            checkHalfTheListIsHeldForTypedStatements()
            checkZeroKeepsEverything()
            checkLoweringTheCapShortensTheFile()
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
        history.record(
            "SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 12.5)
        expect(history.entries.first?.origin, .browse, "the entry says what caused it")
        expect(history.entries.first?.milliseconds, 12.5, "and what the server took")
    }

    /// The rule that was already here: ⌘R four times while fixing a table leaves
    /// one entry, not four.
    @MainActor private static func checkTheSameStatementFromOneOriginReplacesItself() {
        let history = make()
        history.record(
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record(
            "SELECT 1", from: .query, outcome: .rows(2), milliseconds: 2)
        expect(history.entries.count, 1, "the second run replaces the first")
        expect(history.entries.first?.outcome, .rows(2), "with the newer answer")
    }

    /// And the part the origin adds to it. The same SELECT can be typed and can
    /// be what the browse sends, and folding them together would answer "did I
    /// run this or did the sidebar" with whichever came second.
    @MainActor private static func checkTheSameStatementFromTwoOriginsIsTwoEntries() {
        let history = make()
        history.record(
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record(
            "SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 1)
        expect(history.entries.count, 2, "one statement from two places is two entries")
    }

    /// The reason the untyped cap exists. A browse runs every time a table is
    /// picked, so without it an afternoon in the sidebar would push out the
    /// statement somebody typed — the one thing this store exists to give back.
    @MainActor private static func checkBrowsesCannotEvictATypedStatement() {
        let history = make()
        history.record(
            "SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1)
        for i in 0..<(history.untypedLimit + 20) {
            history.record(
                "SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.contains { $0.sql == "SELECT typed" }, true,
            "the typed statement is still there")
        expect(
            history.entries.filter { $0.origin != .query }.count, history.untypedLimit,
            "and the untyped ones are held at their own cap")
    }

    /// Which untyped entry goes is the other half of the rule: the oldest, so
    /// the list stays a record of what just happened.
    @MainActor private static func checkTheOldestUntypedGoesFirst() {
        let history = make()
        for i in 0..<(history.untypedLimit + 1) {
            history.record(
                "SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.contains { $0.sql == "SELECT browse 0" }, false,
            "the first browse is the one that went")
        expect(
            history.entries.contains { $0.sql == "SELECT browse \(history.untypedLimit)" },
            true, "and the newest is the one that stayed")
    }

    /// The total cap is still a cap. Typed statements are protected from
    /// browses, not from each other.
    @MainActor private static func checkTheWholeListIsStillCapped() {
        let history = make()
        for i in 0..<(history.limit + 10) {
            history.record(
                "SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1)
        }
        expect(history.entries.count, history.limit, "the list stops at its limit")
        expect(history.entries.first?.sql, "SELECT \(history.limit + 9)", "newest first")
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
        history.record(
            "SELECT 1", from: .browse, outcome: .rows(3), milliseconds: 12)
        let script = QueryHistory.script(history.entries)
        expect(script.contains("-- browse ·"), true, "the comment names the origin")
        expect(script.contains("12 ms"), true, "and what it took")
        expect(script.contains("3 rows"), true, "and what came back")
    }

    /// A step may or may not arrive carrying its own semicolon, and both `;;` and
    /// a bare statement are ways for the file to fail to run.
    @MainActor private static func checkTheScriptTerminatesEveryStatementExactlyOnce() {
        let history = make()
        history.record(
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1)
        history.record(
            "SELECT 2;", from: .query, outcome: .rows(1), milliseconds: 1)
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
        model.history.record(
            "SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1)
        model.history.record(
            "SELECT browsed", from: .browse, outcome: .rows(1), milliseconds: 1)
        expect(model.shownHistory.count, 1, "the browse is out while All is off")
        expect(model.canExportHistory, true, "and there is still something to write")

        model.showsAllStatements = true
        expect(model.shownHistory.count, 2, "All puts it back")

        model.historyFilter = "BROWSED"
        expect(model.shownHistory.count, 1, "the filter matches whatever the case")

        model.historyFilter = "nothing matches this"
        expect(model.canExportHistory, false, "and a filter that hides everything empties the log")
    }

    // MARK: - How many are kept

    /// The cap is whatever the store says, not a number in this file.
    ///
    /// Driven by setting the preference rather than the key, because that is the
    /// only path there is: the Settings window and the histories never meet, and
    /// the setting reaches them by being written where they read. A check that
    /// spelled the key itself would be a second copy of it, and a check that
    /// assigned some property on the instance would be testing a door nothing
    /// opens.
    @MainActor private static func checkTheCapIsWhateverTheStoreSays() {
        let store = ScratchDefaults.store("verify-query-history-cap")
        let preferences = Preferences(store: store)
        preferences.historyLimit = 5

        let history = QueryHistory(defaults: store)
        expect(history.limit, 5, "the cap is read from the store the entries are in")
        for i in 0..<12 {
            history.record("SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1)
        }
        expect(history.entries.count, 5, "and it is the number actually kept")
        expect(history.entries.first?.sql, "SELECT 11", "with the newest still at the front")

        // A number nobody could type into the field, but a plist is a file
        // somebody edits. Read back through a second `Preferences` because the
        // fold is on the way in, not on the way to the store.
        preferences.historyLimit = -5
        expect(
            Preferences(store: store).historyLimit, 0,
            "a negative reads as no cap rather than as a cap below none")
    }

    /// Half the list is held for typed statements, whatever the cap is.
    ///
    /// Said as literals rather than through `untypedLimit`, because a check that
    /// asked the property for the number it expects would agree with whatever
    /// the property answered.
    @MainActor private static func checkHalfTheListIsHeldForTypedStatements() {
        let store = ScratchDefaults.store("verify-query-history-half")
        let preferences = Preferences(store: store)
        preferences.historyLimit = 10

        let history = QueryHistory(defaults: store)
        history.record("SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1)
        for i in 0..<20 {
            history.record("SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.filter { $0.origin != .query }.count, 5,
            "five browses of a ten-entry list, and no more")
        expect(
            history.entries.contains { $0.sql == "SELECT typed" }, true,
            "so the statement somebody typed twenty browses ago is still here")
    }

    /// Zero keeps everything, which is the only thing an emptied field could
    /// honestly mean for a cap.
    ///
    /// The untyped half of the rule is the part worth checking: that cap is half
    /// the total, and half of nothing has to be nothing rather than none.
    @MainActor private static func checkZeroKeepsEverything() {
        let store = ScratchDefaults.store("verify-query-history-uncapped")
        let preferences = Preferences(store: store)
        preferences.historyLimit = 0

        let history = QueryHistory(defaults: store)
        for i in 0..<(QueryHistory.defaultLimit + 20) {
            history.record("SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1)
        }
        for i in 0..<40 {
            history.record("SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1)
        }
        expect(
            history.entries.count, QueryHistory.defaultLimit + 60,
            "nothing was dropped, typed or otherwise")
    }

    /// Lowering the cap shortens what is on the disk, not only what is in the
    /// list.
    ///
    /// The cap is the only thing bounding how long a statement stays in a file
    /// nothing encrypts, so a cap that shortened the list in memory and left the
    /// file as it was would be doing the one job it has badly. Read back through
    /// the store for that reason.
    @MainActor private static func checkLoweringTheCapShortensTheFile() {
        let store = ScratchDefaults.store("verify-query-history-lowered")
        let preferences = Preferences(store: store)
        preferences.historyLimit = 20

        let first = QueryHistory(defaults: store)
        for i in 0..<20 {
            first.record("SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1)
        }
        expect(first.entries.count, 20, "twenty were kept under the old cap")

        preferences.historyLimit = 3
        let next = QueryHistory(defaults: store)
        expect(next.entries.count, 3, "and the next launch holds what is left to the new one")
        expect(next.entries.first?.sql, "SELECT 19", "keeping the newest, as the rule says")

        let data = store.data(forKey: QueryHistory.key) ?? Data()
        let stored = (try? JSONDecoder().decode([QueryHistoryEntry].self, from: data))?.count ?? -1
        expect(stored, 3, "and the file agrees, which is the only part that bounds anything")
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
