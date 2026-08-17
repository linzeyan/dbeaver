import AppKit
import Foundation
import SwiftUI

/// Executable checks for the settings, run by `--verify-preferences`.
///
/// Each of these settings exists because a design question was answered with
/// "make it a setting", and each answer named a default. Two things can be wrong
/// with that and neither fails to compile: the default can be the other one, and
/// the behaviour can be wired to the wrong side of the switch — a hidden column
/// that is hidden when the box is clear, a confirmation that appears when it was
/// turned off. So every case here is run **both ways**, and asserts on the value
/// as well as on the difference. A check that only exercised the default would
/// pass against a build that ignored the setting entirely.
///
/// The rules themselves live where they can be reached without a database:
/// `EmptyColumns` takes a closure instead of an Arrow table, and the two edit
/// rules are `StagedChanges`', for the reason that file gives. What is left over
/// — reading the value back out of `UserDefaults` — runs against a scratch suite,
/// so running this does not change what a developer's own window does.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum PreferencesChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheDefaultsAreTheOnesThatWereDecided()
        checkASettingSurvivesBeingWrittenAndReadBack()
        checkAConnectionKeptOnThisMacComesBack()
        checkAConnectionSurvivesAnICloudThatIsNotAvailable()
        checkTheSettingsPanelHasRoomForTheICloudCaveat()
        checkAnEmptyColumnIsOnlyHiddenWhenTheSettingSaysSo()
        checkAColumnThatFillsUpLaterComesBackWhileTheEvidenceIsOpen()
        checkAColumnStaysDecidedOnceTheEvidenceIsIn()
        checkATableWithNoRowsHidesNothing()
        checkDeletionsAreOnlyAskedAboutWhenTheSettingSaysSo()
        checkAnEmptyNewRowIsOnlyRefusedWhenTheSettingSaysSo()
        if failures == 0 {
            fputs("preferences: all checks passed\n", stderr)
        } else {
            fputs("preferences: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The store

    /// What a fresh installation does. Written out as the sentences the decisions
    /// were recorded as, because that is the thing being checked — not that the
    /// store round-trips, but that it starts on the answer given.
    private static func checkTheDefaultsAreTheOnesThatWereDecided() {
        let fresh = scratch()
        expect(fresh.hidesEmptyColumns, false, "an all-null column is shown, not hidden")
        expect(fresh.confirmsDeletions, true, "Save asks before it sends deletions")
        expect(fresh.insertsRowOfDefaults, false, "an empty new row is refused here, by name")
        expect(fresh.usesTranslucentSidebar, false, "the sidebar is opaque, showing only itself")
        expect(
            fresh.connectionStorage, .thisMac, "the remembered connection does not leave the Mac")
    }

    /// A setting has to outlive the window, or the Settings window is a switch
    /// that resets every launch.
    private static func checkASettingSurvivesBeingWrittenAndReadBack() {
        let name = suiteName()
        guard let store = UserDefaults(suiteName: name) else {
            fail("a scratch defaults suite could be made")
            return
        }
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let first = Preferences(store: store)
        first.hidesEmptyColumns = true
        first.confirmsDeletions = false
        first.insertsRowOfDefaults = true
        first.usesTranslucentSidebar = true
        first.connectionStorage = .iCloud

        // A second reader over the same store, which is what the next launch is.
        let second = Preferences(store: store)
        expect(second.hidesEmptyColumns, true, "hiding empty columns was kept")
        expect(second.confirmsDeletions, false, "the confirmation being off was kept")
        expect(second.insertsRowOfDefaults, true, "sending a row of defaults was kept")
        expect(second.usesTranslucentSidebar, true, "the translucent sidebar was kept")
        expect(second.connectionStorage, .iCloud, "keeping connections in iCloud was kept")
    }

    // MARK: - Where the connection is kept

    /// A connection kept on this Mac comes back field for field.
    ///
    /// Through a scratch suite, and with an empty password, which is what keeps
    /// this check out of the developer's login Keychain: an empty password is
    /// nothing to remember and `ConnectionKeychain.save` returns without writing.
    /// The fields are the whole of what this half stores, so they are the whole
    /// of what is asserted.
    ///
    /// The iCloud half is not checked here, and not because it does not matter.
    /// Synchronised Keychain items need an entitlement that no ad-hoc signature
    /// carries — this binary included — so a check would be asserting what this
    /// build was signed with rather than what the code does. What is checked is
    /// the consequence of that refusal, below.
    private static func checkAConnectionKeptOnThisMacComesBack() {
        let name = suiteName()
        guard let store = UserDefaults(suiteName: name) else {
            fail("a scratch defaults suite could be made")
            return
        }
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let settings = ConnectionSettings(
            scheme: "postgres", host: "db.example", port: "5432", database: "sales", user: "ana")
        ConnectionStore.save(settings, password: "", to: .thisMac, store: store)
        expect(
            ConnectionStore.load(from: .thisMac, store: store), settings,
            "every field of the connection came back")
    }

    /// A build that cannot sync still remembers the connection.
    ///
    /// The one behaviour of the iCloud setting that can be checked from here, and
    /// the one worth checking: chosen on a build macOS refuses synchronised items
    /// to, the write falls through to this Mac rather than going nowhere. A
    /// setting that silently forgot the connection would look identical at the
    /// moment it was switched on and only fail at the next launch.
    private static func checkAConnectionSurvivesAnICloudThatIsNotAvailable() {
        guard ConnectionKeychain.iCloudRefusal() != nil else {
            // A signed build with the entitlement: the write goes to iCloud, and
            // asserting the fallback here would file a fixture in the user's own
            // iCloud Keychain to prove something that is not true of them.
            return
        }
        let name = suiteName()
        guard let store = UserDefaults(suiteName: name) else {
            fail("a scratch defaults suite could be made")
            return
        }
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let settings = ConnectionSettings(
            scheme: "mysql", host: "db.example", port: "3306", database: "ops", user: "ana")
        ConnectionStore.save(settings, password: "", to: .iCloud, store: store)
        expect(
            ConnectionStore.load(from: .thisMac, store: store), settings,
            "a refused iCloud write left the connection on this Mac")
    }

    /// The panel is tall enough for the sentence about iCloud being unavailable.
    ///
    /// `SettingsWindow` measures this view once and sizes the panel to what it
    /// measured, so anything that can appear in the view has to be in it while it
    /// is being measured. The first version of the caveat arrived from a `.task`
    /// and only under one of the two answers, which meant it was laid out below
    /// the bottom edge of a window that had already been sized — a warning
    /// nobody could read. Asserted as a height rather than by eye because the
    /// panel is 460pt wide and the capture tool only photographs the main window.
    private static func checkTheSettingsPanelHasRoomForTheICloudCaveat() {
        let preferences = scratch()
        let quiet = NSHostingView(
            rootView: SettingsView(preferences: preferences, iCloudRefusal: nil))
        let warned = NSHostingView(
            rootView: SettingsView(
                preferences: preferences,
                iCloudRefusal: ConnectionKeychain.iCloudRefusal() ?? "Two lines of explanation "
                    + "about why this build cannot reach the iCloud Keychain at all."))
        expect(
            warned.fittingSize.height > quiet.fittingSize.height, true,
            "the caveat is inside the height the panel is sized to")
    }

    // MARK: - Hiding a column that is null in every row

    /// The setting decides; the evidence is gathered either way.
    ///
    /// Both halves matter. Gathering regardless is what lets the checkbox act on
    /// the result already on screen, and the grid reading the setting rather than
    /// the evidence is what keeps a column on screen while the box is clear.
    private static func checkAnEmptyColumnIsOnlyHiddenWhenTheSettingSaysSo() {
        var columns = EmptyColumns()
        columns.weigh(rows: 0..<3, columnCount: 3, isNull: nulls(in: [2]))
        expect(columns.columns, [2], "the third column was null in all three rows")

        expect(hidden(columns, whenSettingIs: false), [], "and is drawn while the setting is off")
        expect(hidden(columns, whenSettingIs: true), [2], "and hidden while it is on")
    }

    /// A column with a value on a later page comes back.
    ///
    /// The only direction anything moves in. A grid that went on hiding a column
    /// it had been handed a value for would be hiding data, which is a worse
    /// failure than the empty column this exists to remove.
    private static func checkAColumnThatFillsUpLaterComesBackWhileTheEvidenceIsOpen() {
        var columns = EmptyColumns()
        columns.weigh(rows: 0..<2, columnCount: 3, isNull: nulls(in: [1, 2]))
        expect(columns.columns, [1, 2], "two columns were empty on the first page")

        // The second page holds a value in column 1 and nothing in column 2.
        columns.weigh(rows: 2..<4, columnCount: 3, isNull: nulls(in: [2]))
        expect(columns.columns, [2], "the column that filled up is drawn again")
        expect(columns.isSettled, false, "and a third page still gets a say")
    }

    /// Past the evidence pages the answer stops moving.
    ///
    /// This is the cost the setting is off by default for, so it is asserted
    /// rather than left as an implementation detail: a value arriving in the
    /// fourth page lands in a column nothing will draw, and only re-reading the
    /// relation brings it back.
    private static func checkAColumnStaysDecidedOnceTheEvidenceIsIn() {
        var columns = EmptyColumns()
        for page in 0..<EmptyColumns.evidencePages {
            columns.weigh(rows: (page * 2)..<(page * 2 + 2), columnCount: 2, isNull: nulls(in: [1]))
        }
        expect(columns.isSettled, true, "three pages settle it")
        expect(columns.columns, [1], "with the second column empty throughout")

        // A fourth page carrying a value everywhere, which changes nothing.
        columns.weigh(rows: 6..<8, columnCount: 2, isNull: nulls(in: []))
        expect(columns.columns, [1], "and a later page cannot reopen it")

        // Re-reading the relation is what can.
        columns.reset()
        expect(columns.columns, [], "a fresh read starts from nothing")
        expect(columns.isSettled, false, "and is open to evidence again")
    }

    /// An empty table hides nothing at all.
    ///
    /// Vacuously, every column of a result with no rows is null in every row it
    /// has. Acting on that would leave a grid with not even a header to say what
    /// the table holds — over the table where "add the first row" is exactly what
    /// the user came to do.
    private static func checkATableWithNoRowsHidesNothing() {
        var columns = EmptyColumns()
        columns.weigh(rows: 0..<0, columnCount: 4, isNull: nulls(in: [0, 1, 2, 3]))
        expect(columns.columns, [], "nothing is concluded from no rows")
        expect(columns.pagesWeighed, 0, "and an empty page is not evidence")
    }

    // MARK: - Confirming a delete

    private static func checkDeletionsAreOnlyAskedAboutWhenTheSettingSaysSo() {
        var staged = StagedChanges()
        staged.deletes.formUnion([0, 4])
        expect(
            staged.confirmation(askingBeforeDeleting: false) == nil, true,
            "with the setting off, Save sends the deletions without asking")
        expect(
            staged.confirmation(askingBeforeDeleting: true),
            DeleteConfirmation(rows: 2, others: 0),
            "and with it on, asks about both rows")

        // The case the setting exists for: a Save pressed for the cell edit,
        // carrying rows marked earlier out with it. The other changes are
        // counted separately, because that count is the surprise.
        staged.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "a")
        staged.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(
            staged.confirmation(askingBeforeDeleting: true),
            DeleteConfirmation(rows: 2, others: 2),
            "the edit and the new row are named as riding along")

        // Nothing marked is nothing to ask about, however the setting is set:
        // an UPDATE is on screen to be retyped and an INSERT can be deleted
        // again, so neither is what the question is about.
        var noDeletes = StagedChanges()
        noDeletes.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        expect(
            noDeletes.confirmation(askingBeforeDeleting: true) == nil, true,
            "a Save with no deletions is never interrupted")
    }

    // MARK: - An empty new row

    private static func checkAnEmptyNewRowIsOnlyRefusedWhenTheSettingSaysSo() {
        var empty = StagedChanges()
        empty.drafts = [
            DraftRow(values: [1: PendingValue(text: "filled")]),
            DraftRow()
        ]
        let refusal = empty.refusal(sendingRowOfDefaults: false)
        expect(refusal != nil, true, "with the setting off the row is refused here")
        // Named, and named the way the inspector strip names it: the point of
        // refusing on this side is that the user is told which row while they
        // are still looking at it.
        expect(refusal?.contains("New row 2"), true, "and the refusal says which row")

        expect(
            empty.refusal(sendingRowOfDefaults: true) == nil, true,
            "with it on the row goes, for the core to write as a row of defaults")

        // A row with an explicit NULL typed into it is not an empty row: NULL is
        // a value, and a column left alone is the absence of one. Refusing this
        // would be refusing a row the user did fill in.
        var nulled = StagedChanges()
        nulled.drafts = [DraftRow(values: [1: PendingValue(text: nil)])]
        expect(
            nulled.refusal(sendingRowOfDefaults: false) == nil, true,
            "a column set to NULL is a column that was typed into")
    }

    // MARK: - Harness

    /// A preferences store nothing else can see, emptied before and after.
    ///
    /// Registration domains are per-`UserDefaults` object rather than global, so
    /// a scratch suite reads back exactly what `Preferences` registered — which
    /// is the thing being asserted on.
    private static func scratch() -> Preferences {
        let name = suiteName()
        UserDefaults.standard.removePersistentDomain(forName: name)
        guard let store = UserDefaults(suiteName: name) else {
            fail("a scratch defaults suite could be made")
            return Preferences(store: .standard)
        }
        return Preferences(store: store)
    }

    private static func suiteName() -> String {
        "dev.dbclient.verify.\(UUID().uuidString)"
    }

    /// A page in which exactly `columns` are null, for every row of it.
    private static func nulls(in columns: Set<Int>) -> (Int, Int) -> Bool {
        { _, column in columns.contains(column) }
    }

    /// What the grid would be given for this evidence and this setting, which is
    /// the one line `AppModel.hiddenBrowseColumns` is.
    private static func hidden(_ columns: EmptyColumns, whenSettingIs on: Bool) -> Set<Int> {
        on ? columns.columns : []
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("preferences FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }

    private static func fail(_ what: String) {
        failures += 1
        fputs("preferences FAIL: \(what)\n", stderr)
    }
}
