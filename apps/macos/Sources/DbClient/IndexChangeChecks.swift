import AppKit

/// Executable checks for New Index and Drop Index, run by
/// `--verify-index-change`.
///
/// Two verbs behind one sheet, and the questions here are the ones an index
/// raises that a column does not: the key columns are a *list in an order*, the
/// access method is offered per server rather than from a fixed set, and the
/// index the primary key is made of is not one of these to drop.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum IndexChangeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEachChangeCrossesAsItsOwnWordAndPayload()
        checkTheKeyColumnsCrossInTheOrderTheyAreListed()
        checkTheItemsExistOnlyWhereTheStatementsAreWritten()
        checkTheMethodsOfferedAreTheOnesTheCoreNamed()
        checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow()
        checkTheFormAnswersItsOwnEmptyFields()
        if failures == 0 {
            fputs("index-change: all checks passed\n", stderr)
        } else {
            fputs("index-change: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - What crosses

    /// Each change spells itself and carries what its own verb needs.
    private static func checkEachChangeCrossesAsItsOwnWordAndPayload() {
        expect(IndexChange.create(NewIndex()).verb, "create", "the word the core reads for a make")
        expect(IndexChange.drop(name: "i").verb, "drop", "and for a drop")
        expect(
            [
                IndexChange.create(NewIndex()).isDestructive,
                IndexChange.drop(name: "i").isDestructive
            ], [false, true],
            "and only the drop takes anything away, which is what takes Return off its button")

        let dropped = encoded(.drop(name: "orders_sku_idx"))
        expect(dropped["change"] as? String, "drop", "a drop crosses as its word")
        expect(dropped["name"] as? String, "orders_sku_idx", "naming the index that is going")

        var index = NewIndex(name: "  orders_sku_idx  ", columns: [column("sku")], unique: true)
        var made = encoded(.create(index))
        expect(made["change"] as? String, "create", "a make crosses as its word")
        // Trimmed, the rule every other name on this side follows: an index
        // called " x " is one nobody can name again without the spaces.
        expect(made["name"] as? String, "orders_sku_idx", "carrying the name it was given, trimmed")
        expect(made["unique"] as? Bool, true, "and whether it refuses a repeated value")
        expect(
            made["method"] == nil, true,
            "and no method where none was picked, which is what takes the server's default")

        index.method = "hash"
        made = encoded(.create(index))
        expect(made["method"] as? String, "hash", "and the method where one was")
    }

    /// The key columns cross as names, in the order the rows are in.
    ///
    /// An index on `(a, b)` is not an index on `(b, a)` — the second is no use
    /// to a query that only knows `a` — so the order is the whole answer and not
    /// a presentation of it. The row identities stay on this side: two rows can
    /// hold the same name while somebody is still picking.
    private static func checkTheKeyColumnsCrossInTheOrderTheyAreListed() {
        let index = NewIndex(
            name: "orders_idx", columns: [column("sku"), column("qty"), column("id")])
        let made = encoded(.create(index))
        expect(
            made["columns"] as? [String], ["sku", "qty", "id"],
            "the columns cross as names in the order they were listed")

        // Two rows on the same name are what the core refuses; what this pins is
        // that both reach it, rather than one being swallowed by a list keyed on
        // the value.
        let twice = NewIndex(name: "orders_idx", columns: [column("sku"), column("sku")])
        expect(
            encoded(.create(twice))["columns"] as? [String], ["sku", "sku"],
            "and a name given twice crosses twice, for the core to refuse")

        // An empty row is a row somebody has not answered yet, and it is not the
        // same as no row: the first is a question, the second is an index.
        expect(NewIndex().columns.count, 1, "a new index opens with one row to fill in")
        expect(NewIndex().columns.first?.name, "", "and nothing chosen in it")
    }

    // MARK: - Whether the items exist

    /// The items are drawn where the core writes these statements, and nowhere
    /// else.
    private static func checkTheItemsExistOnlyWhereTheStatementsAreWritten() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesIndexes: false)
        expect(
            model.changesIndexes, false, "a database this build indexes nothing on offers nothing")

        model.sessions[0].capabilities = capabilities(changesIndexes: true)
        expect(model.changesIndexes, true, "one it does, offers both")

        // Its own capability. A build that read `changesColumns` for this would
        // pass every other check here.
        model.sessions[0].capabilities = capabilities(changesIndexes: false, changesColumns: true)
        expect(
            [model.changesColumns, model.changesIndexes], [true, false],
            "an index is a different object from the table's columns")

        model.sessions[0].capabilities = capabilities(changesIndexes: true)
        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.changesIndexes, false, "a read-only connection offers nothing that writes")
        model.prepareIndexChange(.drop(name: "i"), of: orders)
        expect(model.isIndexChangeSheetOpen, false, "and opening it directly is refused too")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.changesIndexes, false, "and nothing is offered over a statement in flight")
    }

    /// The method picker offers what the core said it can write, or nothing.
    ///
    /// A list rather than a flag, because the answer is per server: `gin` named
    /// for MySQL is a statement that reads correctly and is refused. An empty
    /// list is what the sheet reads to draw no picker at all — which is not the
    /// same as the server having one method, and is why this is not a `Bool`.
    private static func checkTheMethodsOfferedAreTheOnesTheCoreNamed() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(
            changesIndexes: true, indexMethods: ["btree", "hash", "gin", "gist", "brin"])
        expect(
            model.capabilities.indexMethods, ["btree", "hash", "gin", "gist", "brin"],
            "the picker's rows come from the core rather than from a list written here")

        model.sessions[0].capabilities = capabilities(changesIndexes: true)
        expect(
            model.capabilities.indexMethods.isEmpty, true,
            "and a server that names no method it is worth choosing offers no picker")

        // Nothing picked is nothing sent, which is what takes the server's own
        // default rather than naming it and being wrong on the next server.
        expect(NewIndex().method == nil, true, "a new index opens on the server's default")
        expect(
            encoded(.create(NewIndex(name: "i", columns: [column("sku")])))["method"] == nil, true,
            "and sends nothing for it")
    }

    // MARK: - What the button refuses

    /// The button waits for a statement written for the change as it is now.
    ///
    /// Every field of a new index changes the statement, so the plan compares
    /// the whole change. A column added to the list or the uniqueness unticked
    /// while the round trip was in the air would otherwise reach the server as
    /// the statement that had the old answer — and a `CREATE INDEX` that is not
    /// unique where somebody asked for unique is one nothing later will notice.
    private static func checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow() {
        let start = NewIndex(name: "orders_idx", columns: [column("sku")])
        let changes: [(String, (inout IndexChange) -> Void)] = [
            (
                "the name",
                {
                    if case .create(var i) = $0 {
                        i.name = "orders_idx_2"
                        $0 = .create(i)
                    }
                }
            ),
            (
                "whether it is unique",
                {
                    if case .create(var i) = $0 {
                        i.unique = true
                        $0 = .create(i)
                    }
                }
            ),
            (
                "how it is stored",
                {
                    if case .create(var i) = $0 {
                        i.method = "hash"
                        $0 = .create(i)
                    }
                }
            ),
            (
                "which columns are in it",
                {
                    if case .create(var i) = $0 {
                        i.columns.append(column("qty"))
                        $0 = .create(i)
                    }
                }
            ),
            (
                "the order they are in",
                {
                    if case .create(var i) = $0 {
                        i.columns = [column("qty"), column("sku")]
                        $0 = .create(i)
                    }
                }
            )
        ]
        for (what, edit) in changes {
            let model = opened(.create(start))
            answer(model, "CREATE INDEX orders_idx ON public.orders (sku);")
            expect(model.indexChangeObstacle, nil, "the statement matches, so it can run")

            model.editIndexChange(edit)
            expect(
                model.indexChangeObstacle, "Writing it…",
                "changing \(what) makes the statement on screen the wrong one")
            expect(
                model.indexPlan?.statement ?? nil, nil,
                "and what the button would run is nothing at all")
            expect(
                model.indexPlan?.preview.contains("CREATE INDEX") ?? false, true,
                "while the pane still shows one, a pane that blanked per keystroke being unusable")
        }
    }

    /// The two empty fields are answered here rather than by the core.
    private static func checkTheFormAnswersItsOwnEmptyFields() {
        let unnamed = opened(.create(NewIndex(columns: [column("sku")])))
        expect(
            unnamed.indexChangeObstacle, "An index needs a name.",
            "an index opens with no name, and says so")
        unnamed.editIndexChange {
            if case .create(var i) = $0 {
                i.name = "   "
                $0 = .create(i)
            }
        }
        expect(
            unnamed.indexChangeObstacle, "An index needs a name.",
            "and whitespace is empty, which is what stops an index called three spaces")

        let unanswered = opened(.create(NewIndex(name: "orders_idx")))
        expect(
            unanswered.indexChangeObstacle, "Every column of an index needs a name.",
            "and a row nobody has chosen a column for is answered as the row it is")
    }

    // MARK: - Harness

    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: nil)

    private static func column(_ name: String) -> IndexColumn {
        IndexColumn(name: name)
    }

    /// A model with the sheet open on `change`, over a connection that writes.
    private static func opened(_ change: IndexChange) -> AppModel {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesIndexes: true)
        model.prepareIndexChange(change, of: orders)
        return model
    }

    /// Stands in for the connection having written a statement for the change
    /// now on the plan, which is what `renderIndexChange` does with the answer.
    private static func answer(_ model: AppModel, _ statement: String) {
        guard let plan = model.indexPlan else { return }
        model.indexPlan?.written = AppModel.IndexChangePlan.Written(
            change: plan.change, text: statement, refusal: nil)
    }

    /// One change as the core will read it.
    private static func encoded(_ change: IndexChange) -> [String: Any] {
        guard let data = try? JSONEncoder().encode(change),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            failures += 1
            fputs("index-change FAIL: a change could not be written as JSON\n", stderr)
            return [:]
        }
        // The index rides inside the create's payload, and what every assertion
        // above is about is the index — so it is lifted here rather than in
        // each of them.
        if let index = object["index"] as? [String: Any] {
            return object.merging(index) { _, inner in inner }
        }
        return object
    }

    private static func capabilities(
        changesIndexes: Bool, changesColumns: Bool = false, indexMethods: [String] = []
    ) -> Capabilities {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: true, editsRows: true, schemaIsTheDatabase: false,
            reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: false, changesColumns: changesColumns, altersColumns: false,
            changesIndexes: changesIndexes, indexMethods: indexMethods, changesDatabases: false)
    }

    /// A model with no connection, built the way `ColumnChangeChecks` builds its
    /// own: a throwaway defaults suite, so that running the checks cannot read
    /// or write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-index-change"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-index-change"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("index-change FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
