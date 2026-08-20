import Foundation

/// What one relation's Content tab was showing, so that leaving a table and
/// coming back is not the same as opening it for the first time.
///
/// A value, deliberately: the model hands a copy to the store on the way out and
/// takes a copy back on the way in, so nothing the store is holding can be
/// altered by the pane that has stopped showing it.
struct BrowseState: Equatable {
    /// The WHERE field's text, verbatim.
    var whereClause = ""

    /// The ORDER BY field's text, verbatim.
    ///
    /// The sort marker is not kept beside it. `AppModel.gridSort` derives the
    /// marker from this string, and keeping only the string is what stops a
    /// restored table from drawing an arrow its ORDER BY does not say.
    var orderClause = ""

    /// The filter rows, in the order they were shown.
    var rules: [FilterRule] = []

    /// The WHERE those rows compiled to, as the core wrote it.
    ///
    /// Kept beside them rather than derived on the way back in, because either
    /// half alone restores a lie: the rows without the clause draw a filter over
    /// a grid that is not filtered, and the clause without the rows filters the
    /// grid with nothing on screen saying why. It was written against this
    /// relation's columns, which is why it travels with this relation and why
    /// `BrowseStore.clear()` matters — the same `schema.name` on another server
    /// is another table.
    var compiledClause = ""

    /// The cell that was selected when the table was left. Read it back through
    /// `selection(within:)` rather than straight out of here.
    var selection: GridSelection?

    /// Whether there is nothing here worth keeping.
    ///
    /// A table that was merely looked at has a state indistinguishable from a
    /// fresh one, and holding it would grow the store by an entry per table
    /// anybody clicked. The store drops these rather than store them.
    var isEmpty: Bool {
        whereClause.isEmpty && orderClause.isEmpty && rules.isEmpty && selection == nil
    }

    /// The selection to restore once `rowCount` rows have arrived, which is not
    /// always the one that was saved.
    ///
    /// Dropped rather than clamped where the row has not come back yet. The
    /// browse fetches a page at a time, so returning to a table whose 5,000th
    /// row was selected finds only the first page loaded — and putting the
    /// selection on the last row that did arrive would be pointing at a row
    /// nobody chose. No selection is the honest answer; a plausible wrong one is
    /// not.
    func selection(within rowCount: Int) -> GridSelection? {
        guard let selection, selection.rows.upperBound < rowCount else { return nil }
        return selection
    }
}

/// Every relation's browse state, keyed by `RelationInfo.id`.
///
/// Keyed by the id string rather than by `RelationInfo` because that struct
/// carries `estimatedRows`, which moves on its own: two values naming one table
/// stop being `==` the moment the sidebar reloads, and a dictionary keyed on
/// them would lose the state exactly when a refresh happened.
struct BrowseStore: Equatable {
    private var states: [String: BrowseState] = [:]

    /// How many relations have state worth restoring.
    ///
    /// Exists for the checks: it is the only way to tell "saved an empty state"
    /// from "saved nothing", and which of those happens is the whole of
    /// `BrowseState.isEmpty`'s job.
    var count: Int { states.count }

    /// What that relation was showing, or a fresh state where it has not been
    /// visited.
    ///
    /// Never optional. "Never visited" and "visited and left untouched" put the
    /// same thing on screen, so a caller made to tell them apart would be
    /// answering a question with no consequence.
    func state(for id: String) -> BrowseState { states[id] ?? BrowseState() }

    /// Remembers a relation's state, or forgets it where there is nothing to
    /// remember.
    mutating func save(_ state: BrowseState, for id: String) {
        states[id] = state.isEmpty ? nil : state
    }

    /// Forgets everything, for a window that has connected somewhere else.
    ///
    /// The keys are `schema.name` strings, and the same string names a different
    /// table on a different server. Restoring one server's filter onto another
    /// server's table is the one wrong answer this type is able to give, and
    /// this is what prevents it.
    mutating func clear() { states.removeAll() }
}
