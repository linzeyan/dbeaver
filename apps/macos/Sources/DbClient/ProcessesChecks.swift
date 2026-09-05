import AppKit

/// Executable checks for the Server Processes sheet, run by `--verify-processes`.
///
/// Four rules, and the first is the one worth the file. A kill is aimed by an id
/// the server hands out and reuses, at a list that is replaced wholesale every
/// time it refreshes. If the row a Kill acts on were remembered rather than
/// looked up, a refresh landing between the click on a row and the click on the
/// button would leave the button aimed at an id the server had since given to
/// somebody else — and the sheet would look exactly the same either way.
///
/// The other three are the ones that fail quietly: which connections offer the
/// menu item at all, which of the two kills each server is asked for, and
/// whether the question is put before anything is sent.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum ProcessesChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheRowAKillActsOnIsLookedUpRatherThanRemembered()
        checkTheFilterReadsEveryColumnAndIgnoresCase()
        checkTheMenuItemIsOfferedOnlyWhereTheServerAnswers()
        checkEachServerIsOfferedOnlyTheKillsItWillPerform()
        checkNothingIsSentUntilTheQuestionIsAnswered()
        checkTheQuestionNamesTheSessionAndWhichKillItIs()
        checkShuttingTheSheetStopsThePolling()
        if failures == 0 {
            fputs("processes: all checks passed\n", stderr)
        } else {
            fputs("processes: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Aiming a kill

    /// The row a Kill acts on is found in the list in front, not remembered.
    ///
    /// The case this exists for is a refresh arriving between the click that
    /// selected a row and the click on Kill: the process ends, the list is
    /// replaced, and the id in `selectedProcess` now names either nothing or
    /// whatever the server has since given that number to. Looking it up means
    /// the buttons go quiet; remembering it means they stay live and aimed at a
    /// stranger.
    ///
    /// The filter is the same defect by a different route, and is the second
    /// half of this: a row narrowed out of sight is a row nobody can see they
    /// are about to kill.
    private static func checkTheRowAKillActsOnIsLookedUpRatherThanRemembered() {
        let model = makeModel()
        fill(model, with: rows)
        model.selectedProcess = "42"
        expect(model.chosenProcess?.id, "42", "the selected row is the one in front")

        // The refresh that ends it. Everything else about the sheet is unchanged.
        model.sessions[0].processes = rows.filter { $0.id != "42" }
        expect(
            model.chosenProcess?.id, nil,
            "a selection the list no longer holds names nothing, rather than an id the server "
                + "may have given to somebody else")

        model.sessions[0].processes = rows
        model.processFilter = "alice"
        model.selectedProcess = "77"
        expect(
            model.chosenProcess?.id, nil,
            "and a row the filter has hidden is not one a Kill can reach either")
    }

    /// The filter matches any column, in any case.
    ///
    /// Every column because there is no way to say which one is being typed
    /// into, and somebody typing `orders` means the statement while somebody
    /// typing `bob` means the user. The state column is the one that makes this
    /// worth having: `idle in transaction` is what the sheet is usually opened
    /// to find.
    private static func checkTheFilterReadsEveryColumnAndIgnoresCase() {
        let model = makeModel()
        fill(model, with: rows)

        expect(model.visibleProcesses.count, 3, "no filter, every row")
        expect(listed(model, filter: "BOB"), ["77"], "the user column, typed in the wrong case")
        expect(listed(model, filter: "sales"), ["9"], "the database column")
        expect(
            listed(model, filter: "idle in"), ["77"], "the state column, which is why it is here")
        expect(listed(model, filter: "orders"), ["42"], "and the statement")
        expect(listed(model, filter: "42"), ["42"], "the id, which is what a log names")
        expect(listed(model, filter: "nobody"), [], "and a word in none of them finds none of them")
    }

    // MARK: - What each server is asked

    /// The menu item is offered where the connection can answer, and nowhere
    /// else.
    ///
    /// A sheet that opened on SQLite could only say "this driver does not report
    /// processes", which is a thing to learn from a greyed-out item rather than
    /// from a panel that had to be dismissed.
    private static func checkTheMenuItemIsOfferedOnlyWhereTheServerAnswers() {
        let model = makeModel()
        for (reach, offered) in [
            (ServerProcesses.unreported, false), (.readOnly, true), (.closable, true),
            (.interruptible, true)
        ] {
            model.sessions[0].capabilities = capabilities(reach)
            expect(model.watchesServerProcesses, offered, "\(reach.rawValue) offers the item")
        }

        // And the item stays shut on a connection that cannot answer, however
        // it is reached: `openProcesses` is what the menu sends to.
        model.sessions[0].capabilities = capabilities(.unreported)
        model.openProcesses()
        expect(model.isProcessesOpen, false, "and opening it directly is refused too")
    }

    /// Each rung offers the kills its server will actually perform.
    ///
    /// The two buttons are not interchangeable and the difference is somebody's
    /// open transaction. A server that can only close sessions must not draw
    /// Cancel Statement — the button would either do nothing or quietly do the
    /// larger thing, and both are worse than its absence.
    private static func checkEachServerIsOfferedOnlyTheKillsItWillPerform() {
        expect(
            [ServerProcesses.unreported, .readOnly, .closable, .interruptible]
                .map { $0.cancelsStatements },
            [false, false, false, true],
            "only a server that interrupts is offered Cancel Statement")
        expect(
            [ServerProcesses.unreported, .readOnly, .closable, .interruptible]
                .map { $0.closesSessions },
            [false, false, true, true],
            "and closing a session is offered by both rungs above read-only — a server that "
                + "cancels statements can always also close the connection")
    }

    // MARK: - The question

    /// Nothing reaches the server until the question has been answered.
    ///
    /// Answered on a real connection, because the guard being checked sits after
    /// the one that drops the work when there is nothing to send it to: without
    /// a database open, a kill that skipped the question would look the same as
    /// one that asked.
    private static func checkNothingIsSentUntilTheQuestionIsAnswered() {
        guard let model = connectedModel(named: "processes-question") else { return }
        fill(model, with: rows)
        model.selectedProcess = "42"

        var asked: [EndProcess] = []
        model.confirmKill = { confirmation in
            asked.append(confirmation.how)
            return false
        }
        model.endChosenProcess(.statement)
        expect(asked, [.statement], "the question is put, and names the kill that was pressed")
        expect(
            model.isReadingProcesses, false,
            "and answering no sends nothing — the sheet does not even go busy")

        model.confirmKill = { _ in true }
        model.endChosenProcess(.session)
        expect(
            model.isReadingProcesses, true,
            "answering yes is what starts it")

        // With nothing selected there is nothing to ask about, and the question
        // is not put at all: an alert naming no session would be one nobody
        // could answer honestly.
        let quiet = makeModel()
        fill(quiet, with: rows)
        quiet.selectedProcess = nil
        var putToNobody = 0
        quiet.confirmKill = { _ in
            putToNobody += 1
            return true
        }
        quiet.endChosenProcess(.statement)
        expect(putToNobody, 0, "with no row chosen, nothing is asked and nothing is sent")
    }

    /// The question names whose session it is and which of the two kills it is.
    ///
    /// The sentence is the only warning there is — a selected row looks the same
    /// under either button — so it has to say what will happen to the
    /// connection, which is the whole difference between them.
    private static func checkTheQuestionNamesTheSessionAndWhichKillItIs() {
        let busy = rows[0]
        let cancelling = AppModel.KillConfirmation(process: busy, how: .statement)
        let closing = AppModel.KillConfirmation(process: busy, how: .session)

        expect(
            cancelling.question, "Cancel the statement running as alice on shop?",
            "the gentler one names the user and the database")
        expect(
            closing.question, "Close the connection belonging to alice on shop?",
            "and the other says it is the connection going, not the statement")
        expect(
            cancelling.detail.contains("nothing is rolled back"), true,
            "the statement kill says the transaction survives")
        expect(
            closing.detail.contains("rolled back"), true,
            "and the session kill says it does not")
        expect(
            closing.detail.contains("SELECT * FROM orders"), true,
            "both show the statement, which is what identifies the session to somebody who did "
                + "not open it")

        // A background worker belongs to nobody and is on no database. The
        // sentence still has to name something, and the id is what is left.
        let anonymous = ServerProcess(
            id: "5", user: "", database: "", state: "active", duration: "00:00:01", statement: "")
        expect(
            AppModel.KillConfirmation(process: anonymous, how: .session).question,
            "Close the connection belonging to process 5?",
            "a process with no user is named by its id rather than by a blank")
    }

    /// Shutting the sheet stops the polling.
    ///
    /// A window that went on asking a struggling server every five seconds after
    /// the sheet was dismissed would be doing it where nobody could see it or
    /// turn it off.
    private static func checkShuttingTheSheetStopsThePolling() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(.interruptible)
        model.openProcesses()
        model.processRefresh = 5
        expect(model.isProcessesOpen, true, "the sheet is up")

        model.closeProcesses()
        expect(model.isProcessesOpen, false, "and shut again")
        expect(model.processRefresh, nil, "with the timer off rather than left running")
    }

    // MARK: - Harness

    /// Three processes, holding what every case above reads: two users, two
    /// databases, an idle-in-transaction state and a statement naming a table.
    private static let rows = [
        ServerProcess(
            id: "42", user: "alice", database: "shop", state: "active", duration: "00:00:12",
            statement: "SELECT * FROM orders WHERE id = 1"),
        ServerProcess(
            id: "77", user: "bob", database: "shop", state: "idle in transaction",
            duration: "00:04:03", statement: ""),
        ServerProcess(
            id: "9", user: "reports", database: "sales", state: "active", duration: "01:12:00",
            statement: "REFRESH MATERIALIZED VIEW daily")
    ]

    private static func listed(_ model: AppModel, filter: String) -> [String] {
        model.processFilter = filter
        return model.visibleProcesses.map(\.id)
    }

    private static func fill(_ model: AppModel, with processes: [ServerProcess]) {
        model.sessions[0].capabilities = capabilities(.interruptible)
        model.sessions[0].processes = processes
    }

    private static func capabilities(_ reach: ServerProcesses) -> Capabilities {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: true, editsRows: true, schemaIsTheDatabase: false,
            reportsRoutines: false,
            reportsSequences: false, serverProcesses: reach, reportsVariables: false,
            changesRelations: false, changesColumns: false, altersColumns: false,
            changesIndexes: false, indexMethods: [], changesConstraints: false,
            changesDatabases: false)
    }

    /// A model with a real connection under it, opened on a scratch SQLite file
    /// the way `KeepAliveChecks` opens one. The driver refuses every call this
    /// sheet makes, which is fine: what is being checked is what happens on this
    /// side before anything is sent.
    private static func connectedModel(named name: String) -> AppModel? {
        let model = makeModel()
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-\(name)-\(UUID().uuidString).db")
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("processes FAIL: a SQLite file would not open\n", stderr)
            return nil
        }
        model.sessions[0].db = db
        return model
    }

    /// A model with no connection, built the way `BrowseRestoreChecks` builds
    /// its own: a throwaway defaults suite, so that running the checks cannot
    /// read or write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-processes"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-processes"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("processes FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
