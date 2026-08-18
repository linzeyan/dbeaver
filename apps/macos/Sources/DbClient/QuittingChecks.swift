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
        failures = 0
        checkAWindowWithNothingInItIsNotAskedAbout()
        checkEveryKindOfStagedRowIsWorthAsking()
        checkAnOpenTransactionAloneIsWorthAsking()
        checkCellsTypedIntoOneRowAreOneRowLost()
        checkARowMarkedForDeletionIsNotCountedTwice()
        checkTheQuestionCarriesTheNumberBesideSave()
        checkTheDetailNamesWhatIsThereAndNothingElse()
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
            open?.question, "Quit with an open transaction?", "and is what the question is about")

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
            work(staged)?.question, "Quit without sending 5 changes?",
            "and puts the number in the question")

        var one = StagedChanges()
        one.deletes.insert(0)
        expect(
            work(one)?.question, "Quit without sending 1 change?",
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

    // MARK: - Harness

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
