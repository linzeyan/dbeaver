import AppKit

/// Executable checks for Add, Drop and Rename Column, run by
/// `--verify-column-change`.
///
/// Three verbs behind one sheet, which is what most of these are about: the plan
/// holds one of three shapes, the statement is written for exactly the shape it
/// holds, and the button runs nothing while those two disagree. A drop here is
/// smaller than a table's and irreversible in the same way — the values go with
/// the column.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum ColumnChangeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEachChangeCrossesAsItsOwnWordAndPayload()
        checkTheItemsExistOnlyWhereTheStatementsAreWritten()
        checkThisCapabilityMovesOnItsOwn()
        checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow()
        checkARenameToTheSameNameIsRefusedHere()
        checkTheFormAnswersItsOwnEmptyFields()
        checkAnAddedKeyColumnCannotBeLeftNullable()
        checkAnAlterationCrossesOnlyWhatMoved()
        checkAlteringIsAskedSeparatelyFromChanging()
        checkAnAlterationThatSaysNothingIsRefusedHere()
        if failures == 0 {
            fputs("column-change: all checks passed\n", stderr)
        } else {
            fputs("column-change: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - What crosses

    /// Each change spells itself and carries what its own verb needs.
    ///
    /// The core picks the statement by the tag alone and refuses one it does not
    /// know, so a spelling changed here becomes a refusal rather than a drop
    /// where a rename was meant — and the payload keys are what decide whether
    /// the right column is named at all.
    private static func checkEachChangeCrossesAsItsOwnWordAndPayload() {
        expect(ColumnChange.add(NewTableColumn()).verb, "add", "the word the core reads for an add")
        expect(ColumnChange.drop(name: "n").verb, "drop", "and for a drop")
        expect(ColumnChange.rename(name: "n", to: "m").verb, "rename", "and for a rename")
        expect(ColumnChange.alter(alteration()).verb, "alter", "and for an alteration")
        expect(
            [
                ColumnChange.add(NewTableColumn()).isDestructive,
                ColumnChange.drop(name: "n").isDestructive,
                ColumnChange.rename(name: "n", to: "m").isDestructive,
                ColumnChange.alter(alteration()).isDestructive
            ], [false, true, false, false],
            "and only the drop destroys anything, which is what takes Return off its button")

        let dropped = encoded(.drop(name: "note"))
        expect(dropped["change"] as? String, "drop", "a drop crosses as its word")
        expect(dropped["name"] as? String, "note", "naming the column that is going")

        let renamed = encoded(.rename(name: "note", to: "comment"))
        expect(renamed["name"] as? String, "note", "a rename names the column it starts from")
        expect(renamed["to"] as? String, "comment", "and the one it ends at")

        // The add's payload is the Create Table form's column, because it is the
        // same five answers — so a key renamed on that side lands here too.
        let added = encoded(.add(NewTableColumn(name: "note", kind: .int, nullable: false)))
        let column = added["column"] as? [String: Any] ?? [:]
        expect(column["name"] as? String, "note", "an add carries the whole column")
        expect(column["kind"] as? String, "int", "with the kind the core reads")
        expect(column["nullable"] as? Bool, false, "and whether it takes a null")
    }

    // MARK: - Whether the items exist

    /// The items are drawn where the core writes these statements, and nowhere
    /// else.
    private static func checkTheItemsExistOnlyWhereTheStatementsAreWritten() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesColumns: false)
        expect(model.changesColumns, false, "a database this build has no ALTER for offers nothing")

        model.sessions[0].capabilities = capabilities(changesColumns: true)
        expect(model.changesColumns, true, "one it has, offers all three")

        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.changesColumns, false, "a read-only connection offers nothing that writes")
        model.prepareColumnChange(.drop(name: "note"), of: orders)
        expect(model.isColumnChangeSheetOpen, false, "and opening it directly is refused too")

        model.sessions[0].safety = ConnectionSafety(isProduction: true)
        expect(
            model.changesColumns, true,
            "a production mark warns and does not forbid, which is what makes it different")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.changesColumns, false, "and nothing is offered over a statement in flight")
    }

    /// Changing a column and changing a relation are separate capabilities.
    ///
    /// They answer alike on every database this build writes today, which is
    /// exactly why this is worth pinning: a build that folded them into one flag
    /// would pass every other check here. Upstream is where they come apart —
    /// DBeaver writes SQLite's `DROP TABLE` and refuses its column drop outright,
    /// recreating the whole table — so the next renderer to be lit may well
    /// answer one and not the other.
    private static func checkThisCapabilityMovesOnItsOwn() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesColumns: false, changesRelations: true)
        expect(
            [model.changesRelations, model.changesColumns], [true, false],
            "a database whose tables this can drop and whose columns it cannot")

        model.sessions[0].capabilities = capabilities(changesColumns: true, changesRelations: false)
        expect(
            [model.changesRelations, model.changesColumns], [false, true],
            "and the other way round, which one flag could not say")
    }

    // MARK: - What the button refuses

    /// The button waits for a statement written for the change as it is now.
    ///
    /// All three verbs carry something the statement depends on, so the plan
    /// compares the whole change and not one field of it. A `NOT NULL` unticked
    /// or a type repicked while the round trip was in the air would otherwise
    /// reach the server as the statement that had the old answer.
    private static func checkTheButtonWaitsForAStatementWrittenForTheChangeAsItIsNow() {
        let changes: [(String, ColumnChange, (inout ColumnChange) -> Void)] = [
            (
                "the new name", .rename(name: "note", to: "comment"),
                { $0 = .rename(name: "note", to: "comment_2") }
            ),
            (
                "what the column holds", .add(NewTableColumn(name: "note", kind: .text)),
                {
                    if case .add(var c) = $0 {
                        c.kind = .int
                        $0 = .add(c)
                    }
                }
            ),
            (
                "whether it takes a null", .add(NewTableColumn(name: "note", kind: .text)),
                {
                    if case .add(var c) = $0 {
                        c.nullable = false
                        $0 = .add(c)
                    }
                }
            ),
            (
                "what it defaults to", .add(NewTableColumn(name: "note", kind: .text)),
                {
                    if case .add(var c) = $0 {
                        c.defaultValue = "'x'"
                        $0 = .add(c)
                    }
                }
            )
        ]
        for (what, start, edit) in changes {
            let model = opened(start)
            answer(model, "ALTER TABLE public.orders ADD COLUMN note text;")
            expect(model.columnChangeObstacle, nil, "the statement matches, so it can run")

            model.editColumnChange(edit)
            expect(
                model.columnChangeObstacle, "Writing it…",
                "changing \(what) makes the statement on screen the wrong one")
            expect(
                model.columnPlan?.statement ?? nil, nil,
                "and what the button would run is nothing at all")
            expect(
                model.columnPlan?.preview.contains("ADD COLUMN") ?? false, true,
                "while the pane still shows one, a pane that blanked per keystroke being unusable")
        }
    }

    /// A rename to the name it already has is stopped here.
    ///
    /// Some servers take it and do nothing, which is worse than a refusal
    /// because it looks like it worked. The rule the relation rename already has,
    /// and it matters more here: the sheet opens with the current name in the
    /// field, so this is the state it opens in.
    private static func checkARenameToTheSameNameIsRefusedHere() {
        let model = opened(.rename(name: "note", to: "note"))
        answer(model, "ALTER TABLE public.orders RENAME COLUMN note TO note;")
        expect(
            model.columnChangeObstacle, "That is the name it already has.",
            "the sheet opens on the current name and the button is shut until it moves")

        model.editColumnChange { $0 = .rename(name: "note", to: "comment") }
        answer(model, "ALTER TABLE public.orders RENAME COLUMN note TO comment;")
        expect(model.columnChangeObstacle, nil, "a different name releases it")
    }

    /// The two empty fields are answered here rather than by the core.
    private static func checkTheFormAnswersItsOwnEmptyFields() {
        let renaming = opened(.rename(name: "note", to: ""))
        expect(
            renaming.columnChangeObstacle, "A rename needs a new name.",
            "an empty field is answered as it empties rather than after a round trip")
        renaming.editColumnChange { $0 = .rename(name: "note", to: "   ") }
        expect(
            renaming.columnChangeObstacle, "A rename needs a new name.",
            "and whitespace is empty, which is what stops a column called three spaces")

        let adding = opened(.add(NewTableColumn()))
        expect(
            adding.columnChangeObstacle, "A column needs a name.",
            "an add opens with no name, and says so")
        adding.editColumnChange {
            if case .add(var c) = $0 {
                c.name = "  "
                $0 = .add(c)
            }
        }
        expect(adding.columnChangeObstacle, "A column needs a name.", "whitespace here too")
    }

    /// A column added as part of a key cannot be left nullable.
    ///
    /// The rule `editNewTable` applies, applied here for one column. The core
    /// refuses the added key outright — a key is a rule about the whole table —
    /// so this never reaches a server; what it stops is a plan that contradicts
    /// itself before the refusal that explains why.
    private static func checkAnAddedKeyColumnCannotBeLeftNullable() {
        let model = opened(.add(NewTableColumn(name: "id", kind: .int)))
        guard case .add(let before)? = model.columnPlan?.change else {
            failures += 1
            fputs("column-change FAIL: the plan is not an add\n", stderr)
            return
        }
        expect(before.nullable, true, "a column starts nullable")

        model.editColumnChange {
            if case .add(var c) = $0 {
                c.isPrimaryKey = true
                $0 = .add(c)
            }
        }
        guard case .add(let after)? = model.columnPlan?.change else { return }
        expect(after.nullable, false, "and marking it part of the key settles the other answer")
    }

    /// An alteration sends the properties that moved and leaves out the rest.
    ///
    /// The rule this whole verb rests on. A column the server describes as
    /// `character varying(64)` is none of the seven kinds the picker offers, so
    /// a payload that carried a type on every alteration would retype it to
    /// `text` while somebody was changing its default — silently, and in the one
    /// direction that loses the length.
    private static func checkAnAlterationCrossesOnlyWhatMoved() {
        let untouched = encoded(.alter(alteration()))
        expect(untouched["change"] as? String, "alter", "an alteration crosses as its word")
        expect(untouched["name"] as? String, "qty", "naming the column it acts on")
        expect(
            untouched["kind"] == nil, true,
            "and sends no type where none was picked, the column's own being one this build "
                + "cannot always spell")
        expect(untouched["nullable"] == nil, true, "nor a nullability nobody changed")
        expect(
            untouched["default"] as? String, "keep",
            "while the default says which of its three answers this is, a null being unable to "
                + "mean both leave it and take it away")

        var moved = alteration()
        moved.kind = .decimal(precision: 12, scale: 2)
        moved.nullable = false
        moved.defaultChange = .set("  0  ")
        let sent = encoded(.alter(moved))
        expect(sent["kind"] as? String, "decimal(12,2)", "a type picked crosses with its size")
        expect(sent["nullable"] as? Bool, false, "and a nullability chosen crosses as itself")
        expect(
            (sent["default"] as? [String: String])?["set"], "0",
            "and a default set crosses trimmed and tagged, as the Create Table form sends one")

        var removed = alteration()
        removed.defaultChange = .drop
        expect(
            encoded(.alter(removed))["default"] as? String, "drop",
            "while removing one is its own answer rather than an empty string")

        // The three that are not sent are still shown, which is what the sheet
        // reads to say what the column is now.
        let standing = alteration()
        expect(standing.currentType, "integer", "the server's own word for the type is kept")
        expect(standing.currentNullable, false, "and its nullability")
        expect(standing.currentDefault, "1", "and its default")
    }

    /// Altering a column and changing which columns there are do not answer
    /// together.
    ///
    /// SQLite is the whole reason: its `ALTER TABLE` adds, drops and renames a
    /// column and reaches nothing inside one, so the Edit Column item has to be
    /// drawn from its own flag or it would refuse every time it was clicked
    /// there.
    private static func checkAlteringIsAskedSeparatelyFromChanging() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesColumns: true, altersColumns: false)
        expect(
            [model.changesColumns, model.altersColumns], [true, false],
            "SQLite's answer, which one flag could not give")

        model.sessions[0].capabilities = capabilities(changesColumns: false, altersColumns: true)
        expect(
            [model.changesColumns, model.altersColumns], [false, true],
            "and the other way round")

        model.sessions[0].capabilities = capabilities(changesColumns: true, altersColumns: true)
        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.altersColumns, false, "a read-only connection offers neither")
        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.altersColumns, false, "and nothing is offered over a statement in flight")
    }

    /// An alteration that asks for nothing is stopped before a round trip.
    ///
    /// `ALTER TABLE t` with no clauses after it is a syntax error rather than a
    /// statement that does nothing, and this is the state the sheet opens in —
    /// so the answer has to be here rather than in the core's reply.
    private static func checkAnAlterationThatSaysNothingIsRefusedHere() {
        let model = opened(.alter(alteration()), altersColumns: true)
        expect(
            model.columnChangeObstacle, "Nothing about this column has been changed.",
            "the sheet opens with every property unchanged and says so")

        model.editColumnChange { pending in
            guard case .alter(var edited) = pending else { return }
            edited.defaultChange = .set("   ")
            pending = .alter(edited)
        }
        expect(
            model.columnChangeObstacle, "A default needs a value; removing one is its own answer.",
            "and a default set to nothing is a syntax error rather than a shorter statement")

        model.editColumnChange { pending in
            guard case .alter(var edited) = pending else { return }
            edited.defaultChange = .set("0")
            pending = .alter(edited)
        }
        answer(model, "ALTER TABLE public.orders ALTER COLUMN qty SET DEFAULT 0;")
        expect(model.columnChangeObstacle, nil, "a property that moved releases the button")

        // And the statement is written for the alteration as it is now, the
        // whole change being what the plan compares.
        model.editColumnChange { pending in
            guard case .alter(var edited) = pending else { return }
            edited.nullable = false
            pending = .alter(edited)
        }
        expect(
            model.columnChangeObstacle, "Writing it…",
            "a second property picked makes the statement on screen the wrong one")
    }

    // MARK: - Harness

    /// One column as the server describes it, and an alteration over it that
    /// asks for nothing yet.
    private static func alteration() -> ColumnAlteration {
        ColumnAlteration(
            ColumnInfo(
                name: "qty", dataType: "integer", nullable: false, position: 1,
                isPrimaryKey: false, defaultValue: "1"))
    }

    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: nil)

    /// A model with the sheet open on `change`, over a connection that writes.
    private static func opened(_ change: ColumnChange, altersColumns: Bool = false) -> AppModel {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(
            changesColumns: true, altersColumns: altersColumns)
        model.prepareColumnChange(change, of: orders)
        return model
    }

    /// Stands in for the connection having written a statement for the change now
    /// on the plan, which is what `renderColumnChange` does with the answer.
    private static func answer(_ model: AppModel, _ statement: String) {
        guard let plan = model.columnPlan else { return }
        model.columnPlan?.written = AppModel.ColumnChangePlan.Written(
            change: plan.change, text: statement, refusal: nil)
    }

    /// One change as the core will read it.
    private static func encoded(_ change: ColumnChange) -> [String: Any] {
        guard let data = try? JSONEncoder().encode(change),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            failures += 1
            fputs("column-change FAIL: a change could not be written as JSON\n", stderr)
            return [:]
        }
        return object
    }

    private static func capabilities(
        changesColumns: Bool, changesRelations: Bool = false, altersColumns: Bool = false
    ) -> Capabilities {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: true, editsRows: true, schemaIsTheDatabase: false,
            reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: changesRelations, changesColumns: changesColumns,
            altersColumns: altersColumns,
            changesIndexes: false, indexMethods: [], changesConstraints: false,
            changesDatabases: false)
    }

    /// A model with no connection, built the way `NewTableChecks` builds its own:
    /// a throwaway defaults suite, so that running the checks cannot read or
    /// write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-column-change"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-column-change"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("column-change FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
