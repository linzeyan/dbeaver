import Foundation

/// Executable checks for the question asked before the process ends, run by
/// `--verify-quitting`.
///
/// This is the one behaviour here whose failure mode is silence. A guard that
/// decides wrongly does not crash, does not draw anything, and does not fail to
/// compile: it either loses somebody's work without a word, or it asks about
/// nothing every time the application is quit until the reader learns to dismiss
/// the dialog unread — which costs them the work on the day it was real.
///
/// So every case is asserted in **both** directions, the way
/// `PreferencesChecks` asserts a setting: there has to be work for the question
/// to be put, and there has to be no question when there is no work. The wording
/// is pinned too, because what makes this dialog answerable is that it says which
/// rows go — "Are you sure?" is a question nobody can answer correctly.
///
/// The dialog itself is not checked here and cannot be: putting an `NSAlert` up
/// and clicking it needs accessibility permission this environment does not
/// grant. What is checked is everything the dialog is built from — whether it is
/// shown at all, and every word it would carry.
///
/// Main-actor isolated for the reason `PreferencesChecks` is: the counts go
/// through the window's own number formatter.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum QuittingChecks {
    private static var failures = 0

    static func run() -> Bool {
        // The one case below that builds a real `AppModel` reads the saved
        // connections, and asks the Keychain about the first — which blocks for
        // ever in a process with no GUI session. `BrowseRestoreChecks` says the
        // same thing at more length.
        guard let scratch = scratchDirectory() else { return false }
        defer { try? FileManager.default.removeItem(at: scratch) }
        setenv("XDG_CONFIG_HOME", scratch.path, 1)
        defer { ScratchDefaults.release() }

        failures = 0
        checkAWindowWithNothingInItIsNotAskedAbout()
        checkEveryKindOfStagedRowIsWorthAsking()
        checkAnOpenTransactionAloneIsWorthAsking()
        checkCellsTypedIntoOneRowAreOneRowLost()
        checkARowMarkedForDeletionIsNotCountedTwice()
        checkTheQuestionCarriesTheNumberBesideSave()
        checkTheDetailNamesWhatIsThereAndNothingElse()
        checkClosingOneOfSeveralWindowsIsNotCalledQuitting()
        checkEveryTabOfAWindowIsCounted()
        checkTheWindowsAreAddedUpAndNamed()
        checkAWindowAnswersForTheTabsBehindTheOneInFront()
        if failures == 0 {
            fputs("quitting: all checks passed\n", stderr)
        } else {
            fputs("quitting: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// An empty grid on an idle connection is quit without a word.
    ///
    /// The whole value of this guard rests on this case: a dialog in front of
    /// every quit is one nobody reads by the third day.
    private static func checkAWindowWithNothingInItIsNotAskedAbout() {
        expect(
            StagedChanges().lostOnQuitting(withOpenTransaction: false) == nil, true,
            "nothing staged and nothing open asks nothing")

        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        expect(
            staged.lostOnQuitting(withOpenTransaction: false) != nil, true,
            "and one changed cell is enough to be asked about")
    }

    /// Each of the three things a grid can be holding is work that would go.
    ///
    /// Checked separately because they are staged separately — a guard written
    /// against `updates` alone would let a window full of rows marked for
    /// deletion be quit in silence, which is the most expensive of the three.
    private static func checkEveryKindOfStagedRowIsWorthAsking() {
        var edited = StagedChanges()
        edited.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        expect(work(edited)?.editedRows, 1, "a changed cell is a row that would lose it")

        var deleted = StagedChanges()
        deleted.deletes.insert(3)
        expect(work(deleted)?.deletedRows, 1, "a marked row is a deletion that would be lost")

        var added = StagedChanges()
        added.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(work(added)?.newRows, 1, "a new row is what was typed into it")

        // The other direction for each: nothing staged, nothing counted.
        let none = work(StagedChanges(), transaction: true)
        expect(none?.editedRows, 0, "an untouched grid has no edited row")
        expect(none?.deletedRows, 0, "and none marked")
        expect(none?.newRows, 0, "and none added")
    }

    /// A transaction with work in it is asked about with nothing staged at all.
    ///
    /// It is the loss with the least on screen to warn about: the rows the
    /// statements changed are already gone from view, and all that is left is an
    /// amber marker in the toolbar. Quitting rolls it back.
    private static func checkAnOpenTransactionAloneIsWorthAsking() {
        let open = StagedChanges().lostOnQuitting(withOpenTransaction: true)
        expect(open?.transactionOpen, true, "an open transaction is work that would be lost")
        expect(open?.changes, 0, "with nothing staged beside it")
        expect(
            open?.question(.quitting), "Quit with an open transaction?",
            "and is what the question is about")

        expect(
            StagedChanges().lostOnQuitting(withOpenTransaction: false) == nil, true,
            "a connection holding nothing open is quit without a word")
    }

    /// Three cells typed into one row are one row, because that is what is on
    /// screen to lose.
    private static func checkCellsTypedIntoOneRowAreOneRowLost() {
        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        staged.updates[GridCell(row: 0, column: 2)] = PendingValue(text: "b")
        expect(work(staged)?.editedRows, 1, "two cells of one row are one edited row")
        expect(work(staged)?.changes, 2, "and still two of the changes Save counts")

        staged.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "c")
        expect(work(staged)?.editedRows, 2, "a cell in another row is another edited row")
    }

    /// A row that was edited and then marked for deletion is named once, as a
    /// deletion.
    ///
    /// Its UPDATE is dropped on the way out — `StagedChanges.request` leaves it
    /// out deliberately — so counting it here would tell somebody they were about
    /// to lose a change that was never going to be sent.
    private static func checkARowMarkedForDeletionIsNotCountedTwice() {
        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        staged.deletes.insert(0)
        expect(work(staged)?.editedRows, 0, "the edit in a doomed row is not a row of its own")
        expect(work(staged)?.deletedRows, 1, "the row is named once, as the deletion it is")

        staged.deletes.remove(0)
        expect(work(staged)?.editedRows, 1, "unmarking the row brings the edit back")
    }

    /// The number in the question is the number the strip beside Save shows.
    ///
    /// They are two counts of one thing, arrived at in two places, and a dialog
    /// saying "4 changes" over a strip saying "5 changes" is a dialog the reader
    /// has to stop and reconcile before answering.
    private static func checkTheQuestionCarriesTheNumberBesideSave() {
        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        staged.updates[GridCell(row: 0, column: 2)] = PendingValue(text: "b")
        staged.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "c")
        staged.deletes.insert(2)
        staged.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(work(staged)?.changes, staged.count, "the dialog counts what the strip counts")
        expect(
            work(staged)?.question(.quitting), "Quit without sending 5 changes?",
            "and puts the number in the question")

        var one = StagedChanges()
        one.deletes.insert(0)
        expect(
            work(one)?.question(.quitting), "Quit without sending 1 change?",
            "one change is not asked about in the plural")
    }

    /// The sentence names the rows that are there and stays silent about the rest.
    ///
    /// Pinned word for word rather than checked for a substring: a detail that
    /// lists a kind of row the grid is not holding is as misleading as one that
    /// omits a kind it is, and only the whole string catches both.
    private static func checkTheDetailNamesWhatIsThereAndNothingElse() {
        var all = StagedChanges()
        all.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        all.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "b")
        all.deletes.insert(2)
        all.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(
            work(all)?.detail,
            "2 edited rows, 1 deleted row and 1 new row are staged here and will not be sent. "
                + "This cannot be undone.",
            "every kind that is staged, in the order the grid holds them")
        expect(
            work(all, transaction: true)?.detail,
            "2 edited rows, 1 deleted row and 1 new row are staged here and will not be sent. "
                + "The transaction open on this connection will be rolled back. "
                + "This cannot be undone.",
            "with the transaction added when there is one to lose")

        var one = StagedChanges()
        one.deletes.insert(0)
        expect(
            work(one)?.detail,
            "1 deleted row is staged here and will not be sent. This cannot be undone.",
            "one row is spoken about in the singular, and the other two kinds go unmentioned")

        expect(
            StagedChanges().lostOnQuitting(withOpenTransaction: true)?.detail,
            "The transaction open on this connection will be rolled back. This cannot be undone.",
            "and a transaction on its own says only that")
    }

    /// ⌘W over one of several windows says Close, and ⌘Q says Quit.
    ///
    /// The two used to be one sentence because they were one event: a window
    /// closing with nothing behind it ends the process. With a second window open
    /// they come apart — closing the front one leaves the other where it is — and
    /// a dialog headed "Quit" over a key that closes a window is a dialog telling
    /// somebody the wrong thing about what they are about to lose.
    private static func checkClosingOneOfSeveralWindowsIsNotCalledQuitting() {
        var staged = StagedChanges()
        staged.deletes.insert(0)
        expect(
            work(staged)?.question(.closing), "Close without sending 1 change?",
            "closing a window names closing")
        expect(
            work(staged)?.question(.quitting), "Quit without sending 1 change?",
            "and quitting names quitting")
        expect(
            StagedChanges().lostOnQuitting(withOpenTransaction: true)?.question(.closing),
            "Close with an open transaction?",
            "both wordings, for the transaction sentence too")

        expect(
            UnsavedWork.Departure.closing.confirmation, "Discard and Close",
            "and the button says which of the two it does")
        expect(
            UnsavedWork.Departure.quitting.confirmation, "Discard and Quit",
            "in both directions")
    }

    /// Every tab of a window is counted, not the one in front.
    ///
    /// A window is a list of connections, each with its own staged changes and
    /// its own transaction. A guard that read only the tab on screen would let ⌘W
    /// throw away the work in the tab beside it without a word — the same loss
    /// this whole file exists to prevent, in the place hardest to notice.
    private static func checkEveryTabOfAWindowIsCounted() {
        var front = StagedChanges()
        front.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        var behind = StagedChanges()
        behind.deletes.insert(3)
        behind.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]

        let both = UnsavedWork.inOneWindow(
            [front, behind].compactMap { $0.lostOnQuitting(withOpenTransaction: false) })
        expect(both?.editedRows, 1, "the tab in front contributes its edited row")
        expect(both?.deletedRows, 1, "the tab behind it contributes its deletion")
        expect(both?.newRows, 1, "and its new row")
        expect(both?.changes, 3, "and the count is every tab's work")
        expect(both?.windows, 1, "all of it in one window")

        expect(
            UnsavedWork.inOneWindow([]), nil,
            "a window whose tabs are all clean is closed without a word")
    }

    /// ⌘Q asks once, about every window, and says how many there are.
    ///
    /// One dialog rather than one per window: a question somebody has to answer
    /// twice is a question they stop reading. And the count has to be in it —
    /// "5 changes" with nothing saying where they are is a number nobody can act
    /// on, because the windows holding them are behind the dialog.
    private static func checkTheWindowsAreAddedUpAndNamed() {
        // Both windows hold a row of every kind the other holds, so every total
        // below differs from either window's own number. A fixture where one
        // window's count happened to equal the sum would pass against a guard
        // that read only the first.
        var here = StagedChanges()
        here.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        here.deletes.insert(3)
        here.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        var there = StagedChanges()
        there.updates[GridCell(row: 7, column: 2)] = PendingValue(text: "b")
        there.deletes.insert(9)
        there.drafts = [DraftRow(values: [1: PendingValue(text: "another")])]

        let one = here.lostOnQuitting(withOpenTransaction: true)
        let two = there.lostOnQuitting(withOpenTransaction: true)
        let both = UnsavedWork.acrossWindows([one, two].compactMap { $0 })
        expect(both?.editedRows, 2, "the edited rows of both windows")
        expect(both?.deletedRows, 2, "and the deleted rows of both")
        expect(both?.newRows, 2, "and the new rows of both")
        expect(both?.changes, 6, "and the changes of both")
        expect(both?.windows, 2, "counted as two windows")
        expect(both?.openTransactions, 2, "and both open transactions")
        expect(
            both?.detail,
            "2 edited rows, 2 deleted rows and 2 new rows are staged in 2 windows and will "
                + "not be sent. The 2 open transactions will be rolled back. "
                + "This cannot be undone.",
            "and the sentence says where the work is and how much is open")

        // One window is spoken about as it always was: "here", and "the
        // transaction", because there is one of each and naming a count would be
        // arithmetic nobody needs.
        expect(
            UnsavedWork.acrossWindows([one].compactMap { $0 })?.detail,
            "1 edited row, 1 deleted row and 1 new row are staged here and will not be sent. "
                + "The transaction open on this connection will be rolled back. "
                + "This cannot be undone.",
            "one window still reads as one window")
        expect(
            UnsavedWork.acrossWindows([]), nil,
            "and an application whose windows are all clean is quit without a word")
    }

    /// The rule above, through a real window rather than through the fold.
    ///
    /// `UnsavedWork.inOneWindow` can be handed anything; what this pins is that
    /// `AppModel` hands it every session. The defect it guards against shipped for
    /// as long as a window had one tab that mattered: the guard read the forwarding
    /// properties, which resolve to the tab in front, so ⌘Q discarded the staged
    /// edits in every other tab of the window in silence.
    ///
    /// Restore is how a model gets a second tab with no database to connect to.
    private static func checkAWindowAnswersForTheTabsBehindTheOneInFront() {
        let model = twoTabbedModel()
        expect(model.sessions.count, 2, "the fixture really has two tabs")
        expect(model.unsavedWork == nil, true, "and neither of them is holding anything")

        model.sessions[1].staged.deletes.insert(4)
        expect(
            model.unsavedWork?.deletedRows, 1,
            "a row marked in the tab behind is work this window would lose")
        expect(model.activeSession, 0, "with that tab not the one in front")

        model.sessions[0].staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        expect(model.unsavedWork?.editedRows, 1, "and the tab in front contributes too")
        expect(model.unsavedWork?.changes, 2, "both counted once")
        expect(model.unsavedWork?.windows, 1, "as the one window they are in")
    }

    // MARK: - Harness

    /// A model with two tabs and no connection on either.
    private static func twoTabbedModel() -> AppModel {
        let tab = RestoredTab(
            connection: nil, settings: nil, label: "New Connection",
            buffers: [], activeBuffer: 0)
        return AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-quitting")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-quitting")),
            preferences: Preferences(store: ScratchDefaults.store("verify-quitting")),
            restoring: RestoredWindow(tabs: [tab, tab], activeTab: 0))
    }

    /// A directory of its own for the config these checks must not read.
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-quitting-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs("quitting FAIL: a scratch directory could not be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    /// What quitting would lose, with the transaction closed unless a case says
    /// otherwise — which is the state nearly every window is in.
    private static func work(_ staged: StagedChanges, transaction open: Bool = false)
        -> UnsavedWork?
    {
        staged.lostOnQuitting(withOpenTransaction: open)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("quitting FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
