import AppKit

/// Executable checks for the Create Table form, run by `--verify-new-table`.
///
/// The one sheet in this application that composes a statement out of answers
/// rather than reading one off something that exists, which is what the checks
/// here are about: every field changes the statement, so every field is a way for
/// the button to run a statement for a table nobody described.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum NewTableChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEveryKindCrossesAsItsOwnWord()
        checkAnEmptyDefaultIsAbsentRatherThanEmpty()
        checkTheFormExistsWhereTheCoreWritesDDL()
        checkAKeyColumnCannotBeLeftNullable()
        checkTheButtonWaitsForAStatementWrittenForEveryFieldAsItIsNow()
        checkTheFormAnswersItsOwnEmptyFields()
        checkTheMinusActsOnTheSelection()
        if failures == 0 {
            fputs("new-table: all checks passed\n", stderr)
        } else {
            fputs("new-table: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The words that cross

    /// Each kind spells itself, and the spelling survives a round trip.
    ///
    /// The seam with no compiler on it. The core reads these strings in
    /// `ColumnKind::parse` and refuses one it does not know, so a spelling
    /// changed on this side is a form that composes nothing — and the decimal's
    /// size is worse than that, a number lost between the two being a column
    /// that silently holds a different one.
    private static func checkEveryKindCrossesAsItsOwnWord() {
        let expected: [(ColumnKind, String)] = [
            (.text, "text"), (.int, "int"), (.float, "float"), (.bool, "bool"),
            (.date, "date"), (.timestamp, "timestamp"),
            (.decimal(precision: 18, scale: 4), "decimal(18,4)")
        ]
        for (kind, word) in expected {
            expect(kind.word, word, "the word the core reads for \(kind.label)")
            expect(ColumnKind(word: word), kind, "and the same word read back")
        }
        expect(
            Set(ColumnKind.offered.map(\.word)).count, ColumnKind.offered.count,
            "every kind the picker offers is a distinct word, since the core picks by this alone")

        // A decimal's two rows of the menu are one row, which is what the picker
        // compares by: `==` there would leave nothing selected the moment a
        // stepper moved.
        expect(
            ColumnKind.decimal(precision: 18, scale: 4)
                .isSameKind(as: .decimal(precision: 12, scale: 2)),
            true, "two sizes of decimal are one row of the picker")
        expect(ColumnKind.int.isSameKind(as: .float), false, "and two kinds are not")
    }

    /// A default nobody typed is absent, not empty.
    ///
    /// `DEFAULT` with nothing after it is a syntax error, so an empty field has
    /// to encode as null rather than as `""` — and whitespace is empty, which is
    /// what stops a column defaulting to two spaces.
    private static func checkAnEmptyDefaultIsAbsentRatherThanEmpty() {
        expect(encoded(NewTableColumn(name: "n"))["default"] as? String, nil, "no default typed")
        expect(
            encoded(NewTableColumn(name: "n", defaultValue: "   "))["default"] as? String, nil,
            "and whitespace is nothing typed")
        expect(
            encoded(NewTableColumn(name: "n", defaultValue: " now() "))["default"] as? String,
            "now()", "while an expression crosses trimmed and otherwise untouched")

        // The other four keys, because a renamed one is a field the core reads
        // as absent — and `primary_key` absent is a table with no key at all.
        let column = encoded(
            NewTableColumn(name: "id", kind: .int, nullable: false, isPrimaryKey: true))
        expect(column["name"] as? String, "id", "the column's name")
        expect(column["kind"] as? String, "int", "what it holds")
        expect(column["nullable"] as? Bool, false, "whether it takes a null")
        expect(column["primary_key"] as? Bool, true, "and whether it is part of the key")
    }

    // MARK: - Whether the form exists

    /// The form is offered where the core writes DDL, and nowhere else.
    ///
    /// `writesStatements` and not a narrower flag, deliberately: every dialect
    /// this build renders writes a `CREATE TABLE`, so there is no narrower
    /// question to ask. A capability invented for this would be one that answered
    /// true six times.
    private static func checkTheFormExistsWhereTheCoreWritesDDL() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(writesStatements: false)
        expect(model.makesTables, false, "a database this build writes no DDL for offers nothing")

        model.sessions[0].capabilities = capabilities(writesStatements: true)
        expect(model.makesTables, true, "one it does, offers the form")

        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.makesTables, false, "a read-only connection offers nothing that writes")
        model.prepareNewTable()
        expect(model.isNewTableSheetOpen, false, "and opening it directly is refused too")

        model.sessions[0].safety = ConnectionSafety(isProduction: true)
        expect(
            model.makesTables, true,
            "a production mark warns and does not forbid, which is what makes it different")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.makesTables, false, "and nothing is offered over a statement in flight")
    }

    // MARK: - What the form will not let through

    /// A key column comes out not nullable however it was set that way.
    ///
    /// The rule exists because two of these servers apply it silently:
    /// PostgreSQL and MySQL make a `PRIMARY KEY` column `NOT NULL` whatever the
    /// statement said. A form that sent "nullable" and got a column refusing
    /// nulls would have been overruled without being told, so the plan settles it
    /// and the statement on screen is what the server will do. The core refuses
    /// the pair outright, which is what makes this a rule and not a nicety.
    ///
    /// Asked of the model rather than of the checkbox, because the checkbox is
    /// not the only way in: the capture flag sets a whole column at once, and a
    /// rule that lived in the view would be one that path walks past.
    private static func checkAKeyColumnCannotBeLeftNullable() {
        let model = opened()
        model.editNewTable { $0.columns = [NewTableColumn(name: "id", kind: .int)] }
        expect(model.newTablePlan?.columns.first?.nullable, true, "a column starts nullable")

        model.editNewTable { $0.columns[0].isPrimaryKey = true }
        expect(
            model.newTablePlan?.columns.first?.nullable, false,
            "ticking Key answers the checkbox beside it")

        // Unticking says nothing about whether a null is wanted now, so the
        // answer stays and the checkbox merely opens again.
        model.editNewTable { $0.columns[0].isPrimaryKey = false }
        expect(
            model.newTablePlan?.columns.first?.nullable, false,
            "unticking it leaves the answer rather than inventing one")

        // Set in one go, which is the path the checkbox does not take: a column
        // handed to the plan already contradicting itself is settled the same way.
        model.editNewTable {
            $0.columns = [
                NewTableColumn(name: "id", kind: .int, nullable: true, isPrimaryKey: true)
            ]
        }
        expect(
            model.newTablePlan?.columns.first?.nullable, false,
            "and a whole column set at once is settled too")

        // Every column, not the first: a key over two of them is the case a loop
        // written as `if let first` gets wrong.
        model.editNewTable {
            $0.columns = [
                NewTableColumn(name: "id", kind: .int, isPrimaryKey: true),
                NewTableColumn(name: "at", kind: .date, isPrimaryKey: true),
                NewTableColumn(name: "note")
            ]
        }
        expect(
            model.newTablePlan?.columns.map(\.nullable), [false, false, true],
            "both halves of a two-column key, and nothing else")
    }

    /// The button waits for a statement written for the table as it is now.
    ///
    /// Every field changes the statement, which is why this is checked field by
    /// field: a plan that compared only the name would let a `NOT NULL` unticked
    /// while the round trip was in the air reach the server as the statement that
    /// had it. That is a column made the wrong way round, and nothing on screen
    /// would have said so.
    private static func checkTheButtonWaitsForAStatementWrittenForEveryFieldAsItIsNow() {
        let model = opened()
        model.editNewTable { plan in
            plan.name = "orders"
            plan.columns = [NewTableColumn(name: "id", kind: .int, nullable: false)]
        }
        answer(model, "CREATE TABLE public.orders (\n    id bigint NOT NULL\n);")
        expect(model.newTableObstacle, nil, "the statement matches the table, so it can run")

        // One change per case, each of them a different statement, and none of
        // them a name.
        let changes: [(String, (inout AppModel.NewTablePlan) -> Void)] = [
            ("the schema it goes in", { $0.schema = "reporting" }),
            ("what a column holds", { $0.columns[0].kind = .text }),
            ("whether it takes a null", { $0.columns[0].nullable = true }),
            ("whether it is part of the key", { $0.columns[0].isPrimaryKey = true }),
            ("what it defaults to", { $0.columns[0].defaultValue = "0" }),
            ("how many columns there are", { $0.columns.append(NewTableColumn(name: "note")) })
        ]
        for (what, change) in changes {
            let model = opened()
            model.editNewTable { plan in
                plan.name = "orders"
                plan.columns = [NewTableColumn(name: "id", kind: .int, nullable: false)]
            }
            answer(model, "CREATE TABLE public.orders (\n    id bigint NOT NULL\n);")
            model.editNewTable(change)
            expect(
                model.newTableObstacle, "Writing it…",
                "changing \(what) makes the statement on screen the wrong one")
            expect(
                model.newTablePlan?.statement ?? nil, nil,
                "and what the button would run is nothing at all")
            expect(
                model.newTablePlan?.preview.contains("id bigint") ?? false, true,
                "while the pane still shows one, a pane that blanked per keystroke being unusable")
        }
    }

    /// The two empty fields are answered here rather than by the core.
    ///
    /// Both would come back as a refusal from the other side of a round trip,
    /// which is a sentence that appears a moment after the field was emptied and
    /// for no visible reason. Said here, they appear as the field empties.
    private static func checkTheFormAnswersItsOwnEmptyFields() {
        let model = opened()
        expect(
            model.newTableObstacle, "A table needs a name.",
            "a form opens with no name, and says so")

        model.editNewTable { $0.name = "   " }
        expect(
            model.newTableObstacle, "A table needs a name.",
            "and whitespace is empty, which is what stops a table called three spaces")

        model.editNewTable { $0.name = "orders" }
        expect(
            model.newTableObstacle, "Every column needs a name.",
            "the row the form opens with is empty, and that is the next thing to say")

        model.editNewTable { $0.columns = [NewTableColumn(name: "id"), NewTableColumn()] }
        expect(
            model.newTableObstacle, "Every column needs a name.",
            "one named column does not answer for the one beside it")

        model.editNewTable { $0.columns[1].name = "note" }
        answer(model, "CREATE TABLE public.orders (\n    id text,\n    note text\n);")
        expect(model.newTableObstacle, nil, "and with both named, the statement is what is left")
    }

    /// The sidebar's minus acts on the selection, and there is not always one.
    ///
    /// Its own question from `changesRelations`, which is about the connection:
    /// the footer button is aimed at whatever is highlighted, and a window that
    /// has just opened has highlighted nothing.
    private static func checkTheMinusActsOnTheSelection() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(
            writesStatements: true, changesRelations: true)
        expect(model.canDropSelected, false, "with nothing selected there is nothing to drop")

        model.sessions[0].selected = RelationInfo(
            schema: "public", name: "orders", kind: .table, estimatedRows: nil)
        expect(model.canDropSelected, true, "and with something selected, that is what goes")

        model.sessions[0].capabilities = capabilities(
            writesStatements: true, changesRelations: false)
        expect(
            model.canDropSelected, false,
            "a database this build writes no DROP for offers it on nothing")
    }

    // MARK: - Harness

    /// A model with the form open on a connection that writes DDL.
    private static func opened() -> AppModel {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(writesStatements: true)
        model.prepareNewTable()
        return model
    }

    /// Stands in for the connection having written a statement for the table now
    /// on the plan, which is what `renderNewTable` does with the answer.
    private static func answer(_ model: AppModel, _ statement: String) {
        guard let plan = model.newTablePlan else { return }
        model.newTablePlan?.written = AppModel.NewTablePlan.Written(
            schema: plan.schema, name: plan.name, columns: plan.columns, text: statement,
            refusal: nil)
    }

    /// One column as the core will read it.
    private static func encoded(_ column: NewTableColumn) -> [String: Any] {
        guard let data = try? JSONEncoder().encode(column),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            failures += 1
            fputs("new-table FAIL: a column could not be written as JSON\n", stderr)
            return [:]
        }
        return object
    }

    private static func capabilities(writesStatements: Bool, changesRelations: Bool = false)
        -> Capabilities
    {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: writesStatements, schemaIsTheDatabase: false, reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: changesRelations, changesDatabases: false)
    }

    /// A model with no connection, built the way `DatabaseChangeChecks` builds
    /// its own: a throwaway defaults suite, so that running the checks cannot
    /// read or write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-new-table"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-new-table"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("new-table FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
