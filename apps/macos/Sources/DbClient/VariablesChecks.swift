import AppKit

/// Executable checks for the Server Variables sheet, run by `--verify-variables`.
///
/// Five rules over a much smaller surface than `ProcessesChecks` covers, because
/// nothing here acts on a row: the whole sheet reads, and the ways it can be
/// wrong are ways of showing the wrong rows or the wrong words.
///
/// The one worth the file is the filter. Six hundred settings are unusable
/// without it, so it is the only way anybody reaches a row — which makes what it
/// matches the difference between finding `wal_level` and concluding the server
/// does not have it.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum VariablesChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheFilterReadsNameAndValueButNotTheScopeColumn()
        checkTheMenuItemIsOfferedOnlyWhereTheServerAnswers()
        checkCopyTakesWhatIsShowingRatherThanEverythingRead()
        checkAScopeTheCoreDoesNotWriteIsRefusedRatherThanGuessed()
        checkShuttingTheSheetLetsTheNextOneRead()
        if failures == 0 {
            fputs("variables: all checks passed\n", stderr)
        } else {
            fputs("variables: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Finding a setting

    /// The filter matches the name and the value, in any case, and not the
    /// scope.
    ///
    /// The value is in because half of what anybody asks this list is which
    /// settings mention a directory, a size or `off` — `shared_preload_libraries`
    /// is found by typing the extension somebody is looking for, not by typing
    /// the setting they have not thought of yet.
    ///
    /// The scope is deliberately out, and that is the half of this rule that
    /// would otherwise rot quietly. Both words are common English: folding the
    /// column into what is matched would make typing `server` return every
    /// server-scoped row — several hundred of them — as though they all
    /// mentioned the word, and the person who typed it was looking for
    /// `server_version`.
    private static func checkTheFilterReadsNameAndValueButNotTheScopeColumn() {
        let model = makeModel()
        fill(model, with: rows)

        expect(model.visibleVariables.count, 6, "no filter, every row")
        expect(listed(model, filter: "WAL_"), ["wal_level"], "the name, typed in the wrong case")
        expect(
            listed(model, filter: "replica"), ["wal_level"],
            "and the value, which is how a setting is found by what it is set to")
        expect(
            listed(model, filter: "pg_stat_statements"), ["shared_preload_libraries"],
            "including one item of a list-valued setting")
        expect(
            listed(model, filter: "session"), ["idle_session_timeout"],
            "the scope column is not matched — `session` finds the setting named after one, and "
                + "not `application_name`, which is the row the column calls a session's")
        expect(
            listed(model, filter: "server"), ["server_version"],
            "and `server` likewise finds a name, not the five rows scoped to the server")
        expect(listed(model, filter: "nothing"), [], "a word in none of them finds none of them")
    }

    // MARK: - What each connection is asked

    /// The menu item is offered where the connection can answer, and nowhere
    /// else.
    ///
    /// Its own capability rather than sharing the processes one, and this is
    /// what says so: the two are separate questions, and a driver taught to read
    /// `SHOW VARIABLES` and not `SHOW PROCESSLIST` must get one item and not the
    /// other.
    private static func checkTheMenuItemIsOfferedOnlyWhereTheServerAnswers() {
        let model = makeModel()

        model.sessions[0].capabilities = capabilities(reportsVariables: false)
        expect(model.readsServerVariables, false, "a connection that cannot answer offers nothing")
        model.openVariables()
        expect(model.isVariablesOpen, false, "and opening it directly is refused too")

        model.sessions[0].capabilities = capabilities(reportsVariables: true)
        expect(model.readsServerVariables, true, "one that can, offers the item")

        // The two capabilities move independently, which is the point of there
        // being two.
        model.sessions[0].capabilities = capabilities(
            reportsVariables: true, serverProcesses: .unreported)
        expect(
            [model.readsServerVariables, model.watchesServerProcesses], [true, false],
            "settings without processes is a state a driver can be in")
        model.sessions[0].capabilities = capabilities(
            reportsVariables: false, serverProcesses: .interruptible)
        expect(
            [model.readsServerVariables, model.watchesServerProcesses], [false, true],
            "and so is the reverse")
    }

    // MARK: - Copying

    /// Copy takes the rows on screen, not every row read.
    ///
    /// Which is the whole reason the button is worth having: somebody narrows
    /// six hundred settings to the eight that matter and copies those into a
    /// ticket. Copying all six hundred is one keystroke away — clear the filter —
    /// and is almost never what was meant by pressing Copy while a filter is up.
    ///
    /// Tab-separated rather than `name = value`, because a value can contain
    /// spaces, commas and equals signs — `log_line_prefix` is `%m [%p] ` on a
    /// default install — and a tab is the one character none of them hold.
    private static func checkCopyTakesWhatIsShowingRatherThanEverythingRead() {
        let model = makeModel()
        fill(model, with: rows)

        model.variableFilter = "wal_"
        expect(
            model.copiedVariables, "wal_level\treplica",
            "the filtered row and only it, name and value with a tab between them")

        model.variableFilter = ""
        expect(
            model.copiedVariables.split(separator: "\n").count, 6,
            "and with no filter, one line per setting")
        expect(
            model.copiedVariables.contains("log_line_prefix\t%m [%p] "), true,
            "a value with spaces and brackets in it survives, which is why the separator is a tab")

        model.variableFilter = "no such setting"
        expect(model.copiedVariables, "", "and nothing showing is nothing to copy")
    }

    // MARK: - Reading the core's answer

    /// A scope word the core does not write stops the decode.
    ///
    /// The same rule `MetadataChecks` states for a renamed capability key, and it
    /// matters more here because the failure is plausible rather than absurd: a
    /// core that started saying `global` instead of `server` would, with a
    /// defaulting decode, leave every row of the sheet labelled `Session` — which
    /// reads as a true and alarming fact about a server, rather than as the
    /// version mismatch it is.
    private static func checkAScopeTheCoreDoesNotWriteIsRefusedRatherThanGuessed() {
        let session: ServerVariable? = decode(
            #"{"name":"application_name","value":"DbClient","scope":"session"}"#)
        expect(session?.scope, .session, "the word the core writes for this connection's own")
        expect(session?.id, "application_name", "and the name is the identity")

        let server: ServerVariable? = decode(
            #"{"name":"wal_level","value":"replica","scope":"server"}"#)
        expect(server?.scope, .server, "and the word it writes for everybody's")
        expect(server?.scope.label, "Server", "which the sheet draws capitalised")

        let stranger: ServerVariable? = decode(
            #"{"name":"wal_level","value":"replica","scope":"global"}"#)
        expect(stranger == nil, true, "a third word is refused rather than read as one of the two")
    }

    // MARK: - Opening and shutting

    /// Shutting the sheet leaves nothing behind that would refuse the next read.
    ///
    /// `loadVariables` drops a request while one is in flight, so a sheet
    /// dismissed mid-read has to clear the flag on the way out. Otherwise the
    /// connection is left permanently unable to answer this question, and the
    /// only symptom is a sheet that opens empty and stays empty.
    private static func checkShuttingTheSheetLetsTheNextOneRead() {
        let model = makeModel()
        model.sessions[0].capabilities = capabilities(reportsVariables: true)
        model.openVariables()
        expect(model.isVariablesOpen, true, "the sheet is up")

        // As though a read were still outstanding when it was dismissed.
        model.sessions[0].isReadingVariables = true
        model.closeVariables()
        expect(model.isVariablesOpen, false, "and shut again")
        expect(
            model.isReadingVariables, false,
            "with nothing left saying a read is in flight, which would refuse every later one")
    }

    // MARK: - Harness

    /// Six settings, in the name order the core promises, holding what every
    /// case above reads.
    ///
    /// The two that carry the filter's rule are the first and the second: the
    /// only row scoped `session` is one whose text says nothing about sessions,
    /// and the only row whose text says `session` is scoped `server`. A filter
    /// that read the scope column would return the wrong one of them, and one
    /// that read only names would still look right.
    private static let rows = [
        ServerVariable(name: "application_name", value: "DbClient", scope: .session),
        ServerVariable(name: "idle_session_timeout", value: "0", scope: .server),
        ServerVariable(name: "log_line_prefix", value: "%m [%p] ", scope: .server),
        ServerVariable(name: "server_version", value: "17.2", scope: .server),
        ServerVariable(
            name: "shared_preload_libraries", value: "pg_stat_statements", scope: .server),
        ServerVariable(name: "wal_level", value: "replica", scope: .server)
    ]

    private static func listed(_ model: AppModel, filter: String) -> [String] {
        model.variableFilter = filter
        return model.visibleVariables.map(\.name)
    }

    private static func fill(_ model: AppModel, with variables: [ServerVariable]) {
        model.sessions[0].capabilities = capabilities(reportsVariables: true)
        model.sessions[0].variables = variables
    }

    private static func capabilities(
        reportsVariables: Bool, serverProcesses: ServerProcesses = .unreported
    ) -> Capabilities {
        Capabilities(
            transactional: true, cancelStopsTheStatement: true, switchesDatabase: false,
            writesStatements: true, schemaIsTheDatabase: false, reportsRoutines: false,
            reportsSequences: false, serverProcesses: serverProcesses,
            reportsVariables: reportsVariables)
    }

    /// A model with no connection, built the way `ProcessesChecks` builds its
    /// own: a throwaway defaults suite, so that running the checks cannot read or
    /// write the history the user's windows share.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-variables"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-variables"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func decode<T: Decodable>(_ json: String) -> T? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("variables FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
