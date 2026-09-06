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
            checkAPasswordTypedIntoTheQueryTabIsNotWrittenDown()
            checkTheStatementIsReadInTheDialectItWasSentIn()
            checkWhatAnEarlierBuildWroteIsSweptAtLaunch()
            checkAShapeThisBuildCannotReadIsTakenOffTheDisk()
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
            "SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 12.5, scheme: "sqlite")
        expect(history.entries.first?.origin, .browse, "the entry says what caused it")
        expect(history.entries.first?.milliseconds, 12.5, "and what the server took")
    }

    /// The rule that was already here: ⌘R four times while fixing a table leaves
    /// one entry, not four.
    @MainActor private static func checkTheSameStatementFromOneOriginReplacesItself() {
        let history = make()
        history.record(
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        history.record(
            "SELECT 1", from: .query, outcome: .rows(2), milliseconds: 2, scheme: "sqlite")
        expect(history.entries.count, 1, "the second run replaces the first")
        expect(history.entries.first?.outcome, .rows(2), "with the newer answer")
    }

    /// And the part the origin adds to it. The same SELECT can be typed and can
    /// be what the browse sends, and folding them together would answer "did I
    /// run this or did the sidebar" with whichever came second.
    @MainActor private static func checkTheSameStatementFromTwoOriginsIsTwoEntries() {
        let history = make()
        history.record(
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        history.record(
            "SELECT 1", from: .browse, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        expect(history.entries.count, 2, "one statement from two places is two entries")
    }

    /// The reason the untyped cap exists. A browse runs every time a table is
    /// picked, so without it an afternoon in the sidebar would push out the
    /// statement somebody typed — the one thing this store exists to give back.
    @MainActor private static func checkBrowsesCannotEvictATypedStatement() {
        let history = make()
        history.record(
            "SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        for i in 0..<(QueryHistory.untypedLimit + 20) {
            history.record(
                "SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1,
                scheme: "sqlite")
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
            history.record(
                "SELECT browse \(i)", from: .browse, outcome: .rows(1), milliseconds: 1,
                scheme: "sqlite")
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
            history.record(
                "SELECT \(i)", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        }
        expect(history.entries.count, QueryHistory.limit, "the list stops at its limit")
        expect(history.entries.first?.sql, "SELECT \(QueryHistory.limit + 9)", "newest first")
    }

    /// Whitespace is not a statement. It was true before and it is the kind of
    /// thing a rewrite of `record` drops.
    @MainActor private static func checkAnEmptyStatementIsNotAStatement() {
        let history = make()
        history.record("   \n ", from: .query, outcome: .rows(0), milliseconds: 0, scheme: "sqlite")
        expect(history.entries.count, 0, "nothing was recorded")
    }

    /// The comment above each statement is the whole reason this is a file and
    /// not a paste of the SQL: without it there is no way to tell the SELECT
    /// somebody typed from the one the sidebar sent.
    @MainActor private static func checkTheScriptSaysWhatCausedEachStatement() {
        let history = make()
        history.record(
            "SELECT 1", from: .browse, outcome: .rows(3), milliseconds: 12, scheme: "sqlite")
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
            "SELECT 1", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        history.record(
            "SELECT 2;", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
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
            milliseconds: 0, scheme: "sqlite")
        expect(QueryHistory.script(history.entries).contains("0 ms"), false, "no duration is shown")
    }

    /// The file is the panel, not the store. Both narrowings decide what gets
    /// written, and `canExportHistory` has to agree with them or the menu item
    /// stays live over a log with nothing in it.
    @MainActor private static func checkTheLogIsWhatThePanelIsShowing() {
        let model = makeModel()
        model.history.record(
            "SELECT typed", from: .query, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        model.history.record(
            "SELECT browsed", from: .browse, outcome: .rows(1), milliseconds: 1, scheme: "sqlite")
        expect(model.shownHistory.count, 1, "the browse is out while All is off")
        expect(model.canExportHistory, true, "and there is still something to write")

        model.showsAllStatements = true
        expect(model.shownHistory.count, 2, "All puts it back")

        model.historyFilter = "BROWSED"
        expect(model.shownHistory.count, 1, "the filter matches whatever the case")

        model.historyFilter = "nothing matches this"
        expect(model.canExportHistory, false, "and a filter that hides everything empties the log")
    }

    // MARK: - What must not be written down

    /// A password typed into the Query tab does not reach the file.
    ///
    /// The list is stored in a plist that nothing encrypts and can be exported
    /// as `.sql`, so an entry holding `hunter2` is a password on a disk —
    /// against this build's rule that one never gets there. Both ways out are
    /// checked, because they are separate code paths and the export reads the
    /// stored entries rather than re-deriving anything.
    @MainActor private static func checkAPasswordTypedIntoTheQueryTabIsNotWrittenDown() {
        let history = make()
        history.record(
            "ALTER USER app IDENTIFIED BY 'hunter2'", from: .query, outcome: .affected(0),
            milliseconds: 3, scheme: "mysql")

        expect(
            history.entries.first?.sql, "ALTER USER app IDENTIFIED BY '…'",
            "the statement is kept in the shape it was, and the secret is not")
        expect(
            QueryHistory.script(history.entries).contains("hunter2"), false,
            "and the exported script cannot carry what the store does not hold")
    }

    /// The dialect the statement was sent in is the one it is read with.
    ///
    /// `$$…$$` is a string literal in PostgreSQL and is not one anywhere else,
    /// so the same statement redacts differently depending on which was named.
    /// That is what makes `scheme` a required argument rather than a defaulted
    /// one: a call site that passed the wrong dialect would leave the body
    /// standing, and this is the pair that notices.
    @MainActor private static func checkTheStatementIsReadInTheDialectItWasSentIn() {
        let postgres = make()
        postgres.record(
            "ALTER ROLE app PASSWORD $$hunter2$$", from: .query, outcome: .affected(0),
            milliseconds: 1, scheme: "postgresql")
        expect(
            postgres.entries.first?.sql, "ALTER ROLE app PASSWORD '…'",
            "a dollar-quoted body is a literal here, and is the form a search written for "
                + "quotes would have walked past")

        let sqlite = make()
        sqlite.record(
            "ALTER ROLE app PASSWORD $$hunter2$$", from: .query, outcome: .affected(0),
            milliseconds: 1, scheme: "sqlite")
        expect(
            sqlite.entries.first?.sql != postgres.entries.first?.sql, true,
            "and a dialect with no such literal reads the same text differently, which is the "
                + "whole reason the caller has to say which one it was")
    }

    /// What an earlier build wrote is taken out at the next launch.
    ///
    /// Every build before this one stored the statement as typed, so the
    /// password that made this worth fixing is already on the disk. The check
    /// reads the defaults back rather than the entries: fixing it in memory and
    /// not writing it would pass an inspection of the list while leaving the
    /// plist exactly as it was.
    @MainActor private static func checkWhatAnEarlierBuildWroteIsSweptAtLaunch() {
        let store = ScratchDefaults.store("verify-query-history-sweep")
        let stale = QueryHistoryEntry(
            id: UUID(), sql: "ALTER USER app IDENTIFIED BY 'hunter2'", ranAt: Date(),
            origin: .query, milliseconds: 1, outcome: .affected(0))
        store.set(try? JSONEncoder().encode([stale]), forKey: QueryHistory.key)

        let history = QueryHistory(defaults: store)
        expect(
            history.entries.first?.sql, "ALTER USER app IDENTIFIED BY '…'",
            "the entry a previous launch left comes back with the secret gone")

        let written = store.data(forKey: QueryHistory.key)
            .map { String(decoding: $0, as: UTF8.self) }
        expect(
            written?.contains("hunter2"), false,
            "and the store itself no longer holds it, which is the only thing that matters")
    }

    /// And a shape this build cannot read is taken off the disk, not just out of
    /// the list.
    ///
    /// The case above is the sweep working on data it can read. This is the
    /// other half of the same promise, and the half that was quietly untrue:
    /// `load` shrugs at what it cannot decode, so the sweep mapped over an empty
    /// list, decided nothing had changed and wrote nothing — leaving the blob it
    /// could not read exactly where it was, password and all. The list looked
    /// empty and the plist was not.
    @MainActor private static func checkAShapeThisBuildCannotReadIsTakenOffTheDisk() {
        let store = ScratchDefaults.store("verify-query-history-unreadable")
        // A shape from a build that did not have all of today's fields. Any
        // undecodable JSON reaches the same branch; this is the one that looks
        // like something an earlier version of this file would have written.
        let stale = #"[{"sql":"ALTER USER app IDENTIFIED BY 'hunter2'","origin":"query"}]"#
        store.set(Data(stale.utf8), forKey: QueryHistory.key)

        let history = QueryHistory(defaults: store)
        expect(
            history.entries.isEmpty, true,
            "a shape this build cannot read opens an empty history rather than refusing to open")

        let written = store.data(forKey: QueryHistory.key)
            .map { String(decoding: $0, as: UTF8.self) }
        expect(
            written?.contains("hunter2"), false,
            "and it does not stay on the disk, where it would be the one thing the sweep is "
                + "unable to reach")
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
