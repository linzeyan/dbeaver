import Foundation

/// One cell of a browse result, by its place in it.
///
/// Position and not identity, because that is what the grid has: a row is the
/// nth row of what was fetched. It survives editing only because a pending edit
/// is discarded whenever the result is re-read, which is also the moment the
/// database's own answer replaces what was typed.
struct GridCell: Hashable {
    let row: Int
    let column: Int
}

/// What a cell was changed to. `nil` is SQL's NULL, which is not an empty
/// string — a text column can hold both and a grid has to be able to say which.
struct PendingValue: Equatable {
    let text: String?
}

/// A row that is not in the result, holding what has been typed into it.
///
/// Values by column index and not by name, so that it is addressed exactly as
/// the rows above it are: the grid draws a draft in the same coordinates it
/// draws everything else, and the editor writes into it through the same
/// selection. A column with no entry is left out of the INSERT rather than sent
/// as NULL, which is what makes the table's own defaults apply — the difference
/// between adding a row and dictating every one of its columns.
struct DraftRow {
    var values: [Int: PendingValue] = [:]
}

/// What Save has to put to the user before it sends.
///
/// Only deletions get asked about, and only because a setting says so. Every
/// other signal a staged deletion has is already on screen — the row stays there
/// struck through in red, the count beside Save includes it, and Revert takes it
/// all back — which is the argument that this is a second confirmation. The
/// argument for it is the one case those signals do not cover: a press of Save
/// that was about a cell somebody just edited, carrying rows they marked ten
/// minutes ago out with it. Deletions are also the only staged change with
/// nothing to undo them afterwards — an overwritten value is on screen to be
/// retyped, an inserted row can be deleted again — and that is what tips the
/// default to asking.
struct DeleteConfirmation: Equatable {
    /// How many rows go. The number is the whole of the question being put.
    let rows: Int
    /// The other statements riding along. Nought is the ordinary case; anything
    /// else is the situation this exists for.
    let others: Int

    /// Main-actor isolated for the reason `QueryHistoryOutcome.label` is: the
    /// counts go through the window's own number formatter.
    @MainActor
    var question: String { "Send \(AppModel.pluralized(rows, "deleted row"))?" }

    @MainActor
    var detail: String {
        let deleted = "\(AppModel.pluralized(rows, "row")) will be deleted."
        guard others > 0 else { return "\(deleted) This cannot be undone." }
        return "\(deleted) \(AppModel.pluralized(others, "other change")) will be sent with it. "
            + "This cannot be undone."
    }
}

/// What a grid is holding but has not sent.
///
/// Three kinds, staged differently because they are different questions. A
/// changed cell is a coordinate into rows that are on screen; a deleted row is
/// one of those rows entire, and has no cell to point at; a new row is not in
/// the result at all and so has to carry its own values.
struct StagedChanges {
    /// Cells changed in rows the result already holds.
    var updates: [GridCell: PendingValue] = [:]
    /// Rows marked to go, by their place in the result.
    var deletes: Set<Int> = []
    /// Rows added and not yet sent, in the order they were added. They sit after
    /// the last fetched row, which is where a row nobody has an ordering for
    /// belongs — and where a further page landing above them leaves them alone.
    var drafts: [DraftRow] = []

    var isEmpty: Bool { updates.isEmpty && deletes.isEmpty && drafts.isEmpty }

    /// How many changes there are to send.
    ///
    /// A row marked for deletion counts once however many of its cells were
    /// typed into, and a new row counts once however many were filled in,
    /// because the statement that goes is the delete or the insert: the count on
    /// screen has to be the number of statements Save will send, or Save's own
    /// report of what it sent will contradict it.
    var count: Int {
        updates.keys.filter { !deletes.contains($0.row) }.count + deletes.count + drafts.count
    }

    /// Why Save will not send this, or nil where it will.
    ///
    /// A new row nobody typed into is the one thing staged here that can be
    /// refused before the core sees it. Every untouched column is left out of the
    /// INSERT so the table's own defaults apply to it, so a row where nothing was
    /// touched is asking for a row that is nothing but defaults — which the
    /// databases here spell three different ways, and one of them cannot spell at
    /// all. Whether it may be asked for is the setting; while that is off, the
    /// row is named here, on screen, next to the row itself, rather than sent for
    /// a server to answer about in a sentence about SQL.
    ///
    /// Numbered from one and among the drafts, because "new row 2" is what the
    /// inspector strip calls it while the cursor is in it. Its number in the grid
    /// would be a hundred thousand and change.
    func refusal(sendingRowOfDefaults: Bool) -> String? {
        guard !sendingRowOfDefaults,
            let empty = drafts.firstIndex(where: { $0.values.isEmpty })
        else { return nil }
        return "New row \(empty + 1) has nothing in it. Fill a column in, discard the row, or "
            + "turn on “Insert a row of defaults for an empty new row” in Settings."
    }

    /// What Save has to ask before it sends this, or nil where it may just send.
    func confirmation(askingBeforeDeleting: Bool) -> DeleteConfirmation? {
        guard askingBeforeDeleting, !deletes.isEmpty else { return nil }
        return DeleteConfirmation(rows: deletes.count, others: count - deletes.count)
    }
}

