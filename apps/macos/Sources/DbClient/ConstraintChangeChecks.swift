import AppKit

/// Executable checks for New Constraint, New Foreign Key and their drops, run by
/// `--verify-constraint-change`.
///
/// Two verbs and three sorts behind one sheet, and the questions here are the
/// ones a constraint raises that an index does not: what crosses depends on
/// which sort was chosen, the drop cannot be written without carrying that sort
/// with it, and the section the item was opened from decides what the object is
/// called.
///
/// Behind a flag on the binary for the reason `IndexChangeChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum ConstraintChangeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEachChangeCrossesAsItsOwnWordAndPayload()
        checkOnlyTheChosenSortsAnswersCross()
        checkTheDropCarriesTheSortItCannotBeWrittenWithout()
        checkTheMenuNamesTheObjectTheSectionDoes()
        checkTheItemsExistOnlyWhereTheStatementsAreWritten()
        checkAKindWithNoFormToRemakeItIsNotOfferedADrop()
        checkTheFormAnswersItsOwnEmptyFields()
        checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow()
        if failures == 0 {
            fputs("constraint-change: all checks passed\n", stderr)
        } else {
            fputs("constraint-change: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - What crosses

    /// Each change spells itself and carries what its own verb needs.
    private static func checkEachChangeCrossesAsItsOwnWordAndPayload() {
        expect(
            ConstraintChange.create(NewConstraint()).verb, "create",
            "the word the core reads for a make")
        expect(
            ConstraintChange.drop(name: "c", sort: .check).verb, "drop", "and for a drop")
        expect(
            [
                ConstraintChange.create(NewConstraint()).isDestructive,
                ConstraintChange.drop(name: "c", sort: .check).isDestructive
            ], [false, true],
            "and only the drop takes anything away, which is what takes Return off its button")

        var made = encoded(.create(NewConstraint(sort: .unique, name: "  orders_sku_key  ")))
        expect(made["change"] as? String, "create", "a make crosses as its word")
        expect(made["sort"] as? String, "unique", "carrying which of the three sorts it is")
        // Trimmed, the rule every other name on this side follows: a constraint
        // called " x " is one nobody can name again without the spaces.
        expect(
            made["name"] as? String, "orders_sku_key", "and the name it was given, trimmed")

        made = encoded(
            .create(
                NewConstraint(
                    sort: .check, name: "orders_qty_check", expression: "  qty > 0  ")))
        expect(made["sort"] as? String, "check", "a check says so")
        expect(
            made["expression"] as? String, "qty > 0",
            "and carries the expression it was given, which is sent as typed")

        made = encoded(.create(foreignKey()))
        expect(
            made["sort"] as? String, "foreign_key", "a foreign key crosses as the core spells it")
        expect(
            made["columns"] as? [String], ["customer_id", "region"],
            "carrying this table's columns in the order the rows are in")
        expect(
            made["other_columns"] as? [String], ["id", "region"],
            "and the columns each one points at, in the same order")
        expect(made["other_schema"] as? String, "sales", "and the container it points into")
        expect(made["other_table"] as? String, "customers", "and the table there")
        // The words and not the SQL. `NO ACTION` is written as nothing at all,
        // so a rule that crossed as its clause would be indistinguishable from a
        // rule nobody chose.
        expect(made["on_delete"] as? String, "cascade", "and what happens to these rows")
        expect(made["on_update"] as? String, "no_action", "and what happens when the key moves")

        // Spelled out rather than compared against each case's own raw value,
        // which would be the same mistake twice: the words are the core's, and a
        // rule renamed on this side would agree with itself and be refused by
        // name at the boundary. Every case of the picker is here, so a rule
        // added later has to be spelled here too.
        var crossed: [String] = []
        for action in ReferentialAction.allCases {
            var key = foreignKey()
            key.onDelete = action
            crossed.append(encoded(.create(key))["on_delete"] as? String ?? "")
        }
        expect(
            crossed, ["no_action", "restrict", "cascade", "set_null", "set_default"],
            "every rule the picker offers crosses as the word the core parses")
    }

    /// Only the chosen sort's answers cross.
    ///
    /// The struct behind the form holds every sort's fields, so that flipping
    /// the picker does not throw away what was typed. What the core reads is
    /// tagged, where a check has no columns and a unique constraint has nowhere
    /// to point — sending the lot would offer the boundary a constraint that is
    /// three things at once, and the core would have to decide which of them
    /// somebody meant.
    private static func checkOnlyTheChosenSortsAnswersCross() {
        // One filled-in form, read three ways: this is what somebody who tried
        // all three sorts before choosing leaves behind.
        var filled = foreignKey()
        filled.expression = "qty > 0"

        filled.sort = .check
        var made = encoded(.create(filled))
        expect(made["columns"] == nil, true, "a check sends no columns, having none")
        expect(made["other_table"] == nil, true, "and no table to point at")
        expect(made["on_delete"] == nil, true, "and no rule for rows it does not reference")

        filled.sort = .unique
        made = encoded(.create(filled))
        expect(
            made["columns"] as? [String], ["customer_id", "region"],
            "a unique constraint sends the columns it is over")
        expect(
            made["expression"] == nil, true, "and no expression, which is the other sort's field")
        expect(made["other_columns"] == nil, true, "and nothing about another table's columns")
        expect(made["other_schema"] == nil, true, "nor which container that table is in")
        expect(made["on_update"] == nil, true, "nor what should happen when its key changes")

        filled.sort = .foreignKey
        made = encoded(.create(filled))
        expect(
            made["expression"] == nil, true,
            "and a foreign key sends no expression, though the field still holds one")
    }

    /// The drop carries the sort, because the statement cannot be written
    /// without it.
    ///
    /// PostgreSQL drops all three with `DROP CONSTRAINT`; MySQL writes `DROP
    /// KEY` for a unique constraint, `DROP CONSTRAINT` for a check and `DROP
    /// FOREIGN KEY` for a key. Nothing in the name says which, and the row the
    /// item was opened from is the only thing that knows.
    private static func checkTheDropCarriesTheSortItCannotBeWrittenWithout() {
        for sort in ConstraintSort.allCases {
            let dropped = encoded(.drop(name: "orders_customer_fk", sort: sort))
            expect(dropped["change"] as? String, "drop", "a drop crosses as its word")
            expect(
                dropped["name"] as? String, "orders_customer_fk",
                "naming the constraint that is going")
            expect(
                dropped["sort"] as? String, sort.rawValue,
                "and the sort, which is what decides the noun MySQL drops it by")
            expect(
                dropped["constraint"] == nil, true,
                "and nothing else: a drop has no constraint to describe")
        }

        expect(
            ConstraintChange.drop(name: "c", sort: .unique).sort, .unique,
            "and the sort a drop was opened with is the one it acts on")
        expect(
            ConstraintChange.create(NewConstraint(sort: .check)).sort, .check,
            "while a make takes its sort from the form")
    }

    // MARK: - What the menu says

    /// The item names the object the way the section it was opened from does.
    ///
    /// Foreign keys have a section of their own in this window. "New
    /// Constraint…" on one of its rows would be naming that row as something the
    /// section does not, and somebody looking for the item that adds a key would
    /// be reading the wrong word.
    private static func checkTheMenuNamesTheObjectTheSectionDoes() {
        expect(
            ConstraintChange.create(NewConstraint(sort: .foreignKey)).menuTitle,
            "New Foreign Key…", "the foreign keys section offers a foreign key")
        expect(
            ConstraintChange.drop(name: "k", sort: .foreignKey).menuTitle,
            "Drop Foreign Key…", "and drops one")
        expect(
            ConstraintChange.create(NewConstraint(sort: .unique)).menuTitle,
            "New Constraint…", "while the constraints section offers a constraint")
        expect(
            ConstraintChange.drop(name: "c", sort: .check).menuTitle,
            "Drop Constraint…", "and drops one of those, whichever of the two kinds it is")

        // The button on the sheet is the doing, so no ellipsis on it — and it
        // says the verb rather than the noun, the noun being at the top of the
        // sheet already.
        expect(
            [
                ConstraintChange.create(NewConstraint()).actionTitle,
                ConstraintChange.drop(name: "c", sort: .check).actionTitle
            ], ["Create", "Drop"], "and the sheet's button says what pressing it does")
    }

    // MARK: - Whether the items exist

    /// The items are drawn where the core writes these statements, and nowhere
    /// else.
    private static func checkTheItemsExistOnlyWhereTheStatementsAreWritten() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesConstraints: false)
        expect(
            model.changesConstraints, false,
            "a database this build constrains nothing on offers nothing")

        model.sessions[0].capabilities = capabilities(changesConstraints: true)
        expect(model.changesConstraints, true, "one it does, offers both verbs")

        // SQLite, which is why this is a flag of its own. It makes and drops an
        // index, and its `ALTER TABLE` reaches a check constraint and nothing
        // else — a unique constraint or a foreign key is part of the text the
        // table was created from. A build that read `changesIndexes` for this
        // would draw two items that always refuse.
        model.sessions[0].capabilities = capabilities(
            changesConstraints: false, changesIndexes: true)
        expect(
            [model.changesIndexes, model.changesConstraints], [true, false],
            "and a server that indexes but cannot constrain gets the indexes menu only")

        model.sessions[0].capabilities = capabilities(changesConstraints: true)
        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.changesConstraints, false, "a read-only connection offers nothing that writes")
        model.prepareConstraintChange(.drop(name: "c", sort: .check), of: orders)
        expect(model.isConstraintChangeSheetOpen, false, "and opening it directly is refused too")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.changesConstraints, false, "and nothing is offered over a statement in flight")
    }

    /// A listed kind this form cannot remake is offered no drop.
    ///
    /// An exclusion constraint drops as `DROP CONSTRAINT` like the rest, so the
    /// statement is writable — but the row would then offer to take away
    /// something the New Constraint form has no way to put back, which is a menu
    /// that is destructive in one direction only.
    private static func checkAKindWithNoFormToRemakeItIsNotOfferedADrop() {
        expect(
            StructurePane.sort(of: .unique), .unique, "a unique constraint drops as what it is")
        expect(StructurePane.sort(of: .check), .check, "and a check as what it is")
        expect(
            StructurePane.sort(of: .exclude) == nil, true,
            "an exclusion constraint has no item, this form being unable to make one again")
        expect(
            StructurePane.sort(of: .other) == nil, true,
            "and neither has a kind nobody has read the catalog for")
    }

    // MARK: - What the button refuses

    /// The empty fields are answered here rather than by the core.
    ///
    /// Each is a question rather than a mistake, and each belongs to one sort:
    /// a check with no expression and a key with no table are different empty
    /// forms, and telling somebody about the wrong one is worse than saying
    /// nothing.
    private static func checkTheFormAnswersItsOwnEmptyFields() {
        // What the constraints section's item opens on. Not a foreign key: that
        // section lists checks and unique constraints, and a form that opened on
        // the sort the *other* section is about would be answering a question
        // nobody asked there.
        expect(NewConstraint().sort, .unique, "a new constraint opens on a sort its section lists")
        // An empty row is a row somebody has not answered yet, and it is not the
        // same as no row: the first is a question, the second is a constraint
        // over nothing, which the core refuses.
        expect(NewConstraint().columns.count, 1, "and with one row to fill in")
        expect(NewConstraint().columns.first?.name, "", "and nothing chosen in it")

        let unnamed = opened(.create(NewConstraint(sort: .check, expression: "qty > 0")))
        expect(
            unnamed.constraintChangeObstacle, "A constraint needs a name.",
            "a constraint opens with no name, and says so")
        unnamed.editConstraintChange { change in
            if case .create(var constraint) = change {
                constraint.name = "   "
                change = .create(constraint)
            }
        }
        expect(
            unnamed.constraintChangeObstacle, "A constraint needs a name.",
            "and whitespace is empty, which is what stops a constraint called three spaces")

        expect(
            AppModel.unanswered(
                .create(NewConstraint(sort: .check, name: "orders_qty_check", expression: "  "))),
            "A check constraint needs an expression.",
            "a check with nothing to check is answered as the check it is")
        expect(
            AppModel.unanswered(.create(NewConstraint(sort: .unique, name: "orders_sku_key"))),
            "Every column of a constraint needs a name.",
            "and a row nobody has chosen a column for is answered as the row it is")

        var key = NewConstraint(sort: .foreignKey, name: "orders_customer_fk")
        key.columns = [ConstraintColumn(name: "customer_id", other: "id")]
        var missingTable = key
        missingTable.otherTable = "  "
        expect(
            AppModel.unanswered(.create(missingTable)),
            "A foreign key needs a table to reference.",
            "a key pointing at nothing is answered before the columns are, there being nothing "
                + "for them to point into")

        var missingOther = key
        missingOther.otherTable = "customers"
        missingOther.columns = [ConstraintColumn(name: "customer_id", other: "  ")]
        expect(
            AppModel.unanswered(.create(missingOther)),
            "Every column needs the column it references.",
            "and a row with only this table's end filled in is answered as the half row it is")

        key.otherTable = "customers"
        expect(
            AppModel.unanswered(.create(key)) == nil, true,
            "while a key with both ends of every row is a question the core is asked")

        // A drop has no form to fill in, so nothing here refuses it — what it
        // waits for is the statement.
        expect(
            AppModel.unanswered(.drop(name: "orders_customer_fk", sort: .foreignKey)) == nil, true,
            "and a drop has no empty field, the name being the whole of it")
    }

    /// The button waits for a statement written for the change as it is now.
    ///
    /// Every field of a new constraint changes the statement, so the plan
    /// compares the whole change. A column added to the list or a referential
    /// rule changed while the round trip was in the air would otherwise reach
    /// the server as the statement that had the old answer — and a foreign key
    /// that cascades where somebody asked for restrict is one nothing later will
    /// notice.
    private static func checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow() {
        let start = foreignKey()
        let edits: [(String, (inout NewConstraint) -> Void)] = [
            ("the name", { $0.name = "orders_customer_fk_2" }),
            ("which sort it is", { $0.sort = .unique }),
            // Both ends filled in, because a row with only one is an unanswered
            // form rather than a changed statement — that path is the check
            // above this one.
            (
                "which columns are in it",
                { $0.columns.append(ConstraintColumn(name: "sku", other: "sku")) }
            ),
            ("the order they are in", { $0.columns.reverse() }),
            ("the column it points at", { $0.columns[0].other = "customer_id" }),
            ("the table it points at", { $0.otherTable = "people" }),
            ("the container that table is in", { $0.otherSchema = "public" }),
            ("what happens when the referenced row goes", { $0.onDelete = .restrict }),
            ("what happens when its key changes", { $0.onUpdate = .cascade })
        ]
        let written = """
            ALTER TABLE public.orders ADD CONSTRAINT orders_customer_fk \
            FOREIGN KEY (customer_id,region) REFERENCES sales.customers(id,region) \
            ON DELETE CASCADE;
            """
        for (what, edit) in edits {
            let model = opened(.create(start))
            answer(model, written)
            expect(model.constraintChangeObstacle, nil, "the statement matches, so it can run")

            model.editConstraintChange { change in
                if case .create(var constraint) = change {
                    edit(&constraint)
                    change = .create(constraint)
                }
            }
            expect(
                model.constraintChangeObstacle, "Writing it…",
                "changing \(what) makes the statement on screen the wrong one")
            expect(
                model.constraintPlan?.statement ?? nil, nil,
                "and what the button would run is nothing at all")
            expect(
                model.constraintPlan?.preview.contains("FOREIGN KEY") ?? false, true,
                "while the pane still shows one, a pane that blanked per keystroke being unusable")
        }

        // The refusal the core sends back is shown rather than swallowed, and it
        // stops the button just as an unanswered field does — this is the path
        // a server that cannot write the sort somebody picked arrives on.
        let refused = opened(.create(start))
        answer(
            refused, nil,
            refusal: "SQLite's ALTER TABLE reaches a check constraint and nothing "
                + "else")
        expect(
            refused.constraintChangeObstacle?.contains("ALTER TABLE") ?? false, true,
            "a core that will not write this statement says why, on the sheet")
    }

    // MARK: - Harness

    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: nil)

    /// A two-column foreign key with every field answered, which is the change
    /// with the most that can be got wrong.
    private static func foreignKey() -> NewConstraint {
        var key = NewConstraint(sort: .foreignKey, name: "orders_customer_fk")
        key.columns = [
            ConstraintColumn(name: "customer_id", other: "id"),
            ConstraintColumn(name: "region", other: "region")
        ]
        key.otherSchema = "sales"
        key.otherTable = "customers"
        key.onDelete = .cascade
        return key
    }

    /// A model with the sheet open on `change`, over a connection that writes.
    private static func opened(_ change: ConstraintChange) -> AppModel {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesConstraints: true)
        model.prepareConstraintChange(change, of: orders)
        return model
    }

    /// Stands in for the connection having answered for the change now on the
    /// plan, which is what `renderConstraintChange` does with what comes back.
    private static func answer(_ model: AppModel, _ statement: String?, refusal: String? = nil) {
        guard let plan = model.constraintPlan else { return }
        model.constraintPlan?.written = AppModel.ConstraintChangePlan.Written(
            change: plan.change, text: statement, refusal: refusal)
    }

    /// One change as the core will read it.
    private static func encoded(_ change: ConstraintChange) -> [String: Any] {
        guard let data = try? JSONEncoder().encode(change),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            failures += 1
            fputs("constraint-change FAIL: a change could not be written as JSON\n", stderr)
            return [:]
        }
        // The constraint rides inside the create's payload, and what most of the
        // assertions above are about is the constraint — so it is lifted here
        // rather than in each of them. A drop has no such payload, which is
        // itself asserted above.
        if let constraint = object["constraint"] as? [String: Any] {
            return object.merging(constraint) { _, inner in inner }
        }
        return object
    }

    private static func capabilities(changesConstraints: Bool, changesIndexes: Bool = false)
        -> Capabilities
    {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: true, editsRows: true, schemaIsTheDatabase: false,
            reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: false, changesColumns: false, altersColumns: false,
            changesIndexes: changesIndexes, indexMethods: [],
            changesConstraints: changesConstraints, changesDatabases: false)
    }

    /// A model with no connection, built the way `IndexChangeChecks` builds its
    /// own: a throwaway defaults suite, so that running the checks cannot read
    /// or write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-constraint-change"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-constraint-change"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("constraint-change FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
