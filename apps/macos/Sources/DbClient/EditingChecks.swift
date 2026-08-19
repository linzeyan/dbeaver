import Foundation

/// Executable checks for what the grid stages, run by `--verify-editing`.
///
/// What a change becomes once it is sent is the core's business: `crates/edit`
/// decides which literal a value gets, which columns may name a row, and in
/// which order the statements go. Restating any of that here would be a second
/// copy of a rule, which is a rule that will disagree with the first one the day
/// either is corrected.
///
/// What is checked here is this side's own, and it is the half nothing else can
/// see. A request that is silently empty, or that names a row by a key read from
/// the edit instead of from the row, produces statements the core will happily
/// build and a database will happily run — against rows nobody was looking at.
/// The core cannot catch that, because by then the request is all there is.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum EditingChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAChangedCellNamesItsRowByWhatTheDatabaseSaid()
        checkOneRowIsOneStatement()
        checkAMarkedRowGoesWholeAndTakesItsEditsWithIt()
        checkANewRowSendsOnlyTheColumnsItWasGiven()
        checkADuplicatedRowCopiesEverythingButTheKey()
        checkTheCountOnScreenIsTheNumberOfStatements()
        checkARowThatCannotBeNamedIsRefused()
        if failures == 0 {
            fputs("editing: all checks passed\n", stderr)
        } else {
            fputs("editing: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The `WHERE` clause carries the value the row was read with, even when
    /// that column is the one being changed.
    ///
    /// The tempting wrong version reads the key out of the staged cells, and it
    /// works for every edit except this one — where it writes `WHERE id = 7` for
    /// a row whose id is 1, and changes whichever row happens to be 7.
    private static func checkAChangedCellNamesItsRowByWhatTheDatabaseSaid() {
        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 0)] = PendingValue(text: "7")
        expect(
            summary(of: staged), "update id=1 → id=7",
            "the key is the old value, the assignment the new one")

        var nulled = StagedChanges()
        nulled.updates[GridCell(row: 1, column: 1)] = PendingValue(text: nil)
        expect(
            summary(of: nulled), "update id=2 → label=NULL",
            "NULL survives as an absent value rather than as the word")
    }

    /// Cells are grouped by the row they sit in, because a row is what a
    /// statement can name.
    private static func checkOneRowIsOneStatement() {
        var together = StagedChanges()
        together.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        together.updates[GridCell(row: 0, column: 2)] = PendingValue(text: "2")
        expect(
            summary(of: together), "update id=1 → label=a,qty=2",
            "two cells of one row are one UPDATE, in column order")

        var apart = StagedChanges()
        apart.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "b")
        apart.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        expect(
            summary(of: apart), "update id=1 → label=a | update id=2 → label=b",
            "two rows are two UPDATEs, in row order whatever order they were typed in")
    }

    /// A marked row leaves as a DELETE, and whatever was typed into it does not
    /// go separately.
    private static func checkAMarkedRowGoesWholeAndTakesItsEditsWithIt() {
        var staged = StagedChanges()
        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        staged.deletes.insert(0)
        expect(summary(of: staged), "delete id=1", "the UPDATE of a deleted row is dropped")

        var mixed = StagedChanges()
        mixed.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        mixed.deletes.insert(2)
        expect(
            summary(of: mixed), "update id=1 → label=a | delete id=3",
            "a row edited and another deleted are both sent")

        var many = StagedChanges()
        many.deletes.formUnion([2, 0])
        expect(summary(of: many), "delete id=1 | delete id=3", "deletes go in row order")
    }

    /// A column nobody typed into is absent from the INSERT, which is what makes
    /// the table's default apply to it.
    ///
    /// The distinction the three states carry is the whole of this feature's
    /// correctness: sending NULL for every untouched column would override every
    /// default a schema has — a `created_at DEFAULT now()` would land empty —
    /// and refusing to send NULL at all would make an explicitly emptied column
    /// impossible to write.
    private static func checkANewRowSendsOnlyTheColumnsItWasGiven() {
        var one = StagedChanges()
        one.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(summary(of: one), "insert label=new", "only the column that was filled in")

        var nulled = StagedChanges()
        nulled.drafts = [
            DraftRow(values: [1: PendingValue(text: nil), 2: PendingValue(text: "4")])
        ]
        expect(
            summary(of: nulled), "insert label=NULL,qty=4",
            "an emptied column is sent as NULL, in column order")

        var two = StagedChanges()
        two.drafts = [
            DraftRow(values: [1: PendingValue(text: "first")]),
            DraftRow(values: [1: PendingValue(text: "second")])
        ]
        expect(
            summary(of: two), "insert label=first | insert label=second",
            "two new rows are two INSERTs, in the order they were added")

        // A row nobody typed into still becomes an insert, carrying no cells,
        // which is how the core is asked for a row of every default. Whether it
        // may be asked for at all is a setting and is decided before this — see
        // `StagedChanges.refusal` and `--verify-preferences`. What is checked
        // here is only that the empty row is not quietly dropped on the way.
        var empty = StagedChanges()
        empty.drafts = [DraftRow()]
        expect(summary(of: empty), "insert ", "an untouched new row keeps its place in the batch")
    }

    /// A duplicated row copies what the grid is drawing — a staged edit included —
    /// with the key columns left out so the table's default supplies a fresh
    /// key.
    private static func checkADuplicatedRowCopiesEverythingButTheKey() {
        var staged = StagedChanges()
        staged.drafts = [staged.draft(copying: 0, from: table, clearing: ["id"])]
        expect(
            summary(of: staged), "insert label=one,qty=1.00",
            "the key is absent, so the table's default supplies it")

        var nulled = StagedChanges()
        nulled.drafts = [nulled.draft(copying: 1, from: table, clearing: ["id"])]
        expect(
            summary(of: nulled), "insert label=NULL,qty=2.00",
            "a NULL is copied as NULL, not as an absent column")

        // The copy is of the row on screen, so a staged edit rides along. The
        // draft is built into a separate `StagedChanges` before the summary,
        // or the summary would also carry the UPDATE the copy was made from.
        var edited = StagedChanges()
        edited.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "changed")
        var copy = StagedChanges()
        copy.drafts = [edited.draft(copying: 0, from: table, clearing: ["id"])]
        expect(
            summary(of: copy), "insert label=changed,qty=1.00",
            "the copy is of the row on screen, not of the row the database sent")

        var whole = StagedChanges()
        whole.drafts = [whole.draft(copying: 0, from: table, clearing: [])]
        expect(
            summary(of: whole), "insert id=1,label=one,qty=1.00",
            "the caller's key list is what removes the key, not this function")
    }

    /// The number beside Save is the number of statements Save will send.
    ///
    /// They are counted in two different places — one for the strip, one for the
    /// request — and a row that is both edited and deleted is where they would
    /// drift apart.
    private static func checkTheCountOnScreenIsTheNumberOfStatements() {
        var staged = StagedChanges()
        expect(staged.isEmpty, true, "nothing staged")
        expect(staged.count, 0, "and nothing to send")

        staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "a")
        staged.updates[GridCell(row: 0, column: 2)] = PendingValue(text: "1")
        staged.updates[GridCell(row: 1, column: 1)] = PendingValue(text: "b")
        staged.deletes.insert(2)
        expect(staged.count, 4, "three cells and a deleted row")
        expect(statements(of: staged), 3, "which is two UPDATEs and a DELETE")

        staged.deletes.insert(0)
        expect(staged.count, 3, "deleting the row those two cells were in")
        expect(statements(of: staged), 3, "leaves one UPDATE and two DELETEs")

        staged.drafts = [DraftRow(values: [1: PendingValue(text: "new")])]
        expect(staged.count, 4, "and a new row counts once however many columns it holds")
        expect(statements(of: staged), 4, "which is what Save sends")
    }

    /// A row the result cannot identify produces nothing at all.
    ///
    /// Refused rather than sent short: a `DELETE` whose `WHERE` lost a column of
    /// a composite key is a statement that runs, succeeds, and takes rows the
    /// user never saw.
    private static func checkARowThatCannotBeNamedIsRefused() {
        var staged = StagedChanges()
        staged.deletes.insert(0)
        expect(
            staged.request(
                schema: "app", relation: "orders", keyColumns: [], rows: table) == nil, true,
            "a relation with no primary key is refused")
        expect(
            staged.request(
                schema: "app", relation: "orders", keyColumns: ["id", "tenant"], rows: table)
                == nil, true,
            "and so is a key column the result does not carry")
    }

    // MARK: - Harness

    /// Three rows of a table with an integer key, standing in for the Arrow grid
    /// so that a request can be built with no database anywhere near it.
    private static let table = Rows(
        columnNames: ["id", "label", "qty"],
        cells: [
            ["1", "one", "1.00"],
            ["2", nil, "2.00"],
            ["3", "three", "3.00"]
        ])

    private struct Rows: StagedRows {
        let columnNames: [String]
        let cells: [[String?]]

        func value(row: Int, column: Int) -> String? { cells[row][column] }
    }

    /// The request as one line, which is the form a failure has to be read in.
    ///
    /// The alternative — comparing encoded JSON — says "these two strings
    /// differ" about a document with four levels of nesting, and the thing that
    /// differed is usually one value in the middle of it.
    private static func summary(of staged: StagedChanges) -> String {
        guard
            let request = staged.request(
                schema: "app", relation: "orders", keyColumns: ["id"], rows: table)
        else { return "(refused)" }
        let updates = request.updates.map { "update \(cells($0.key)) → \(cells($0.set))" }
        let inserts = request.inserts.map { "insert \(cells($0.set))" }
        let deletes = request.deletes.map { "delete \(cells($0.key))" }
        return (updates + inserts + deletes).joined(separator: " | ")
    }

    private static func statements(of staged: StagedChanges) -> Int {
        guard
            let request = staged.request(
                schema: "app", relation: "orders", keyColumns: ["id"], rows: table)
        else { return -1 }
        return request.updates.count + request.deletes.count + request.inserts.count
    }

    private static func cells(_ cells: [EditRequest.Cell]) -> String {
        cells.map { "\($0.column)=\($0.value ?? "NULL")" }.joined(separator: ",")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("editing FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
