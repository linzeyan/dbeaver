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
    /// Whether the connection is holding a transaction the database has not been
    /// told to keep. Quitting rolls it back, which is the loss the toolbar's
    /// amber marker has been warning about all along.
    let transactionOpen: Bool

    /// Nothing here would be lost, so nothing may be asked. An application that
    /// puts a dialog in front of a quit that costs nothing teaches the reflex
    /// that dismisses the one that costs something.
    var isEmpty: Bool { changes == 0 && !transactionOpen }

    /// Main-actor isolated for the reason `DeleteConfirmation.question` is: the
    /// counts go through the window's own number formatter.
    @MainActor
    var question: String {
        // With nothing staged the only thing at stake is the transaction, and a
        // count of changes would be a nought nobody needs read to them.
        guard changes > 0 else { return "Quit with an open transaction?" }
        return "Quit without sending \(AppModel.pluralized(changes, "change"))?"
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
            sentences.append("\(Self.listed(rows)) \(verb) staged here and will not be sent.")
        }
        if transactionOpen {
            sentences.append("The transaction open on this connection will be rolled back.")
        }
        sentences.append("This cannot be undone.")
        return sentences.joined(separator: " ")
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
            changes: count, transactionOpen: open)
        return work.isEmpty ? nil : work
    }
}
