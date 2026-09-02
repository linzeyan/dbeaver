import Foundation

/// What ending the process would throw away, and the question that has to be put
/// before it does.
///
/// One value for both ways out. ⌘W closes the only window this application has
/// and a closed window ends the process, so closing and quitting lose exactly the
/// same work — two questions worded differently would be two chances to word one
/// of them wrongly.
///
/// Rows rather than cells, because rows are what the person looking at the grid
/// can see: three cells typed into one row are one row that will lose what was
/// typed into it. `changes` is the exception, and deliberately so — it is the
/// number the strip beside Save is showing, and a dialog that disagreed with the
/// window behind it would be the less believable of the two.
struct UnsavedWork: Equatable {
    /// Rows holding a changed cell, counted once each. Rows also marked for
    /// deletion are not among them: their edits are never sent.
    let editedRows: Int
    /// Rows marked to be deleted when Save is pressed.
    let deletedRows: Int
    /// Rows added to the grid and not yet sent.
    let newRows: Int
    /// What the strip beside Save is counting, which is what this dialog's own
    /// number has to be.
    let changes: Int
    /// How many connections are holding a transaction the database has not been
    /// told to keep. Quitting rolls them back, which is the loss the toolbar's
    /// amber marker has been warning about all along.
    ///
    /// A count rather than a flag, because this value now answers for more than
    /// one connection: a window has a tab per connection and the application has
    /// a window per screenful, and a dialog saying "the transaction" over three
    /// of them would be naming one and losing three.
    let openTransactions: Int
    /// How many windows the work above is spread over. One for a window's own
    /// answer; more only in the question ⌘Q puts.
    let windows: Int

    var transactionOpen: Bool { openTransactions > 0 }

    /// Nothing here would be lost, so nothing may be asked. An application that
    /// puts a dialog in front of a quit that costs nothing teaches the reflex
    /// that dismisses the one that costs something.
    var isEmpty: Bool { changes == 0 && !transactionOpen }

    /// Which way out is being taken.
    ///
    /// It changes only what the dialog is called, and it has to: with more than
    /// one window open, closing one and quitting stop being the same event — the
    /// first loses that window's work and leaves the others where they were — and
    /// a dialog saying "Quit" over ⌘W would be naming a key that does something
    /// else. With one window they are still the same event, and both spellings
    /// come out of the same sentence rather than out of two.
    enum Departure: String {
        case quitting = "Quit"
        case closing = "Close"

        /// What the confirming button says: the cost and the destination, so the
        /// button can be answered without reading the sentence above it twice.
        var confirmation: String { "Discard and \(rawValue)" }
    }

    /// Main-actor isolated for the reason `DeleteConfirmation.question` is: the
    /// counts go through the window's own number formatter.
    @MainActor
    func question(_ departure: Departure) -> String {
        // With nothing staged the only thing at stake is the transaction, and a
        // count of changes would be a nought nobody needs read to them.
        guard changes > 0 else { return "\(departure.rawValue) with an open transaction?" }
        return "\(departure.rawValue) without sending \(AppModel.pluralized(changes, "change"))?"
    }

    @MainActor
    var detail: String {
        var sentences: [String] = []
        let rows = [
            editedRows > 0 ? AppModel.pluralized(editedRows, "edited row") : nil,
            deletedRows > 0 ? AppModel.pluralized(deletedRows, "deleted row") : nil,
            newRows > 0 ? AppModel.pluralized(newRows, "new row") : nil
        ].compactMap { $0 }
        if !rows.isEmpty {
            let verb = editedRows + deletedRows + newRows == 1 ? "is" : "are"
            // Where, once there is more than one place it could be. A count of
            // changes with nothing saying which window holds them is a number
            // somebody cannot act on — and the windows are behind the dialog.
            let place = windows > 1 ? "in \(windows) windows" : "here"
            sentences.append("\(Self.listed(rows)) \(verb) staged \(place) and will not be sent.")
        }
        if openTransactions == 1 {
            sentences.append("The transaction open on this connection will be rolled back.")
        } else if openTransactions > 1 {
            sentences.append("The \(openTransactions) open transactions will be rolled back.")
        }
        sentences.append("This cannot be undone.")
        return sentences.joined(separator: " ")
    }

    /// The tabs of one window, as that window's single answer.
    ///
    /// Every tab is counted, not only the one in front. A window is a list of
    /// connections and each has its own staged changes and its own transaction,
    /// so a guard that read the tab on screen would let ⌘Q throw away the work in
    /// the tab beside it without asking — which is exactly the loss this dialog
    /// exists to prevent, in the place it is hardest to notice.
    static func inOneWindow(_ work: [UnsavedWork]) -> UnsavedWork? {
        summed(work, windows: 1)
    }

    /// One entry per window that has something to lose, as the question ⌘Q puts.
    ///
    /// One dialog for every window, because ⌘Q ends every window: a dialog per
    /// window, each naming its own share, is a dialog somebody dismisses twice
    /// without reading either of them.
    static func acrossWindows(_ work: [UnsavedWork]) -> UnsavedWork? {
        summed(work, windows: work.count)
    }

    private static func summed(_ work: [UnsavedWork], windows: Int) -> UnsavedWork? {
        guard !work.isEmpty else { return nil }
        let combined = UnsavedWork(
            editedRows: work.reduce(0) { $0 + $1.editedRows },
            deletedRows: work.reduce(0) { $0 + $1.deletedRows },
            newRows: work.reduce(0) { $0 + $1.newRows },
            changes: work.reduce(0) { $0 + $1.changes },
            openTransactions: work.reduce(0) { $0 + $1.openTransactions },
            windows: windows)
        // A fold of things that each had something to lose can still have
        // nothing: `lostOnQuitting` never hands back an empty one, but this is
        // also the place a caller could pass an empty list of tabs.
        return combined.isEmpty ? nil : combined
    }

    /// The parts as English rather than as a comma-separated list, because this
    /// sentence is read once, under time pressure, by somebody deciding whether
    /// to lose it.
    private static func listed(_ parts: [String]) -> String {
        guard let last = parts.last, parts.count > 1 else { return parts.first ?? "" }
        return parts.dropLast().joined(separator: ", ") + " and " + last
    }
}

extension StagedChanges {
    /// What quitting would throw away, or nil where it would throw away nothing.
    ///
    /// A rule on the staged changes rather than a branch inside the quit guard,
    /// for the reason `confirmation(askingBeforeDeleting:)` is one: deciding
    /// whether there is anything to lose is the half that can be wrong, and it is
    /// checkable with no window, no connection and nobody at the keyboard.
    ///
    /// The transaction is passed in rather than reached for. It is the
    /// connection's state and not the grid's, and this file is the one place both
    /// have to be weighed together — a quit loses whichever of them is there.
    func lostOnQuitting(withOpenTransaction open: Bool) -> UnsavedWork? {
        // Counted once per row, and never for a row that is going anyway: an
        // UPDATE staged in a row marked for deletion is dropped on the way out,
        // so naming it here would name a change that was never going to be sent.
        let edited = Set(updates.keys.filter { !deletes.contains($0.row) }.map(\.row))
        let work = UnsavedWork(
            editedRows: edited.count, deletedRows: deletes.count, newRows: drafts.count,
            changes: count, openTransactions: open ? 1 : 0, windows: 1)
        return work.isEmpty ? nil : work
    }
}