/// What building a request needs to know about the rows on screen: their column
/// names, and what the database said a cell held when the row was read.
///
/// A protocol rather than `ArrowTable` itself, so that the rules below can be
/// checked without a connection — `--verify-editing` runs them against a table
/// of literals, which is the only way to assert on statements nobody sends.
protocol StagedRows {
    var columnNames: [String] { get }
    /// The value at a cell, or nil for SQL's NULL — which is not an empty
    /// string: a text column can hold both, and they name different rows.
    func value(row: Int, column: Int) -> String?
}

extension StagedChanges {
    /// The pending changes as one request, or nil where a row cannot be named.
    ///
    /// The key values come out of the grid rather than out of the edit: they are
    /// what the database said when the row was read, which is what identifies it
    /// — including when the key column itself is what was changed. A key column
    /// the result does not carry cannot happen through a browse, which is
    /// `SELECT *`, and is refused here rather than sent as a shorter key that
    /// would name more rows than one.
    ///
    /// Cells staged in a row that is also marked for deletion are left out. The
    /// core tolerates the pair — it orders deletes last for exactly this reason
    /// — but an UPDATE of a row the user has already crossed out is a statement
    /// nobody asked for, and it would show up in their transaction.
    func request(
        schema: String, relation: String, keyColumns: [String], rows: StagedRows
    ) -> EditRequest? {
        guard !keyColumns.isEmpty else { return nil }
        var request = EditRequest(schema: schema, relation: relation)
        // Grouped by row, so one row with three changed cells is one UPDATE.
        let changed = updates.keys.filter { !deletes.contains($0.row) }
        for (row, cells) in Dictionary(grouping: changed, by: \.row).sorted(by: { $0.key < $1.key })
        {
            guard let key = key(of: row, keyColumns: keyColumns, rows: rows) else { return nil }
            let set = cells.sorted(by: { $0.column < $1.column }).map { cell in
                EditRequest.Cell(column: rows.columnNames[cell.column], value: updates[cell]?.text)
            }
            request.updates.append(EditRequest.Update(key: key, set: set))
        }
        // Columns nobody typed into are absent rather than NULL, so the table's
        // defaults apply to them — which is the whole difference between adding
        // a row and dictating every column of one. A draft that is empty is left
        // in and becomes an insert with no cells, which the core reads as a row
        // of every default; whether it may be sent at all is `refusal`'s
        // question, asked before this one and answered by a setting.
        for draft in drafts {
            let set = draft.values.keys.sorted().map { column in
                EditRequest.Cell(
                    column: rows.columnNames[column], value: draft.values[column]?.text)
            }
            request.inserts.append(EditRequest.Insert(set: set))
        }
        for row in deletes.sorted() {
            guard let key = key(of: row, keyColumns: keyColumns, rows: rows) else { return nil }
            request.deletes.append(EditRequest.Delete(key: key))
        }
        return request
    }

    /// A draft pre-filled from a row on screen, with the key columns left out
    /// so the table's default supplies a fresh key.
    ///
    /// A key column is absent rather than NULL, for the reason an untouched
    /// column of a new row is: sending NULL would override the default. A
    /// staged edit wins over the value the database sent, because the copy is
    /// of the row on screen — and a NULL staged edit is a value, so it is
    /// copied as NULL rather than read back from the row.
    func draft(copying row: Int, from rows: StagedRows, clearing keyColumns: [String]) -> DraftRow {
        var values: [Int: PendingValue] = [:]
        for i in rows.columnNames.indices {
            guard !keyColumns.contains(rows.columnNames[i]) else { continue }
            if let staged = updates[GridCell(row: row, column: i)] {
                values[i] = staged
            } else {
                values[i] = PendingValue(text: rows.value(row: row, column: i))
            }
        }
        return DraftRow(values: values)
    }

    private func key(of row: Int, keyColumns: [String], rows: StagedRows) -> [EditRequest.Cell]? {
        var key: [EditRequest.Cell] = []
        for name in keyColumns {
            guard let at = rows.columnNames.firstIndex(of: name) else { return nil }
            key.append(EditRequest.Cell(column: name, value: rows.value(row: row, column: at)))
        }
        return key
    }
}

/// The changes a grid is holding, as the core's edit surface wants them.
///
/// Mirrors of `dbedit`'s types, encoding to the shape `db_edit_sql_json`
/// documents. Written out here rather than reused from the decode side because
/// nothing decodes them: this is the one direction, and a shared type would
/// invite a field to be added on the side that does not send it.
struct EditRequest: Encodable {
    let schema: String
    let relation: String
    var updates: [Update] = []
    var inserts: [Insert] = []
    var deletes: [Delete] = []

    struct Update: Encodable {
        let key: [Cell]
        let set: [Cell]
    }

    struct Insert: Encodable {
        let set: [Cell]
    }

    struct Delete: Encodable {
        let key: [Cell]
    }

    struct Cell: Encodable {
        let column: String
        let value: String?
    }

    var json: String {
        let data = (try? JSONEncoder().encode(self)) ?? Data()
        return String(data: data, encoding: .utf8) ?? "{}"
    }
}

/// The browse grid answering what a request needs to ask. Two lines, because an
/// Arrow result already holds exactly that and nothing about editing belongs in
/// the table itself.
extension ArrowTable: StagedRows {
    var columnNames: [String] { columns.map(\.name) }

    func value(row: Int, column: Int) -> String? {
        isNull(row: row, column: column) ? nil : text(row: row, column: column)
    }
}
