import Foundation

/// Executable checks for the Connection Privileges sheet, run by
/// `--verify-login-info`.
///
/// A small surface — the sheet reads six rows and has nothing to press — so the
/// rules worth stating are not about what is drawn but about what is kept and
/// who is asked.
///
/// Two of them carry the design decisions this feature is made of. The sheet is
/// offered on every open connection because there is no capability that could
/// say in advance whether the answer is empty, so the check that matters is the
/// one that fails if somebody adds one. And what it read is dropped when it
/// closes, because a privilege is revoked by an administrator with no word to
/// this window — a check that only opened and closed the sheet would pass
/// against a version that cached the answer forever.
///
/// Behind a flag on the binary for the reason `VariablesChecks` gives.
@MainActor
enum LoginInfoChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkEveryOpenConnectionIsOfferedTheSheetWhateverElseItCannotDo()
        checkAnEngineWithNoLoginAnswersEmptyRatherThanFailing()
        checkClosingTheSheetDropsWhatItRead()
        checkClosingTheSheetLeavesNothingThatWouldRefuseTheNextRead()
        checkAFailedReadAnywhereClearsTheFlagToo()
        checkTheFieldsAreReadByTheNamesTheCoreWrites()
        checkTheRowsKeepTheOrderTheDriverSentThem()
        if failures == 0 {
            fputs("login-info: all checks passed\n", stderr)
        } else {
            fputs("login-info: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Who is offered the sheet

    /// Every open connection is offered it, and a tab with nothing open is not.
    ///
    /// The capabilities are set to the poorest answer a driver can give — a
    /// connection that cannot write a statement, cannot edit a row, reports no
    /// routines, no sequences, no processes and no settings — and the item is
    /// still live. That is the whole of the "no capability flag" decision stated
    /// as something that can fail: a flag added here later, of any kind, greys
    /// the item on exactly this connection, and this check goes red.
    ///
    /// `isBusy` is the one thing that does grey it, and not for honesty's sake:
    /// the core queue is serial, so a read started behind a running statement
    /// would land whenever that statement did.
    private static func checkEveryOpenConnectionIsOfferedTheSheetWhateverElseItCannotDo() {
        let model = makeModel()
        expect(model.canReadLoginInfo, false, "a tab with nothing open has no connection to ask")
        model.openLoginInfo()
        expect(model.isLoginInfoOpen, false, "and asking it directly opens nothing")

        guard let db = opened() else { return }
        model.sessions[0].db = db
        model.sessions[0].capabilities = poorest()
        expect(
            model.canReadLoginInfo, true,
            "the poorest connection a driver can report is still one that can be asked who it is")

        model.sessions[0].isBusy = true
        expect(
            model.canReadLoginInfo, false,
            "a statement already running would swallow the read, so the item goes grey")
        model.sessions[0].isBusy = false

        model.openLoginInfo()
        expect(model.isLoginInfoOpen, true, "and with the connection idle the sheet opens")
        model.closeLoginInfo()
    }

    // MARK: - What an engine with no users says

    /// SQLite answers with no fields, and that is an answer rather than an error.
    ///
    /// Through a real connection and the real FFI call, because this is the one
    /// rule the whole design rests on and the only place it can actually be
    /// wrong: `login_info` defaults to an empty vector in the core, and if that
    /// crossed as a failure instead, every one of the fourteen drivers that has
    /// not been taught to look would put an error banner in front of somebody
    /// who opened a menu item — which is precisely the reason given for not
    /// putting a capability flag in front of it.
    private static func checkAnEngineWithNoLoginAnswersEmptyRatherThanFailing() {
        guard let db = opened() else { return }
        guard let fields = try? db.loginInfo() else {
            failures += 1
            fputs("login-info FAIL: a file with no user in it refused the question\n", stderr)
            return
        }
        expect(fields.isEmpty, true, "nothing signs in to a file, so there is nothing to report")
    }

    // MARK: - What is kept

    /// Closing the sheet drops what it read.
    ///
    /// Where this parts company with the settings sheet, which keeps its list.
    /// A setting is what it is until somebody changes it; a privilege is taken
    /// away by an administrator without a word to this window, and the minute
    /// somebody opens this sheet is the minute after a statement was refused.
    /// A kept copy would answer that with the rights they held before the
    /// refusal, offered as the explanation of it.
    private static func checkClosingTheSheetDropsWhatItRead() {
        let model = makeModel()
        model.sessions[0].loginInfo = sent
        model.sessions[0].isLoginInfoOpen = true
        expect(model.loginInfo.count, 3, "three rows on screen")

        model.closeLoginInfo()
        expect(model.isLoginInfoOpen, false, "the sheet is down")
        expect(
            model.loginInfo.isEmpty, true,
            "and it took the answer with it, rather than leaving last week's rights to be read "
                + "as this minute's")
    }

    /// And leaves no flag saying a read is still in flight.
    ///
    /// `loadLoginInfo` drops a request while one is outstanding, so a sheet
    /// dismissed mid-read has to clear that on the way out. Otherwise the
    /// connection is left permanently unable to answer, and the only symptom is
    /// a sheet that opens empty and stays empty — which reads exactly like an
    /// engine that has no login to report.
    private static func checkClosingTheSheetLeavesNothingThatWouldRefuseTheNextRead() {
        let model = makeModel()
        model.sessions[0].isLoginInfoOpen = true
        model.sessions[0].isReadingLoginInfo = true

        model.closeLoginInfo()
        expect(
            model.isReadingLoginInfo, false,
            "a sheet shut mid-read leaves nothing that would refuse every later one")
    }

    /// And a read that fails anywhere clears it too.
    ///
    /// The check above covers the sheet being dismissed; this covers the other
    /// way the flag is left standing. `fail` is where every failed call in this
    /// model lands, and it clears the in-flight flags for exactly one reason:
    /// the read that failed is the one the flag stood for, and a sheet still
    /// thinking a read is outstanding refuses every later one — including the
    /// Refresh pressed to find out what went wrong.
    ///
    /// Made to fail by claiming a capability SQLite does not have. The
    /// capability is the front end's belief; the core refuses `processes()`
    /// regardless, which is a real failure arriving through the real path
    /// rather than an error handed to the model directly. Which call failed
    /// does not matter — that is the point of there being one `fail`.
    private static func checkAFailedReadAnywhereClearsTheFlagToo() {
        let model = makeModel()
        guard let db = opened() else { return }
        model.sessions[0].db = db
        model.sessions[0].capabilities = poorest(serverProcesses: .interruptible)
        model.sessions[0].isReadingLoginInfo = true

        model.loadProcesses()
        expect(
            settle { !model.isReadingLoginInfo }, true,
            "a failed read leaves nothing saying this sheet is mid-read, or it could never be "
                + "read again on this connection")
    }

    // MARK: - Reading the core's answer

    /// The two names the core writes, and nothing else.
    ///
    /// The same rule `MetadataChecks` states for a renamed capability key. It is
    /// worth restating on this call because the failure is silent in the worst
    /// way: a defaulting decode would turn a core that started spelling these
    /// differently into a sheet that draws no rows, which this build already
    /// treats as a true and ordinary answer.
    private static func checkTheFieldsAreReadByTheNamesTheCoreWrites() {
        let field: InfoField? = decode(#"{"label":"Connected as","value":"app_readonly"}"#)
        expect(field?.label, "Connected as", "the word the driver chose")
        expect(field?.value, "app_readonly", "and what it says")
        expect(field?.id, "Connected as", "the label is the identity, so a sheet can list them")

        let renamed: InfoField? = decode(#"{"name":"Connected as","value":"app_readonly"}"#)
        expect(renamed == nil, true, "a field under another name is refused rather than dropped")

        let none: [InfoField]? = decode("[]")
        expect(none?.isEmpty, true, "and an empty array decodes, because it is the usual answer")
    }

    /// The rows are drawn in the order they arrived.
    ///
    /// The driver decides it — identity before privilege, which is the order the
    /// question is asked in — and this side knows nothing about what the rows
    /// mean, so it has nothing to sort them by that would not be worse. Sorted
    /// by label, this fixture would put "Connected as" under "Attributes" and
    /// answer a question nobody asked first.
    private static func checkTheRowsKeepTheOrderTheDriverSentThem() {
        let model = makeModel()
        model.sessions[0].loginInfo = sent
        expect(
            model.loginInfo.map(\.label), ["Connected as", "Role attributes", "Member of"],
            "the driver's order, which is not alphabetical and is not the reverse either")
    }

    // MARK: - Harness

    /// Three fields in an order no sort produces, which is what makes the order
    /// check able to fail.
    private static let sent = [
        InfoField(label: "Connected as", value: "app_readonly"),
        InfoField(label: "Role attributes", value: "none"),
        InfoField(label: "Member of", value: "analysts, reporting")
    ]

    /// The least a driver can claim: it can read, and nothing else.
    private static func poorest(serverProcesses: ServerProcesses = .unreported) -> Capabilities {
        Capabilities(
            transactional: false, cancelStopsTheStatement: false, switchesDatabase: false,
            writesStatements: false, editsRows: false, schemaIsTheDatabase: false,
            reportsRoutines: false, reportsSequences: false, serverProcesses: serverProcesses,
            reportsVariables: false, changesRelations: false, changesColumns: false,
            altersColumns: false, changesIndexes: false, indexMethods: [],
            changesConstraints: false, changesDatabases: false)
    }

    /// Runs the main runloop until `done` answers true, or half a second passes.
    ///
    /// The one thing here that cannot be written as a pure function. `fail` is
    /// reached only from the catch inside the model's own dispatch, so the check
    /// below has to let a real failure actually happen. Bounded rather than
    /// open-ended, so that a check which stops working reports a failure instead
    /// of hanging the suite — half a second is two orders of magnitude more than
    /// a refused SQLite call takes.
    private static func settle(until done: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(0.5)
        while !done(), Date() < deadline {
            RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01))
        }
        return done()
    }

    /// Held for the life of the process, and the files with them.
    ///
    /// Not released per check: `openLoginInfo` starts a read on the model's own
    /// queue and does not wait for it, so a handle freed while that call is
    /// inside the core would be a crash on the way out. See `SchemaDiffChecks`,
    /// which keeps its own for the same reason.
    private static var held: [Database] = []
    private static var scratch: [URL] = []

    /// An empty scratch SQLite file, opened.
    private static func opened() -> Database? {
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-login-\(UUID().uuidString).db")
        scratch.append(file)
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("login-info FAIL: a SQLite file would not open\n", stderr)
            return nil
        }
        held.append(db)
        return db
    }

    /// A model with no connection, on a throwaway defaults suite, for the reason
    /// `VariablesChecks.makeModel` gives.
    private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-login-info"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-login-info"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func decode<T: Decodable>(_ json: String) -> T? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("login-info FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
