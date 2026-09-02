import AppKit

/// Executable checks for Drop, Empty and Rename, run by `--verify-relation-change`.
///
/// The only sheet in this application whose button destroys something the server
/// will not give back, which is what the rules below are all about: which of
/// three words crosses the boundary, whether the menu that offers them is drawn
/// at all, and whether the statement on screen is the statement the button runs.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum RelationChangeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEachChangeCrossesAsItsOwnWord()
        checkTheMenuIsDrawnOnlyWhereTheStatementsAreWritten()
        checkASheetOpensOnTheRelationItWasAskedAbout()
        checkARenameWithNothingToRenameToIsRefusedBeforeItIsSent()
        checkTheButtonWaitsForAStatementWrittenForTheNameNowTyped()
        if failures == 0 {
            fputs("relation-change: all checks passed\n", stderr)
        } else {
            fputs("relation-change: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The word that crosses

    /// Each change spells itself, and the three spellings are distinct.
    ///
    /// The raw values are what `db_table_change_sql` reads, and the core refuses
    /// a word it does not know rather than defaulting to one — so a spelling
    /// changed on this side becomes a refusal and not a drop where a rename was
    /// meant. This check is what stops the three from being renamed into each
    /// other's words, which is the one edit here that would be silent.
    ///
    /// The two tenses are checked with them because they are what the window
    /// says a change did, and "Renameped" is the sort of thing that survives a
    /// review of the code and not of the screen.
    private static func checkEachChangeCrossesAsItsOwnWord() {
        expect(TableChange.drop.rawValue, "drop", "the word the core reads for a drop")
        expect(TableChange.truncate.rawValue, "truncate", "and for emptying a table")
        expect(TableChange.rename.rawValue, "rename", "and for a rename")
        expect(
            Set(TableChange.allCases.map(\.rawValue)).count, 3,
            "three distinct words, since the core picks the statement by this alone")

        expect(
            TableChange.allCases.map(\.actionTitle), ["Drop", "Empty", "Rename"],
            "what the button says — Empty rather than Truncate, which is the server's word")
        expect(
            TableChange.allCases.map(\.pastTense), ["Dropped", "Emptied", "Renamed"],
            "and what the status line says afterwards")
        expect(
            TableChange.allCases.map(\.isDestructive), [true, true, false],
            "the two that cannot be undone say so and the rename does not")
    }

    // MARK: - Whether the menu exists

    /// The row menu is drawn where the core writes these statements, and nowhere
    /// else.
    ///
    /// Its own capability rather than `writesStatements`: every dialect the core
    /// carries can have a `SELECT` composed for it and only three of the six have
    /// had a drop written, so a menu keyed on the wider flag would put three
    /// items on a ClickHouse table that refuse whichever is clicked.
    ///
    /// A read-only connection loses them too. That mark is the one thing in this
    /// window that says "this connection is not for changing", and a Drop item
    /// under it would be the mark not being kept.
    private static func checkTheMenuIsDrawnOnlyWhereTheStatementsAreWritten() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesRelations: false)
        expect(
            model.changesRelations, false, "a database whose statements are unwritten offers none")

        model.sessions[0].capabilities = capabilities(changesRelations: true)
        expect(model.changesRelations, true, "one they are written for offers all three")

        // Separate from the flag that decides whether a `SELECT` can be composed,
        // which is the confusion this capability exists to prevent.
        model.sessions[0].capabilities = capabilities(changesRelations: false)
        expect(
            [model.capabilities.writesStatements, model.changesRelations], [true, false],
            "a connection can compose statements and still write none of these three")

        model.sessions[0].capabilities = capabilities(changesRelations: true)
        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.changesRelations, false, "a read-only connection offers nothing that writes")

        model.sessions[0].safety = ConnectionSafety(isProduction: true)
        expect(
            model.changesRelations, true,
            "a production mark warns and does not forbid, which is what makes it different")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.changesRelations, false, "and nothing is offered over a statement in flight")
    }

    // MARK: - Opening the sheet

    /// The sheet opens on the relation and verb it was asked about, and a rename
    /// starts from the name the relation already has.
    ///
    /// The seeded name is not a nicety: the field is the only editable thing on
    /// the sheet, and an empty one would make the common rename — change three
    /// letters of a long name — into retyping the whole thing.
    ///
    /// A read-only connection is refused here as well as having no menu, because
    /// the menu is not the only way in: `prepareRelationChange` is what the item
    /// calls, and a second caller would otherwise reach it unguarded.
    private static func checkASheetOpensOnTheRelationItWasAskedAbout() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesRelations: true)

        model.prepareRelationChange(.drop, of: orders)
        expect(model.isRelationChangeSheetOpen, true, "the sheet is up")
        expect(model.changePlan?.change, .drop, "on the verb that was clicked")
        expect(model.changePlan?.qualified, "public.orders", "about the row that was clicked")
        expect(model.changePlan?.newName, "", "a drop reads no name, so none is seeded")

        model.prepareRelationChange(.rename, of: orders)
        expect(
            model.changePlan?.newName, "orders",
            "a rename opens on the name it has, so a small change is a small edit")

        model.changePlan = nil
        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        model.prepareRelationChange(.drop, of: orders)
        expect(model.isRelationChangeSheetOpen, false, "and a read-only connection opens nothing")
        expect(
            model.errorMessage?.contains("public.orders") ?? false, true,
            "saying which relation was left alone")
    }

    // MARK: - What the button refuses

    /// A rename needs a name, and one that is not the name already there.
    ///
    /// Both are caught while the field is still on screen. The blank one never
    /// reaches the core — which would refuse it, but with a sentence about an
    /// argument rather than about the field somebody is looking at. The unchanged
    /// one would be accepted by some of these servers and do nothing, which is
    /// the worse outcome: a window that reports success for a rename that did not
    /// happen.
    private static func checkARenameWithNothingToRenameToIsRefusedBeforeItIsSent() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesRelations: true)
        model.prepareRelationChange(.rename, of: orders)

        model.setRelationNewName("")
        expect(
            model.relationChangeObstacle, "A rename needs a new name.",
            "a blank name is answered here rather than by the core")
        model.setRelationNewName("   ")
        expect(
            model.relationChangeObstacle, "A rename needs a new name.",
            "and whitespace is blank, which is what stops a table called four spaces")

        // As though the core had answered for this name.
        model.setRelationNewName("orders")
        answer(model, "ALTER TABLE public.orders RENAME TO orders;")
        expect(
            model.relationChangeObstacle, "That is the name it already has.",
            "a rename to the name it has is refused, statement or no statement")

        model.setRelationNewName("orders_2026")
        answer(model, "ALTER TABLE public.orders RENAME TO orders_2026;")
        expect(model.relationChangeObstacle, nil, "and a different name is ready to run")
    }

    /// The button waits for a statement written for the name now in the field.
    ///
    /// The statement is composed on the other side of a round trip while the name
    /// is typed on this one, so for as long as that takes the two are about
    /// different tables. The pane goes on showing the older statement — blanking
    /// on every keystroke would be unreadable — and this is what stops the button
    /// from running it: a `RENAME TO orders_2025` sent because it arrived while
    /// the last three characters were being changed is a rename nobody asked for
    /// and nobody saw.
    private static func checkTheButtonWaitsForAStatementWrittenForTheNameNowTyped() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesRelations: true)
        model.prepareRelationChange(.rename, of: orders)

        model.setRelationNewName("orders_2025")
        answer(model, "ALTER TABLE public.orders RENAME TO orders_2025;")
        expect(model.relationChangeObstacle, nil, "the statement matches the name, so it can run")

        // One more keystroke, and nothing has come back for it yet.
        model.setRelationNewName("orders_2026")
        expect(
            model.relationChangeObstacle, "Writing it…",
            "the button waits rather than running the statement for the older name")
        expect(
            model.changePlan?.preview.contains("orders_2025") ?? false, true,
            "while the pane still shows one, because a pane that blanked per keystroke is unusable")
        expect(
            model.changePlan?.statement ?? nil, nil,
            "and what the button would run is nothing at all")

        answer(model, "ALTER TABLE public.orders RENAME TO orders_2026;")
        expect(model.relationChangeObstacle, nil, "the answer for this name releases it")

        // A refusal travels with the name the same way a statement does.
        model.setRelationNewName("select")
        refuse(model, "that name is a keyword")
        expect(
            model.relationChangeObstacle, "that name is a keyword",
            "and a refusal for the name now typed is what the footer says")
    }

    // MARK: - Harness

    /// The relation every case above acts on. A plain table, because the kinds
    /// that cannot take a change are refused by the core and read here as
    /// whatever it said.
    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: 1200)

    /// Stands in for the connection having written a statement for the name now
    /// on the plan, which is what `renderRelationChange` does with the answer.
    private static func answer(_ model: AppModel, _ statement: String) {
        guard let plan = model.changePlan else { return }
        model.changePlan?.written = AppModel.RelationChangePlan.Written(
            newName: plan.newName, text: statement, refusal: nil)
    }

    /// The same, for an answer that was a refusal.
    private static func refuse(_ model: AppModel, _ why: String) {
        guard let plan = model.changePlan else { return }
        model.changePlan?.written = AppModel.RelationChangePlan.Written(
            newName: plan.newName, text: nil, refusal: why)
    }

    private static func capabilities(changesRelations: Bool) -> Capabilities {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            // True throughout, so that every case above turns on the narrower
            // flag rather than on this one.
            writesStatements: true, schemaIsTheDatabase: false, reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: changesRelations, changesColumns: false, altersColumns: false,
            changesIndexes: false, indexMethods: [],
            changesDatabases: false)
    }

    /// A model with no connection, built the way `VariablesChecks` builds its
    /// own: a throwaway defaults suite, so that running the checks cannot read or
    /// write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-relation-change"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-relation-change"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("relation-change FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
