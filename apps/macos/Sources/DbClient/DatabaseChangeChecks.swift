import AppKit

/// Executable checks for New Database and Drop Database, run by
/// `--verify-database-change`.
///
/// The most destructive button in the application is on this sheet — a dropped
/// database takes every relation in it — so what these pin is everything that
/// stands between a click and that statement: which word crosses, whether the
/// item exists at all, and the two states where the button has to refuse.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum DatabaseChangeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEachChangeCrossesAsItsOwnWord()
        checkTheItemsExistOnlyWhereTheStatementsAreWritten()
        checkTheTwoCapabilitiesMoveIndependently()
        checkNothingRunsInsideATransaction()
        checkTheDatabaseThisTabIsOnCannotBeDropped()
        checkTheButtonWaitsForAStatementWrittenForTheNameNowTyped()
        if failures == 0 {
            fputs("database-change: all checks passed\n", stderr)
        } else {
            fputs("database-change: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The word that crosses

    /// Each change spells itself, and the two spellings are distinct.
    ///
    /// The raw values are what `db_database_change_sql` reads, and the core
    /// refuses a word it does not know rather than defaulting to one — so a
    /// spelling changed here becomes a refusal and not a drop where a create was
    /// meant. With only two words, defaulting to either is the whole risk.
    private static func checkEachChangeCrossesAsItsOwnWord() {
        expect(DatabaseChange.create.rawValue, "create", "the word the core reads for a create")
        expect(DatabaseChange.drop.rawValue, "drop", "and for a drop")
        expect(
            Set(DatabaseChange.allCases.map(\.rawValue)).count, 2,
            "two distinct words, since the core picks the statement by this alone")
        expect(
            DatabaseChange.allCases.map(\.isDestructive), [false, true],
            "and only one of them destroys anything")

        // "Database" on every engine, and not `AppModel.containerNoun`, which
        // names the schema level — PostgreSQL calls that a schema, and borrowing
        // it here put "A schema needs a name." under a `CREATE DATABASE`.
        expect(DatabaseChange.create.menuTitle, "New Database…", "the item that makes one")
        expect(DatabaseChange.drop.menuTitle, "Drop Database…", "and the one that removes it")
    }

    // MARK: - Whether the items exist

    /// The items are drawn where the core writes these statements, and nowhere
    /// else.
    private static func checkTheItemsExistOnlyWhereTheStatementsAreWritten() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(changesDatabases: false)
        expect(model.changesDatabases, false, "a database this build cannot make offers nothing")

        model.sessions[0].capabilities = capabilities(changesDatabases: true)
        expect(model.changesDatabases, true, "one it can, offers both")

        model.sessions[0].safety = ConnectionSafety(isReadOnly: true)
        expect(model.changesDatabases, false, "a read-only connection offers nothing that writes")
        model.prepareDatabaseChange(.create)
        expect(model.isDatabaseChangeSheetOpen, false, "and opening it directly is refused too")

        model.sessions[0].safety = ConnectionSafety(isProduction: true)
        expect(
            model.changesDatabases, true,
            "a production mark warns and does not forbid, which is what makes it different")

        model.sessions[0].safety = ConnectionSafety()
        model.sessions[0].isBusy = true
        expect(model.changesDatabases, false, "and nothing is offered over a statement in flight")
    }

    /// Making a database and changing a relation are separate capabilities.
    ///
    /// SQLite is the case that proves it and the reason there are two flags: it
    /// drops and renames a table, and a SQLite database is a file — made by
    /// opening a path, not by sending SQL. A build that read one flag for both
    /// would either offer SQLite a New Database it cannot write or take the Drop
    /// Table away from it.
    private static func checkTheTwoCapabilitiesMoveIndependently() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(
            changesDatabases: false, changesRelations: true)
        expect(
            [model.changesRelations, model.changesDatabases], [true, false],
            "SQLite's answer: relations yes, databases no")

        model.sessions[0].capabilities = capabilities(
            changesDatabases: true, changesRelations: true)
        expect(
            [model.changesRelations, model.changesDatabases], [true, true],
            "and PostgreSQL's, which is both")
    }

    // MARK: - What the button refuses

    /// Neither statement runs inside a transaction.
    ///
    /// One rule for both engines rather than a per-dialect flag, and the reason
    /// is that both failures are bad in different ways: PostgreSQL refuses with
    /// `cannot run inside a transaction block`, and MySQL commits whatever was
    /// open before running it — ending somebody's transaction without being
    /// asked. Refusing here is the only answer that is true on both.
    private static func checkNothingRunsInsideATransaction() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesDatabases: true)
        model.prepareDatabaseChange(.create)
        model.setNewDatabaseName("reporting")
        answer(model, "CREATE DATABASE reporting;")
        expect(model.databaseChangeObstacle, nil, "in autocommit, the statement is ready")

        model.sessions[0].transaction = TransactionState(
            transactional: true, autocommit: false, open: true, savepoints: [])
        expect(
            model.databaseChangeObstacle?.contains("outside a transaction") ?? false, true,
            "with one open, the button waits rather than sending a statement that will be "
                + "refused or that will silently commit")

        model.sessions[0].transaction = TransactionState(
            transactional: true, autocommit: false, open: false, savepoints: [])
        expect(
            model.databaseChangeObstacle, nil,
            "manual commit with nothing open is not an open transaction")
    }

    /// The database this tab is connected to cannot be dropped from this tab.
    ///
    /// The server refuses while any session is on it, and the session is this
    /// one — so without this the button sends a statement that can only fail.
    /// Said here, where it can also say what to do instead.
    private static func checkTheDatabaseThisTabIsOnCannotBeDropped() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesDatabases: true)
        model.sessions[0].databases = [
            DatabaseInfo(name: "bench", isCurrent: true),
            DatabaseInfo(name: "reporting", isCurrent: false)
        ]

        model.prepareDatabaseChange(.drop, named: "reporting")
        answer(model, "DROP DATABASE reporting;")
        expect(model.databaseChangeObstacle, nil, "another database is droppable")

        model.prepareDatabaseChange(.drop, named: "bench")
        answer(model, "DROP DATABASE bench;")
        expect(
            model.databaseChangeObstacle?.contains("connected to") ?? false, true,
            "the one this tab is on is not, and the sentence says to open another first")

        // A create of the same name is a different question, and the server's to
        // answer: this rule is about the drop only.
        model.prepareDatabaseChange(.create)
        model.setNewDatabaseName("bench")
        answer(model, "CREATE DATABASE bench;")
        expect(
            model.databaseChangeObstacle, nil,
            "creating one that is already there is the server's refusal to give, not this side's")
    }

    /// The button waits for a statement written for the name now in the field.
    ///
    /// The name is typed here and the statement is written on the other side of
    /// a round trip, so for as long as that takes the two are about different
    /// databases. The rule the relation sheet has, and it matters more here: the
    /// statement that arrives late names a database that would be made under a
    /// name nobody is looking at.
    private static func checkTheButtonWaitsForAStatementWrittenForTheNameNowTyped() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(changesDatabases: true)
        model.prepareDatabaseChange(.create)

        expect(
            model.databaseChangeObstacle, "A database needs a name.",
            "an empty field is answered here rather than by the core")
        model.setNewDatabaseName("   ")
        expect(
            model.databaseChangeObstacle, "A database needs a name.",
            "and whitespace is empty, which is what stops a database called three spaces")

        model.setNewDatabaseName("reporting")
        answer(model, "CREATE DATABASE reporting;")
        expect(model.databaseChangeObstacle, nil, "the statement matches the name, so it can run")

        // One more keystroke, and nothing has come back for it yet.
        model.setNewDatabaseName("reporting_2026")
        expect(
            model.databaseChangeObstacle, "Writing it…",
            "the button waits rather than making a database under the older name")
        expect(
            model.databasePlan?.preview.contains("reporting;") ?? false, true,
            "while the pane still shows one, because a pane that blanked per keystroke is unusable")
        expect(
            model.databasePlan?.statement ?? nil, nil,
            "and what the button would run is nothing at all")

        answer(model, "CREATE DATABASE reporting_2026;")
        expect(model.databaseChangeObstacle, nil, "the answer for this name releases it")
    }

    // MARK: - Harness

    /// Stands in for the connection having written a statement for the name now
    /// on the plan, which is what `renderDatabaseChange` does with the answer.
    private static func answer(_ model: AppModel, _ statement: String) {
        guard let plan = model.databasePlan else { return }
        model.databasePlan?.written = AppModel.DatabaseChangePlan.Written(
            name: plan.name, text: statement, refusal: nil)
    }

    private static func capabilities(changesDatabases: Bool, changesRelations: Bool = false)
        -> Capabilities
    {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            // True throughout, so that every case above turns on the narrower
            // flag rather than on this one.
            writesStatements: true, schemaIsTheDatabase: false, reportsRoutines: false,
            reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
            changesRelations: changesRelations, changesColumns: false, altersColumns: false,
            changesDatabases: changesDatabases)
    }

    /// A model with no connection, built the way `VariablesChecks` builds its
    /// own: a throwaway defaults suite, so that running the checks cannot read or
    /// write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-database-change"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-database-change"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("database-change FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
