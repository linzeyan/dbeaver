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
        defer { ScratchDefaults.release() }
        checkAScratchSuiteLeavesNoFileBehind()
        checkTheDefaultsAreTheOnesThatWereDecided()
        checkASettingSurvivesBeingWrittenAndReadBack()
        checkAnEditorFontSizeFromOutsideTheRangeIsFoldedBack()
        checkAConnectionKeptOnThisMacComesBack()
        checkAConnectionKeptInICloudGoesToTheDrive()
        checkAConnectionSurvivesAnICloudThatIsNotAvailable()
        checkTheFileGoesWhereXDGSaysItDoes()
        checkTheSettingsPanelHasRoomForTheICloudCaveat()
        checkAPasswordKeptOnThisMacIsNotInTheFileAsText()
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

    /// A scratch suite leaves nothing behind — not its values, and not its file.
    ///
    /// The checks ran without this for a while and left several hundred plists in
    /// `~/Library/Preferences`. `removePersistentDomain` empties a domain and does
    /// not delete it, so "cleaned up" quietly meant "one more empty file every
    /// run": invisible from inside the application, permanent outside it, and
    /// exactly the shape of the Keychain items an earlier version of these checks
    /// saved and never removed.
    ///
    /// First in the run, because `release` drops every suite handed out so far.
    /// Anywhere later and it would empty the stores the checks above it hold.
    private static func checkAScratchSuiteLeavesNoFileBehind() {
        let directory = NSHomeDirectory() + "/Library/Preferences"
        func leftovers() -> [String] {
            (try? FileManager.default.contentsOfDirectory(atPath: directory))?
                .filter { $0.hasPrefix("dev.dbclient.verify-leak.") } ?? []
        }
        // A run that failed this check left its own file behind, and without
        // this the next run would fail on that rather than on anything it did.
        // A check that stays red after the bug is fixed teaches people to
        // ignore it.
        for stale in leftovers() {
            try? FileManager.default.removeItem(atPath: directory + "/" + stale)
        }

        let store = ScratchDefaults.store("verify-leak")
        // Written to on purpose. An untouched suite has no file to leave behind,
        // so a probe that skipped this would pass against the bug it is here for.
        store.set(true, forKey: "dev.dbclient.leakProbe")
        store.synchronize()
        ScratchDefaults.release()
        expect(leftovers(), [], "the suite's plist is gone rather than merely emptied")
    }

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
            fresh.passwordStorage, .never, "no password is kept until somebody asks for it")
        expect(
            fresh.connectionStorage, .thisMac, "the remembered connection does not leave the Mac")
        expect(
            fresh.shutConnectionFolders, [], "every connection folder starts open")
        expect(fresh.editorFontSize, 13, "the editor draws at the size it always has")
    }

    /// A setting has to outlive the window, or the Settings window is a switch
    /// that resets every launch.
    private static func checkASettingSurvivesBeingWrittenAndReadBack() {
        let store = ScratchDefaults.store("verify-preferences")

        let first = Preferences(store: store)
        first.hidesEmptyColumns = true
        first.confirmsDeletions = false
        first.insertsRowOfDefaults = true
        first.usesTranslucentSidebar = true
        first.passwordStorage = .thisMac
        first.connectionStorage = .iCloud
        first.shutConnectionFolders = ["clients/acme"]
        first.editorFontSize = 16

        // A second reader over the same store, which is what the next launch is.
        let second = Preferences(store: store)
        expect(second.hidesEmptyColumns, true, "hiding empty columns was kept")
        expect(second.confirmsDeletions, false, "the confirmation being off was kept")
        expect(second.insertsRowOfDefaults, true, "sending a row of defaults was kept")
        expect(second.usesTranslucentSidebar, true, "the translucent sidebar was kept")
        expect(second.passwordStorage, .thisMac, "where passwords are kept was kept")
        expect(second.connectionStorage, .iCloud, "keeping connections in iCloud was kept")
        expect(
            second.shutConnectionFolders, ["clients/acme"],
            "a folder somebody shut is still shut on the next launch")
        expect(second.editorFontSize, 16, "the editor's type size was kept")
    }

    /// A size the Settings window could never have written reads back as the
    /// nearest one it offers.
    ///
    /// The value is a number in a plist somebody can edit: taken literally, a 0
    /// draws no text at all and a 96 leaves three lines on screen, and both are
    /// states with no control that leads back out of them. The key is spelled
    /// out here because it is a contract with the disk — a renamed key would
    /// silently orphan every kept size, and nothing else would notice.
    private static func checkAnEditorFontSizeFromOutsideTheRangeIsFoldedBack() {
        let store = ScratchDefaults.store("verify-preferences-font")
        store.set(96, forKey: "dev.dbclient.editorFontSize")
        expect(
            Preferences(store: store).editorFontSize, 18,
            "a hand-edited 96 reads as the largest size offered")
        store.set(0, forKey: "dev.dbclient.editorFontSize")
        expect(
            Preferences(store: store).editorFontSize, 10,
            "and a 0 as the smallest, not as no text at all")
    }

    // MARK: - Where the connection is kept

    /// A connection kept on this Mac comes back field for field, out of a file
    /// only its owner can read.
    ///
    /// Against a scratch pair of directories, and with an empty password, which is
    /// what keeps this check out of the developer's login Keychain: an empty
    /// password is nothing to remember and `ConnectionKeychain.save` writes
    /// nothing for it. The permissions are asserted as well as the fields, because
    /// the file names a host, a database and a user.
    private static func checkAConnectionKeptOnThisMacComesBack() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: root.appending(path: "drive"))

        let kept = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        ConnectionStore.save([kept], to: .thisMac, in: directories)
        expect(
            ConnectionStore.load(from: .thisMac, in: directories), [kept],
            "every field of the connection came back")

        let file = directories.local.appending(path: "dbclient/connections.json")
        let mode =
            (try? FileManager.default.attributesOfItem(atPath: file.path))?[.posixPermissions]
            as? Int
        expect(mode, 0o600, "and the file is readable only by its owner")
        // Nothing was left in iCloud Drive: "on this Mac" is also a statement
        // about where the connection is not.
        expect(
            FileManager.default.fileExists(
                atPath: directories.cloud!.appending(path: "dbclient/connections.json").path),
            false, "and no copy was left in iCloud Drive")
    }

    /// Choosing iCloud writes the file to iCloud Drive and leaves no local copy.
    private static func checkAConnectionKeptInICloudGoesToTheDrive() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: root.appending(path: "drive"))

        let kept = SavedConnection(
            settings: ConnectionSettings(
                scheme: "mysql", host: "db.example", port: "3306", database: "ops", user: "ana"))
        ConnectionStore.save([kept], to: .thisMac, in: directories)
        ConnectionStore.save([kept], to: .iCloud, in: directories)
        expect(
            FileManager.default.fileExists(
                atPath: directories.cloud!.appending(path: "dbclient/connections.json").path),
            true, "the file is in iCloud Drive")
        expect(
            FileManager.default.fileExists(
                atPath: directories.local.appending(path: "dbclient/connections.json").path),
            false, "and the copy it used to have on this Mac is gone")
        expect(
            ConnectionStore.load(from: .iCloud, in: directories), [kept],
            "and it reads back from there")
    }

    /// A Mac with no iCloud Drive still remembers the connection.
    ///
    /// The setting says iCloud and there is nowhere to sync to, so the write falls
    /// through to the local file rather than going nowhere — and the next launch,
    /// still set to iCloud, has to find it there. A setting that silently forgot
    /// the connection would look identical at the moment it was switched on and
    /// only fail at the next launch.
    private static func checkAConnectionSurvivesAnICloudThatIsNotAvailable() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: nil)

        let kept = SavedConnection(
            settings: ConnectionSettings(
                scheme: "mysql", host: "db.example", port: "3306", database: "ops", user: "ana"))
        ConnectionStore.save([kept], to: .iCloud, in: directories)
        expect(
            ConnectionStore.load(from: .thisMac, in: directories), [kept],
            "the connection is on this Mac")
        expect(
            ConnectionStore.load(from: .iCloud, in: directories), [kept],
            "and the launch that still asks for iCloud finds it")
        expect(
            ConnectionStore.syncCaveat(in: directories) != nil, true,
            "and the Settings panel has something to say about it")
    }

    /// The file goes where XDG says, and a relative `XDG_CONFIG_HOME` is ignored.
    ///
    /// The fallback is the specification's own — `~/.config` when the variable is
    /// unset — and ignoring a relative value is too. Without that rule an
    /// `XDG_CONFIG_HOME=.config` in a shell profile would put the connection
    /// wherever the application was launched from, which is a different file every
    /// time and none of them the one the last launch wrote.
    private static func checkTheFileGoesWhereXDGSaysItDoes() {
        let home = URL(filePath: "/Users/someone")
        expect(
            ConnectionDirectories.localDirectory(xdgConfigHome: nil, home: home).path,
            "/Users/someone/.config", "unset means ~/.config")
        expect(
            ConnectionDirectories.localDirectory(xdgConfigHome: "/tmp/xdg", home: home).path,
            "/tmp/xdg", "an absolute value is honoured")
        expect(
            ConnectionDirectories.localDirectory(xdgConfigHome: ".config", home: home).path,
            "/Users/someone/.config", "a relative one is ignored")
        expect(
            ConnectionDirectories.localDirectory(xdgConfigHome: "", home: home).path,
            "/Users/someone/.config", "and so is an empty one")
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
            rootView: SettingsView(preferences: preferences, syncCaveat: nil))
        let warned = NSHostingView(
            rootView: SettingsView(
                preferences: preferences,
                syncCaveat: ConnectionStore.syncCaveat() ?? "Two lines of explanation about "
                    + "which half of syncing this build cannot do and what happens instead."))
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
        Preferences(store: ScratchDefaults.store("verify-preferences"))
    }

    /// What "on this Mac" is worth, which is three separate claims.
    ///
    /// That a password comes back is the easy one. The other two are what make
    /// this a place to put a secret at all: the password is not in the file as
    /// anything `strings` or a backup indexer would find, and the file is
    /// readable only by its owner. A store that quietly failed either would look
    /// exactly like this one from the form's side.
    private static func checkAPasswordKeptOnThisMacIsNotInTheFileAsText() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let file = CredentialFile(url: root.appending(path: "credentials"))
        let first = UUID()
        let second = UUID()

        file.save("hunter2", for: first)
        expect(file.password(for: first), "hunter2", "a password kept here comes back")

        let bytes = (try? Data(contentsOf: file.url)) ?? Data()
        expect(bytes.isEmpty, false, "and there is a file to come back from")
        expect(
            bytes.range(of: Data("hunter2".utf8)) == nil, true,
            "with the password nowhere in it as text")
        let mode =
            (try? FileManager.default.attributesOfItem(atPath: file.url.path))?[.posixPermissions]
            as? Int
        expect(mode, 0o600, "and readable only by its owner")

        // One file holds all of them, so the second must not displace the first.
        file.save("s3cond", for: second)
        expect(file.password(for: first), "hunter2", "a second password leaves the first alone")
        expect(file.password(for: second), "s3cond", "and is itself readable")

        file.delete(for: first)
        expect(file.password(for: first), nil, "a withdrawn password is gone")
        expect(file.password(for: second), "s3cond", "and takes none of the others with it")

        // Nothing left behind, rather than an encrypted empty map. "There is no
        // file" is the state somebody checks by looking.
        file.delete(for: second)
        expect(
            FileManager.default.fileExists(atPath: file.url.path), false,
            "and the last one takes the file with it")
    }

    /// A directory nothing else can see, for the checks that write files. Removed
    /// by the caller, so a failing check leaves nothing behind either.
    private static func scratchDirectory() -> URL? {
        let root = URL(filePath: NSTemporaryDirectory())
            .appending(path: "dbclient-verify-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fail("a scratch directory could be made: \(error)")
            return nil
        }
        return root
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
