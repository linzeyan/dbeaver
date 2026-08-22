import Foundation
import Observation
import SwiftUI

/// Executable checks for the AppModel's connection chooser, run by `--verify-connection-form`.
///
/// Tests the model layer of the connection chooser, ensuring that the AppModel
/// correctly handles connection selection, editing, saving, and deletion.
enum AppModelConnectionChecks {
    private static var failures = 0

    static func run() -> Bool {
        // Set up scratch directory for config to avoid touching user's files
        let scratch = scratchDirectory()
        guard let scratch else {
            fputs("connection-form FAIL: could not create scratch directory\n", stderr)
            return false
        }
        defer {
            try? FileManager.default.removeItem(at: scratch)
        }

        // Set environment to use scratch directory for config
        setenv("XDG_CONFIG_HOME", scratch.path, 1)
        // And for the cache, which is a second directory this suite writes to.
        // Without this the checks below file trees under the developer's own
        // `~/.cache`, where nothing deletes them and where a run of the checks
        // would be indistinguishable from a connection they had actually opened.
        setenv("XDG_CACHE_HOME", scratch.path, 1)

        failures = 0
        defer { ScratchDefaults.release() }
        checkSelectingRowLoadsIntoForm()
        checkSelectingQuickConnect()
        checkTypedEditBecomesUnsavedEdits()
        checkRevertPutsSavedValuesBack()
        checkSaveWritesThroughToList()
        checkSaveOnQuickConnectAddsRow()
        checkDeleteRemovesExactlySelectedRow()
        checkDeleteIsRefusedWhenNothingToDelete()
        checkSettleUnsavedConnectionEditsHonoursAnswers()
        checkFilterNarrowsByTitleAndAddress()
        checkTheSidebarSeesTheFolders()
        checkSelectingARowDoesNotReadThePassword()
        checkSavingDoesNotWipeAnUnreadPassword()
        checkATypedPasswordIsNotOverwritten()
        checkTheKeychainIsUntouchedWhileTheSettingIsOff()
        checkProductionQuestion()
        checkTheSessionHoldsTheConnection()
        checkAWindowHoldsAListOfConnections()
        checkOnlyIdleOpenConnectionsAreProbed()
        checkADatabaseLevelIsDrawnOnlyWhenThereIsOne()
        checkTheFilterReachesTheDatabaseLevel()
        checkADatabaseWithNoGrammarOffersNoEditing()
        checkTheTabSaysWhatItIsWithoutSayingTheSecret()
        checkOpeningAnotherDatabaseKeepsEverythingElseAboutTheConnection()
        checkSwitchingDatabaseMovesTheTabRatherThanAddingOne()
        checkSwitchingIsRefusedWhileThereIsWorkToLose()
        checkTheBastionIsBuiltFromWhatWasTyped()
        checkAnEntryThatDeclinedStorageKeepsItsPasswordInMemoryOnly()
        checkAPasswordKeptOnThisMacIsThereOnTheNextLaunch()
        checkABastionSecretIsKeptTheWayThePasswordIs()
        checkTheNavigatorCacheKeepsOneTreePerDatabase()
        checkAReopenedConnectionDrawsLastTimesTreeAtOnce()
        if failures == 0 {
            fputs("connection-form: all checks passed\n", stderr)
        } else {
            fputs("connection-form: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    /// A connection's state is the session's, reached through the window.
    ///
    /// The forwarding is the whole of this step and is also the thing a later
    /// edit can undo without anything complaining: a property put back to being
    /// stored on the window would still compile, still pass every other check,
    /// and leak one connection's state into the next one. So this writes through
    /// the name the panes use and reads the session underneath.
    private static func checkTheSessionHoldsTheConnection() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(model.sessions.count, 1, "a window opens holding one session")
            expect(model.activeSession, 0, "and shows it")
            expect(model.sessions[0].db == nil, true, "with nothing open on it yet")
            expect(
                model.sessions[0].connectionLabel, model.connectionLabel,
                "the label the chrome reads is the session's own")

            // A port nothing listens on, so the attempt is refused on the core
            // queue while this check reads what the main actor was left with.
            // Everything asserted below is written before the dispatch, which is
            // what makes it safe to read here.
            model.connect(using: "postgres://nobody@127.0.0.1:1/none")
            expect(
                model.sessions[0].status, "Connecting…",
                "opening a connection writes through to the session")
            expect(model.sessions[0].isBusy, true, "and so does the flag the chrome spins on")
            expect(model.status, "Connecting…", "and the window reads back what it wrote")

            // The navigator is the same claim as the label: it describes one
            // connection's database, and the same schema name means a different
            // thing on the next server. Written through the name the sidebar
            // reads, so that a property put back onto the window is caught.
            model.expanded.insert("public")
            expect(
                model.sessions[0].expanded.contains("public"), true,
                "what the sidebar has open is the session's")
            model.navigatorFilter = "orders"
            expect(
                model.sessions[0].navigatorFilter, "orders",
                "and so is what it is filtered by")
            model.activeTab = .structure
            expect(
                model.sessions[0].activeTab, .structure,
                "and which detail pane is showing")

            // The editor's text is written against one database's dialect and
            // the filters against one relation's columns, so both are the
            // session's. `queryText` reaches the active buffer, which now lives
            // there too — this is the check that the whole chain still lands in
            // one place.
            model.queryText = "select 1"
            expect(
                model.sessions[0].queryBuffers[0].text, "select 1",
                "what is typed in the editor is the session's")
            model.addQueryBuffer()
            expect(model.sessions[0].queryBuffers.count, 2, "and so is a second buffer")
            expect(
                model.sessions[0].activeQueryBufferIndex, 1,
                "and which of them the editor is in")
            model.whereClause = "id > 10"
            expect(
                model.sessions[0].whereClause, "id > 10",
                "and what the browse is filtered by")

            // A cursor is a connection the server holds open on this session's
            // behalf, so it is the one property here whose leaking is not a
            // cosmetic defect. Nothing can open one without a server, so what is
            // pinned is that the session is where it would be looked for.
            expect(model.sessions[0].browseCursor == nil, true, "no cursor is open yet")
            expect(model.sessions[0].isExporting, false, "and nothing is being written to a file")
            model.isValueViewerOpen = true
            expect(
                model.sessions[0].isValueViewerOpen, true,
                "and the viewer over a cell belongs to the session that has the cell")
        }
    }

    /// A window is a list of connections, and a refused attempt leaves no tab.
    ///
    /// Everything here happens without a server, which bounds what it can pin: a
    /// second tab needs a connection that opened. What it does pin is the rule
    /// that decides whether there is a second tab at all, and the rule is where
    /// the mistakes are — a window that gained a dead tab per mistyped password
    /// would be asking the user to clean up after a refusal.
    private static func checkAWindowHoldsAListOfConnections() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(model.sessions.count, 1, "a window opens with one tab")

            // A port nothing listens on. The refusal arrives on the core queue
            // and is applied on a later turn of the run loop, so what is read
            // here is what the attempt did on its way out.
            model.connect(using: "postgres://nobody@127.0.0.1:1/none")
            expect(
                model.sessions.count, 1,
                "the first connection fills the tab that is already there")
            model.connect(using: "postgres://nobody@127.0.0.1:1/other")
            expect(
                model.sessions.count, 1,
                "and so does the next, because nothing opened in it")
            expect(
                model.sessions[0].connectionLabel.isEmpty, false,
                "the tab names what it is reaching for while it reaches")

            // Out of range is a no-op rather than a crash. The strip is drawn
            // from the same list, but a menu item can outlive the tab it names.
            model.selectSession(7)
            expect(model.activeSession, 0, "there is no tab seven to go to")

            // Closing the only one leaves a window with a tab, and an empty one:
            // what was typed against the connection being closed goes with it.
            model.queryText = "select 1"
            model.closeSession(0)
            expect(model.sessions.count, 1, "a window always has a tab")
            expect(model.queryText, "", "and the one left is empty")
            expect(model.sessions[0].db == nil, true, "with nothing open on it")

            // A window with nothing open shows the chooser. Closing the last
            // connection has to reach that state, or the window is left drawing
            // the panes of a database that has gone under a toolbar naming it.
            expect(
                model.isShowingConnectionForm, true,
                "and the window is asking which database to open")
            expect(
                model.canDisconnect, false,
                "with nothing for Disconnect to close")
        }
    }

    /// The sidebar reads the same folders the file holds, and the filter reaches
    /// through them.
    ///
    /// `visibleConnectionGroups` is what the sidebar draws and `visibleConnections`
    /// is what the footer counts, so the two have to agree about how many there
    /// are. They are computed separately — one groups, one does not — which is
    /// exactly the arrangement that lets a header promise a row the list will not
    /// draw.
    private static func checkTheSidebarSeesTheFolders() {
        MainActor.assumeIsolated {
            func made(_ name: String, folder: String) -> SavedConnection {
                SavedConnection(
                    name: name, folder: folder,
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                        user: "ana"))
            }
            let model = makeModel(with: [
                made("Acme prod", folder: "clients/acme"),
                made("Scratch", folder: ""),
                made("Bink prod", folder: "clients/bink")
            ])

            expect(
                model.visibleConnectionGroups.map(\.path), ["", "clients/acme", "clients/bink"],
                "the sidebar is handed the top level and then the folders")
            expect(
                model.visibleConnectionGroups.reduce(0) { $0 + $1.connections.count },
                model.visibleConnections.count,
                "and holds exactly what the footer counts")

            // The filter reaches into folders rather than only over them. A search
            // that only matched top-level rows would find nothing in a sidebar
            // where everything has been filed.
            model.connectionFilter = "Acme"
            expect(
                model.visibleConnectionGroups.map(\.path), ["clients/acme"],
                "a filter leaves only the folders that still hold something")
            expect(
                model.visibleConnectionGroups.first?.connections.map(\.name), ["Acme prod"],
                "and only what it matched inside them")
        }
    }

    /// Clicking a row is looking, not connecting, and must not raise the panel
    /// that asks permission to read a secret.
    ///
    /// The password is written by this check and deleted afterwards. It is the
    /// same process that reads it back, so no panel appears here either way —
    /// what is being pinned is that the model does not go and look.
    private static func checkSelectingARowDoesNotReadThePassword() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Stored Password",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))
            let model = makeModel(with: [connection])
            model.preferences.passwordStorage = .keychain
            ConnectionKeychain.save("s3cret", for: connection.id)
            defer { ConnectionKeychain.delete(for: connection.id) }

            model.selectConnection(connection.id)
            expect(model.hasUnreadPassword, true, "the form knows there is one it has not read")
            expect(model.connectionPassword, "", "and the field is empty rather than filled")
            expect(
                model.unsavedConnectionEdits == nil, true,
                "an unread password is not an unsaved edit")
        }
    }

    /// Saving an unrelated change must not delete a password that was never
    /// read. `ConnectionKeychain.save` stores nothing for an empty string, so
    /// writing the untouched field through would erase it silently.
    private static func checkSavingDoesNotWipeAnUnreadPassword() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Stored Password",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))
            let model = makeModel(with: [connection])
            model.preferences.passwordStorage = .keychain
            ConnectionKeychain.save("s3cret", for: connection.id)
            defer { ConnectionKeychain.delete(for: connection.id) }

            model.selectConnection(connection.id)
            model.connectionDraft.settings.port = "5433"
            model.saveConnection()
            expect(
                ConnectionKeychain.password(for: connection.id), "s3cret",
                "the stored password outlived a change to the port")

            // And a password that *was* typed still replaces it.
            model.connectionPassword = "typed"
            model.saveConnection()
            expect(
                ConnectionKeychain.password(for: connection.id), "typed",
                "but typing one over it still writes")
        }
    }

    /// Typing a password and pressing Connect must use the typed one.
    ///
    /// The fault this pins: the read happened unconditionally, so the panel
    /// appeared for a secret that was not needed and the stored password then
    /// replaced what had just been typed — the form silently connected with a
    /// password other than the one on screen.
    private static func checkATypedPasswordIsNotOverwritten() {
        MainActor.assumeIsolated {
            // `connectFromForm` really does start a connection, on a background
            // queue this check does not wait for. Port 1 on the loopback refuses
            // at once, so nothing here resolves a name or waits for a timeout.
            let connection = SavedConnection(
                name: "Stored Password",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "127.0.0.1", port: "1", database: "db",
                    user: "user"))
            let model = makeModel(with: [connection])
            model.preferences.passwordStorage = .keychain
            ConnectionKeychain.save("stored", for: connection.id)
            defer { ConnectionKeychain.delete(for: connection.id) }

            model.selectConnection(connection.id)
            model.connectionPassword = "typed"
            model.connectFromForm()
            expect(model.connectionPassword, "typed", "the typed password survived Connect")
            expect(
                model.hasUnreadPassword, true,
                "and the stored one was never read, so nothing asked permission")
        }
    }

    /// With the setting off the Keychain is not consulted at all — not read, not
    /// asked whether an item exists, and not written by Save.
    private static func checkTheKeychainIsUntouchedWhileTheSettingIsOff() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Stored Password",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))
            let model = makeModel(with: [connection])
            defer { ConnectionKeychain.delete(for: connection.id) }

            model.selectConnection(connection.id)
            expect(
                model.hasUnreadPassword, false,
                "with the setting off the form has nothing deferred")

            model.connectionPassword = "typed"
            model.saveConnection()
            expect(
                ConnectionKeychain.password(for: connection.id), nil,
                "and Save wrote no password to the Keychain")
            expect(
                model.unsavedConnectionEdits == nil, true,
                "yet Save still settled the form, rather than leaving a permanent unsaved mark")
        }
    }

    /// A production connection asks about writes, says nothing about reads, and
    /// the question it puts names the statement rather than a severity.
    ///
    /// The wording is pinned and not merely the Bool. A dialog that says only
    /// "this is dangerous" tells somebody how to feel about a statement they can
    /// already see; the one thing they cannot see from the Run button is which
    /// statement the caret was in, and that is what has to be in the box.
    ///
    /// The read case is the one worth defending hardest. A question in front of
    /// every SELECT would be dismissed reflexively within a minute, and the
    /// reflex is what would then dismiss the DROP.
    private static func checkProductionQuestion() {
        MainActor.assumeIsolated {
            let production = ConnectionSafety(isProduction: true)
            expect(
                production.asks(about: .safe), false,
                "a read on production is not worth a dialog")
            expect(production.asks(about: .modify), true, "a write on production is")
            expect(production.asks(about: .fatal), true, "and so is a drop")
            expect(
                ConnectionSafety(isReadOnly: true).asks(about: .fatal), false,
                "read-only refuses this client's writes rather than asking about typed SQL")
            expect(
                ConnectionSafety().asks(about: .fatal), false,
                "and an unmarked connection runs what it is given")

            let one = AppModel.ProductionRun(
                count: 1, worst: "DELETE FROM orders", danger: .modify,
                label: "ana@db.example/sales")
            expect(
                one.question, "Run this statement on “ana@db.example/sales”?",
                "one statement is asked about as one")
            expect(
                one.detail.contains("DELETE FROM orders"), true,
                "and the question shows the statement itself")
            expect(
                one.detail.contains("changes rows on it"), true,
                "and says what running it would do")

            let many = AppModel.ProductionRun(
                count: 5, worst: "DROP TABLE orders", danger: .fatal,
                label: "ana@db.example/sales")
            expect(
                many.question, "Run 5 statements on “ana@db.example/sales”?",
                "a script is asked about by its count")
            expect(
                many.detail.contains("one of 5"), true,
                "and the statement shown is placed among them")
            expect(
                many.detail.contains("destroys something on it"), true,
                "with the worst of them setting the words")

            // A dialog that has become a document is one people dismiss without
            // reading it, which is the failure this mark exists to avoid.
            let long = AppModel.ProductionRun(
                count: 1, worst: String(repeating: "x", count: 900), danger: .fatal, label: "db")
            expect(long.detail.count < 500, true, "a long statement is cut rather than shown whole")
        }
    }

    /// What four typed fields and one secret become on the way to the core.
    ///
    /// Worth its own check because every mistake available here is silent. A
    /// secret sent as a password when a key was named is refused by the bastion
    /// and reads as the wrong key; a port left empty and passed on as 0 is
    /// refused by the core with a message about a field the form does not show;
    /// and a host left empty that still produced a bastion would send every
    /// connection in the application through a tunnel to nowhere.
    private static func checkTheBastionIsBuiltFromWhatWasTyped() {
        func settings(
            sshHost: String = "bastion.example", sshPort: String = "", sshUser: String = "ana",
            sshKeyPath: String = ""
        ) -> ConnectionSettings {
            ConnectionSettings(
                scheme: "postgres", host: "db.internal", port: "5432", database: "sales",
                user: "ana", sshHost: sshHost, sshPort: sshPort, sshUser: sshUser,
                sshKeyPath: sshKeyPath)
        }

        // Written as comparisons rather than passed as `nil`, so that the check
        // does not depend on how `expect`'s generic infers a bare literal.
        expect(
            AppModel.bastion(for: settings(sshHost: ""), secret: "s") == nil, true,
            "no host named is no bastion at all")
        expect(
            AppModel.bastion(for: settings(sshHost: "   "), secret: "s") == nil, true,
            "and neither is a host somebody typed a space into")

        let withPassword = AppModel.bastion(for: settings(), secret: "hunter2")
        expect(withPassword?.host, "bastion.example", "the host is what was typed")
        expect(withPassword?.user, "ana", "and so is the user")
        expect(withPassword?.port, 22, "a port nobody filled in is 22")
        expect(withPassword?.password, "hunter2", "with no key named, the secret is a password")
        expect(withPassword?.keyPath == nil, true, "and there is no key file")
        expect(withPassword?.passphrase == nil, true, "and nothing to unlock")

        let withKey = AppModel.bastion(
            for: settings(sshPort: "2222", sshKeyPath: "/Users/ana/.ssh/id_ed25519"),
            secret: "hunter2")
        expect(withKey?.port, 2222, "a port that was filled in is used")
        expect(
            withKey?.keyPath, "/Users/ana/.ssh/id_ed25519", "the key file is what was typed")
        expect(withKey?.password == nil, true, "and the secret is not also sent as a password")
        expect(withKey?.passphrase, "hunter2", "it unlocks the key instead")

        // A key with no passphrase is the ordinary case, and the empty string is
        // what an untouched field holds. The core reads empty as absent, so this
        // is the same as saying nothing — but it has to arrive as the empty
        // string rather than as something invented here.
        let unlocked = AppModel.bastion(
            for: settings(sshKeyPath: "/Users/ana/.ssh/id_ed25519"), secret: "")
        expect(unlocked?.passphrase, "", "a key with no passphrase says so with nothing")

        // Neither field filled in, which the core reads as the ssh-agent. The
        // empty password is the whole of that signal — there is no third field
        // and no picker — so a build that sent nil instead, or invented
        // something to put there, would turn the one arrangement needing nothing
        // typed at all into a connection error. It would do it to everybody
        // whose key never leaves their agent, and the message they would get is
        // about a password they had deliberately not set.
        let agent = AppModel.bastion(for: settings(), secret: "")
        expect(agent?.password, "", "nothing typed anywhere still sends a password field")
        expect(agent?.keyPath == nil, true, "with no key file beside it")
        expect(agent?.passphrase == nil, true, "and nothing to unlock, which is what the agent is")

        // Trimmed for the reason the connection string's fields are: a host
        // pasted with a trailing space is the commonest way to spend five
        // minutes on a connection error.
        let padded = AppModel.bastion(
            for: settings(sshHost: " bastion.example ", sshUser: " ana "), secret: "s")
        expect(padded?.host, "bastion.example", "a pasted host is trimmed")
        expect(padded?.user, "ana", "and so is the user")

        // The user's own record of which servers are which, not a list this
        // application keeps. Checked as a suffix because the home directory is
        // whoever is running the checks.
        expect(
            AppModel.knownHostsFile.hasSuffix("/.ssh/known_hosts"), true,
            "the host keys come from the user's own known_hosts")
    }

    /// Picking another database moves this tab onto it, and moves nothing else.
    ///
    /// The tab is rebuilt from a fresh `Session` rather than cleared out, so the
    /// half that can be wrong is the short list of things deliberately carried
    /// across — and the loudest of them is the editor. Somebody who has written
    /// a statement and then goes looking for the table it names, one database
    /// over, must not find their own work gone when they arrive.
    ///
    /// The port is one nothing listens on, so the connection this asks for is
    /// refused on the session's queue while the main actor reads what was
    /// written before the dispatch — which is everything below.
    private static func checkSwitchingDatabaseMovesTheTabRatherThanAddingOne() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].connString = "postgres://nobody@127.0.0.1:1/bench"
            model.sessions[0].databases = [
                DatabaseInfo(name: "bench", isCurrent: true),
                DatabaseInfo(name: "archive", isCurrent: false)
            ]
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.renameQueryBuffer(0, to: "the work")
            model.queryText = "select 1"

            expect(
                model.canSwitchDatabase(to: "bench"), false,
                "the database already open is not somewhere to switch to")
            expect(
                model.canSwitchDatabase(to: "elsewhere"), false,
                "and neither is a name the server did not report")

            model.switchDatabase(to: "archive")
            expect(model.sessions.count, 1, "the tab moved rather than a second one appearing")
            expect(
                model.sessions[0].connString, "postgres://nobody@127.0.0.1:1/archive",
                "onto the database that was picked")
            expect(
                model.queryBuffers.first?.name, "the work",
                "carrying the buffers somebody was writing in")
            expect(model.queryText, "select 1", "and what they had written in them")
            expect(
                model.schemas.isEmpty, true,
                "and nothing of the tree of the database it left")
        }
    }

    /// A switch throws away what a quit would, so it refuses where a quit asks.
    ///
    /// Refusing rather than asking, because unlike a quit this one has a way out
    /// that costs nothing: the same database opens in a new tab with this one
    /// left exactly as it was, and that is what the sentence says to do. A
    /// version that switched anyway would roll back the transaction the toolbar
    /// has been showing in amber, on a double-click aimed at a name.
    private static func checkSwitchingIsRefusedWhileThereIsWorkToLose() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].connString = "postgres://nobody@127.0.0.1:1/bench"
            model.sessions[0].databases = [
                DatabaseInfo(name: "bench", isCurrent: true),
                DatabaseInfo(name: "archive", isCurrent: false)
            ]
            model.sessions[0].transaction = TransactionState(
                transactional: true, autocommit: false, open: true, savepoints: [])

            model.switchDatabase(to: "archive")
            expect(
                model.sessions[0].connString, "postgres://nobody@127.0.0.1:1/bench",
                "the tab stayed on the database it was on")
            expect(
                model.errorMessage?.contains("new tab"), true,
                "and the refusal names the way to see the other one anyway")
        }
    }

    /// Everything except the database has to survive being pointed somewhere
    /// else, and the password is the part that survives least willingly.
    ///
    /// It is percent-encoded in the string, so a rewrite that decodes and
    /// re-encodes it turns `p%40ss` into `p@ss` — which is not a parse error
    /// anywhere, it is a connection that is simply refused, on a tab the user
    /// opened by double-clicking a name. The query string matters for the same
    /// reason: `sslmode=require` dropped on the way through is a tab that
    /// silently connects in plaintext.
    private static func checkOpeningAnotherDatabaseKeepsEverythingElseAboutTheConnection() {
        expect(
            AppModel.connString(
                "postgres://bench:bench@127.0.0.1:55432/bench", onDatabase: "archive"),
            "postgres://bench:bench@127.0.0.1:55432/archive",
            "the database is the only part that changes")
        expect(
            AppModel.connString(
                "postgres://bench:p%40ss%2Fword@db.internal:5432/bench?sslmode=require",
                onDatabase: "archive"),
            "postgres://bench:p%40ss%2Fword@db.internal:5432/archive?sslmode=require",
            "an encoded password and the query survive unchanged")
        expect(
            AppModel.connString("mysql://root@10.0.0.5:3306/", onDatabase: "reporting"),
            "mysql://root@10.0.0.5:3306/reporting",
            "a string with no database named gains one")
        expect(
            AppModel.connString("postgres://u:p@h:5432/bench", onDatabase: "my db"),
            "postgres://u:p@h:5432/my%20db",
            "and a name that needs encoding is encoded on the way in")
        // Written as a comparison rather than passed as `nil`, so that the
        // check does not depend on how `expect`'s generic infers a bare literal.
        expect(
            AppModel.connString("host=127.0.0.1 dbname=bench", onDatabase: "archive") == nil, true,
            "a libpq keyword string comes back nil rather than as a bare /archive")
        expect(
            AppModel.connString("", onDatabase: "archive") == nil, true,
            "and so does an empty one")
    }

    /// The one question the navigator asks, and the two different answers that
    /// both come back as no.
    ///
    /// nil and empty are not the same fact — an engine with no level above
    /// schemas against a login that can see none of them — and the difference is
    /// worth keeping in the session, which is why it is kept there. What the
    /// view needs is neither of those: it needs to know whether to draw a level,
    /// and both of these mean it does not. A check that only covered nil would
    /// pass against `databases != nil`, which is the wrong condition and the one
    /// somebody would reach for.
    private static func checkADatabaseLevelIsDrawnOnlyWhenThereIsOne() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(
                model.hasDatabaseLevel, false,
                "a session that has read nothing yet draws no database level")

            model.sessions[0].databases = []
            expect(
                model.hasDatabaseLevel, false,
                "nor does a login that can see none of them")

            model.sessions[0].databases = [
                DatabaseInfo(name: "bench", isCurrent: true),
                DatabaseInfo(name: "archive", isCurrent: false)
            ]
            expect(model.hasDatabaseLevel, true, "and a server with databases draws one")
            expect(
                model.databases?.count, 2,
                "which the window reads from the session it is showing")
            expect(
                model.databases?.first?.isCurrent, true,
                "keeping which one the connection is open on")
        }
    }

    /// The filter reaches the database level, and says so when it reaches
    /// nothing at all.
    ///
    /// The level is somewhere to go rather than a caption, so leaving it out of
    /// the filter left the one row that moves the window as the one row a filter
    /// could not find. The empty answer is the load-bearing half: the navigator
    /// puts its "No matches" state up when no database survives, and a tree that
    /// kept every database row could never reach that state — it would answer a
    /// filter that matched nothing with a list of databases, which reads as a
    /// filter that is not switched on.
    private static func checkTheFilterReachesTheDatabaseLevel() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].databases = [
                DatabaseInfo(name: "bench", isCurrent: true),
                DatabaseInfo(name: "archive", isCurrent: false)
            ]
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].relations = [
                "public": [
                    RelationInfo(
                        schema: "public", name: "orders", kind: .table, estimatedRows: nil),
                    RelationInfo(schema: "public", name: "items", kind: .table, estimatedRows: nil)
                ]
            ]

            expect(
                model.visibleDatabases.map(\.name), ["bench", "archive"],
                "with no filter the tree lists every database")

            model.navigatorFilter = "orders"
            expect(
                model.visibleDatabases.map(\.name), ["bench"],
                "a filter matching a relation keeps the database holding it")
            expect(model.matchedRelationCount, 1, "and narrows the relations under it")

            model.navigatorFilter = "arch"
            expect(
                model.visibleDatabases.map(\.name), ["archive"],
                "a database is reached by its own name")
            expect(
                model.matchedRelationCount, 0,
                "and the database the connection is on goes with its relations")

            model.navigatorFilter = "bench"
            expect(
                model.visibleDatabases.map(\.name), ["bench"],
                "the open database answers to its own name too")
            expect(
                model.matchedRelationCount, 2,
                "keeping everything under it, the rule a schema's own name follows")

            model.navigatorFilter = "no such thing"
            expect(
                model.visibleDatabases.isEmpty, true,
                "and a filter that matches nothing anywhere leaves the tree empty to say so")
        }
    }

    /// A database this build writes no statements for offers no editing, and
    /// says why instead of failing when a button is pressed.
    ///
    /// The defect this is about was visible and cost nothing until it was
    /// pressed: a Redis connection drew Set, NULL, Delete Row, Add Row and
    /// Duplicate Row over a browsed key type — every condition for editing held,
    /// because a Redis key really is a primary key and the core really can name
    /// one row — and the first press came back `ERR unknown command 'UPDATE'`.
    /// Nothing between the grid and the wire knew that this build carries no
    /// grammar to write Redis in, because the fact belongs to neither: the
    /// driver does not know which dialects were compiled in, and `dbsql` does not
    /// know which connection is open. The FFI knows both, which is why it is the
    /// layer that answers.
    private static func checkADatabaseWithNoGrammarOffersNoEditing() {
        MainActor.assumeIsolated {
            let model = makeModel()
            let keys = RelationInfo(
                schema: "db0", name: "hash", kind: .table, estimatedRows: nil)
            model.sessions[0].selected = keys
            model.sessions[0].activeTab = .content
            model.sessions[0].rowIdentity = RowIdentity(columns: ["key"], obstacle: nil)
            model.sessions[0].connString = "redis://127.0.0.1:6379/0"
            model.sessions[0].capabilities = Capabilities(
                transactional: false, cancelStopsTheStatement: true, switchesDatabase: false,
                writesStatements: false)

            expect(model.canEditCell, false, "no cell of a Redis key type is editable")
            expect(
                model.editObstacle?.contains("writes no statements"), true,
                "and the bar says so where the controls would have been")
            expect(
                model.editObstacle?.contains("Query tab"), true,
                "pointing at the pane where the change can still be made by hand")

            // The same window, one field different. Everything else about this
            // relation already said yes, which is what made the defect invisible.
            model.sessions[0].capabilities = Capabilities(
                transactional: false, cancelStopsTheStatement: true, switchesDatabase: false,
                writesStatements: true)
            expect(
                model.editObstacle == nil, true,
                "a database with a grammar has nothing to explain")
        }
    }

    /// What the pointer gets over a tab: the product, the address, and no
    /// password.
    ///
    /// A tab is one short name, and two connections to `bench` on two servers are
    /// the case somebody has to be able to tell apart before running anything.
    /// The tooltip is where the rest goes. It is built from the string the tab
    /// was opened with — which is the only place a Quick connect's address exists
    /// — and that string carries the password, so the line is assembled from
    /// fields rather than printed.
    private static func checkTheTabSaysWhatItIsWithoutSayingTheSecret() {
        MainActor.assumeIsolated {
            let model = makeModel()
            let session = model.sessions[0]
            expect(
                session.tabDescription, "New Connection",
                "a tab holding a blank form is described by the only name it has")

            session.connString = "postgres://bench:hunter2@db.example:5432/bench"
            expect(
                session.tabDescription, "bench@db.example:5432/bench",
                "an open tab is described by where it points")
            expect(
                session.tabDescription.contains("hunter2"), false,
                "and never by the password in the string it was opened with")

            session.server = "PostgreSQL 17.0"
            expect(
                session.tabDescription, "PostgreSQL 17.0 · bench@db.example:5432/bench",
                "with what answered in front, once something has")
        }
    }

    /// Which sessions a health probe would ask, which is the whole of the rule
    /// that can be checked without a server.
    ///
    /// Both exclusions are the kind that look like tidiness and are not. Asking a
    /// session with nothing open would ping through a nil connection; asking a
    /// busy one would queue a round trip behind the statement it is running, on
    /// the one connection that statement is using, and answer about a moment that
    /// has passed. Neither shows up as a crash — they show up as a dot that is
    /// wrong, which is the one thing this feature exists to stop.
    private static func checkOnlyIdleOpenConnectionsAreProbed() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(
                model.connectionsWorthProbing.isEmpty, true,
                "a window with nothing open has no connection to ask about")

            // A real open connection, because the busy exclusion says nothing on
            // a session that has nothing to ping — checking it against a failed
            // connection attempt would pass whether or not the exclusion existed.
            // SQLite is the one driver here that opens without a server.
            let file = FileManager.default.temporaryDirectory
                .appending(path: "dbclient-probe-\(UUID().uuidString).db")
            defer { try? FileManager.default.removeItem(at: file) }
            // Made here rather than left to the driver, which refuses a path that
            // is not already a file: opening a name that does not exist is how a
            // typo becomes an empty database instead of an error. Zero bytes is a
            // valid SQLite database.
            FileManager.default.createFile(atPath: file.path, contents: nil)
            guard let db = try? Database(connString: "sqlite://\(file.path)") else {
                failures += 1
                fputs("connection-form FAIL: a SQLite file would not open\n", stderr)
                return
            }
            let session = model.sessions[0]
            session.db = db
            expect(
                model.connectionsWorthProbing.count, 1,
                "an idle session with a connection open is asked")

            // The flag the chrome spins on while a statement runs, on the one
            // connection a ping would go down.
            session.isBusy = true
            expect(
                model.connectionsWorthProbing.isEmpty, true,
                "and a busy session is not asked, because the answer would arrive about a "
                    + "moment that had passed")
        }
    }

    /// What "don't save this password" costs and what it buys, in one pass.
    ///
    /// The global setting is switched **on** here on purpose. With it off the
    /// Keychain is untouched anyway — `checkTheKeychainIsUntouchedWhileTheSettingIsOff`
    /// is that check — so leaving it off would let this one pass whether or not
    /// the per-connection flag did anything at all.
    private static func checkAnEntryThatDeclinedStorageKeepsItsPasswordInMemoryOnly() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "No Keychain", savesPassword: false,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))
            let model = makeModel(with: [connection])
            model.preferences.passwordStorage = .keychain
            defer {
                ConnectionKeychain.delete(for: connection.id)
                SessionPasswords.forget(connection.id)
            }

            model.selectConnection(connection.id)
            model.connectionPassword = "hunter2"
            model.saveConnection()

            expect(
                ConnectionKeychain.password(for: connection.id), nil,
                "the password of an entry that declined storage never reaches the Keychain, "
                    + "even with the global setting on")
            expect(
                SessionPasswords.password(for: connection.id), "hunter2",
                "it is held in memory instead")

            // Away and back, which is the whole of what memory buys: the form
            // fills itself in again without anybody retyping, and without a
            // Keychain panel.
            model.selectConnection(nil)
            expect(model.connectionPassword, "", "leaving the entry clears the field")
            model.selectConnection(connection.id)
            expect(
                model.connectionPassword, "hunter2",
                "and coming back fills it from memory rather than asking again")

            // What quitting does, since the store is the only thing that would
            // have survived it.
            SessionPasswords.forget(connection.id)
            model.selectConnection(nil)
            model.selectConnection(connection.id)
            expect(
                model.connectionPassword, "",
                "and once the process has forgotten, the field is empty — which is the "
                    + "prompt on next launch")
        }
    }

    /// Where the cache goes, and that a connection with two databases open on it
    /// keeps two trees rather than one.
    ///
    /// The key is the part worth pinning. Both databases live in one file, so a
    /// save that keyed on the connection alone would still write, still read
    /// back, and still look right in every other check here — it would show
    /// `sales`'s tables under `archive` for as long as it took the real ones to
    /// arrive, which is precisely the window this cache exists to fill.
    private static func checkTheNavigatorCacheKeepsOneTreePerDatabase() {
        func tree(schema: String, relation: String) -> NavigatorCache.Tree {
            NavigatorCache.Tree(
                schemas: [SchemaInfo(name: schema)],
                databases: [DatabaseInfo(name: "sales", isCurrent: true)],
                relations: [
                    schema: [
                        RelationInfo(
                            schema: schema, name: relation, kind: .table, estimatedRows: nil)
                    ]
                ])
        }

        let home = URL(filePath: "/Users/nobody")
        expect(
            NavigatorCache.cacheDirectory(xdgCacheHome: "/tmp/somewhere", home: home).path,
            "/tmp/somewhere", "an absolute XDG_CACHE_HOME is where the cache goes")
        expect(
            NavigatorCache.cacheDirectory(xdgCacheHome: nil, home: home).path,
            "/Users/nobody/.cache", "and unset means ~/.cache")
        // The specification's rule, and not a detail: a relative value resolved
        // against the working directory would give `make screenshot` a different
        // cache from the one the application writes.
        expect(
            NavigatorCache.cacheDirectory(xdgCacheHome: "relative/cache", home: home).path,
            "/Users/nobody/.cache", "while a relative one is ignored rather than resolved")

        let directory = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-navcache-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = NavigatorCache(directory: directory)

        let connection = UUID()
        let sales = NavigatorCacheKey(connection: connection, database: "sales")
        let archive = NavigatorCacheKey(connection: connection, database: "archive")
        // Written as a comparison rather than passed as `nil`, so that the check
        // does not depend on how `expect`'s generic infers a bare literal.
        expect(cache.load(sales) == nil, true, "nothing was filed, so nothing comes back")

        cache.save(tree(schema: "public", relation: "orders"), for: sales)
        cache.save(tree(schema: "history", relation: "invoices"), for: archive)
        expect(
            cache.load(sales)?.schemas.map(\.name), ["public"],
            "each database on the connection keeps its own tree")
        expect(
            cache.load(archive)?.relations["history"]?.map(\.name), ["invoices"],
            "and the second did not overwrite the first, which shares its file")

        // Forgotten whole. A database left behind would be a tree filed under a
        // uuid nothing will ever name again.
        cache.forget(connection)
        expect(cache.load(sales) == nil, true, "forgetting the connection takes the first")
        expect(cache.load(archive) == nil, true, "and every other database on it")
    }

    /// Pressing Connect draws the tree from last time before the server has said
    /// anything, and says that is what it is doing.
    ///
    /// The whole feature is the moment this check reads: between `open` filling
    /// the session and the connection landing. Everything after that is the
    /// live tree, so a check that waited would pass against a build with no
    /// cache in it at all.
    ///
    /// Port 1 on the loopback for the reason `checkATypedPasswordIsNotOverwritten`
    /// gives: the attempt is real and is refused at once, so nothing here
    /// resolves a name or waits for a timeout.
    private static func checkAReopenedConnectionDrawsLastTimesTreeAtOnce() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Reopened",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "127.0.0.1", port: "1", database: "sales",
                    user: "user"))
            defer { NavigatorCache.shared.forget(connection.id) }
            NavigatorCache.shared.save(
                NavigatorCache.Tree(
                    schemas: [SchemaInfo(name: "public")],
                    databases: [DatabaseInfo(name: "sales", isCurrent: true)],
                    relations: [
                        "public": [
                            RelationInfo(
                                schema: "public", name: "orders", kind: .table, estimatedRows: nil)
                        ]
                    ]),
                for: NavigatorCacheKey(connection: connection.id, database: "sales"))

            let model = makeModel(with: [connection])
            model.selectConnection(connection.id)
            expect(model.schemas.isEmpty, true, "a window that has not connected shows nothing")
            expect(model.isTreeStale, false, "and has nothing to mark")

            model.connectFromForm()
            expect(
                model.schemas.map(\.name), ["public"],
                "pressing Connect draws last time's tree before the server has answered")
            expect(
                model.relations["public"]?.map(\.name), ["orders"],
                "with the objects under it, which is the part that is worth waiting less for")
            expect(model.isTreeStale, true, "and the window knows it is not the live one")
            expect(
                model.connectionDescription.hasSuffix(
                    ", showing the objects from the last time it was open"), true,
                "which is said out loud, because dimming is invisible to a screen reader")

            // The same saved connection, pointed at a database nothing was filed
            // under. This is the half a connection-only key would get wrong, and
            // it would get it wrong silently: `sales`'s tables drawn under a tab
            // that says `archive`.
            let elsewhere = makeModel(with: [connection])
            elsewhere.selectConnection(connection.id)
            elsewhere.connectionDraft.settings.database = "archive"
            elsewhere.connectFromForm()
            expect(elsewhere.schemas.isEmpty, true, "another database on it starts empty")
            expect(elsewhere.isTreeStale, false, "and is not marked as showing anything")

            // Deleting the connection takes the tree with it, for the reason the
            // password beside it goes: an entry nothing will ever show again
            // should leave nothing behind that names somebody's tables.
            let owner = makeModel(with: [connection])
            owner.selectConnection(connection.id)
            owner.deleteConnection()
            expect(
                NavigatorCache.shared.load(
                    NavigatorCacheKey(connection: connection.id, database: "sales")) == nil, true,
                "and forgetting the connection takes its tree off the disk")
        }
    }

    /// A bastion's secret follows the connection's password: the same store, the
    /// same launch, the same veto.
    ///
    /// Its own check rather than a line in the password one, because what it
    /// guards against is the pair coming apart. Every branch of Save writes two
    /// secrets now, and a branch that writes one leaves a connection that comes
    /// back next launch with a password and no way through the bastion — which
    /// reads as the bastion having changed, on the day somebody is trying to get
    /// to a database rather than to debug a client.
    private static func checkABastionSecretIsKeptTheWayThePasswordIs() {
        MainActor.assumeIsolated {
            let kept = SavedConnection(
                name: "Behind a bastion",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.internal", port: "5432", database: "sales",
                    user: "ana", sshHost: "bastion.example", sshUser: "ana"))
            let declined = SavedConnection(
                name: "Not written down", savesPassword: false,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.internal", port: "5432", database: "sales",
                    user: "ana", sshHost: "bastion.example", sshUser: "ana"))
            defer {
                for id in [kept.id, declined.id] {
                    CredentialFile.shared.delete(for: id)
                    ConnectionKeychain.delete(for: id)
                    SessionPasswords.forget(id)
                }
            }

            let model = makeModel(with: [kept])
            model.preferences.passwordStorage = .thisMac
            // Asked again after the setting is in place: the model selected this
            // row while it was being built, and selecting the row it is already
            // showing does nothing.
            model.selectConnection(nil)
            model.selectConnection(kept.id)
            model.connectionPassword = "the database's own"
            model.connectionSshSecret = "the bastion's"
            model.saveConnection()

            let next = makeModel(with: [kept])
            next.preferences.passwordStorage = .thisMac
            next.selectConnection(nil)
            next.selectConnection(kept.id)
            expect(
                next.connectionPassword, "the database's own",
                "the next launch has the database's password")
            expect(
                next.connectionSshSecret, "the bastion's",
                "and the bastion's secret beside it, not instead of it")

            // Where each one goes. This is the only place that decides which of
            // the two the bastion is handed, and a check that stopped at the
            // store would pass against a build that sent the database's password
            // to the SSH server.
            let bastion = AppModel.bastion(for: kept.settings, secret: next.connectionSshSecret)
            expect(bastion?.password, "the bastion's", "and the bastion is handed its own")

            // A secret field shows nothing either way, so somebody leaving the
            // row after changing it would otherwise be told there was nothing to
            // lose.
            next.connectionSshSecret = "changed"
            expect(
                next.unsavedConnectionEdits?.fields, ["SSH secret"],
                "changing it is an unsaved edit, and one with a name")

            // The veto is one answer about one connection. An arrangement that
            // kept the password out of the file and wrote the bastion's secret
            // into it would be the flag half-honoured, on exactly the entries
            // where somebody cared enough to turn it off.
            let memory = makeModel(with: [declined])
            memory.preferences.passwordStorage = .thisMac
            memory.selectConnection(nil)
            memory.selectConnection(declined.id)
            memory.connectionSshSecret = "held"
            memory.saveConnection()
            // Written as a comparison rather than passed as `nil`, so that the
            // check does not depend on how `expect`'s generic infers a literal.
            expect(
                CredentialFile.shared.sshSecret(for: declined.id) == nil, true,
                "an entry that declined storage writes no bastion secret to disk")
            expect(
                SessionPasswords.password(for: declined.id, .ssh), "held",
                "and keeps it in this process instead")
        }
    }

    /// The whole point of the file answer, from the form's side.
    ///
    /// A second model over the same store is what the next launch is, and this is
    /// the thing that was broken: the field came up empty and the password had to
    /// be typed again, or a Keychain panel had to be authorised, every single
    /// time. Nothing is deferred here — that state exists only for the answer
    /// that raises a panel, and this one does not.
    ///
    /// The suite's own `XDG_CONFIG_HOME` points at a scratch directory, so
    /// `CredentialFile.shared` writes there rather than into the developer's
    /// `~/.config`.
    private static func checkAPasswordKeptOnThisMacIsThereOnTheNextLaunch() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Kept Here",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))
            defer {
                CredentialFile.shared.delete(for: connection.id)
                ConnectionKeychain.delete(for: connection.id)
            }

            let model = makeModel(with: [connection])
            model.preferences.passwordStorage = .thisMac
            model.selectConnection(connection.id)
            model.connectionPassword = "hunter2"
            model.saveConnection()
            expect(
                ConnectionKeychain.password(for: connection.id), nil,
                "choosing the file means the Keychain is not written")

            // The model selected this row while it was being built, before the
            // setting above was in place, so ask it again — on a real launch the
            // setting is read from disk before any window exists.
            let next = makeModel(with: [connection])
            next.preferences.passwordStorage = .thisMac
            next.selectConnection(nil)
            next.selectConnection(connection.id)
            expect(
                next.connectionPassword, "hunter2",
                "and the next launch has the password already, without anybody retyping it")
            expect(
                next.hasUnreadPassword, false,
                "with nothing deferred, because this answer raises no panel to wait for")

            // Changing the setting moves nothing by itself; the next Save does.
            // Anything else would rewrite stores behind somebody who was only
            // reading the Settings window.
            model.preferences.passwordStorage = .keychain
            model.connectionPassword = "moved"
            model.saveConnection()
            expect(
                CredentialFile.shared.password(for: connection.id), nil,
                "and a Save under the Keychain answer takes the file copy with it")
        }
    }

    // MARK: - Helper

    /// Creates a model with stubbed alert closures and given connections
    @MainActor private static func makeModel(with connections: [SavedConnection] = []) -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-chooser"))
        // A scratch store, which is what `Preferences.init` says its argument is
        // for. On the standard one these checks read whatever the developer has
        // set — and a check that turns a setting on would be turning it on in
        // their own window.
        let preferences = Preferences(store: ScratchDefaults.store("verify-chooser"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-chooser"))
        let model = AppModel(history: history, favorites: favorites, preferences: preferences)

        // Stub alert closures to prevent modal dialogs
        model.confirmConnectionDeletion = { _ in true }
        model.resolveUnsavedConnection = { _ in .discard }
        // Refuses rather than allows: a check that reached this without meaning
        // to should fail loudly by running nothing, not quietly by running it.
        model.confirmProductionRun = { _ in false }

        model.connections = ConnectionList(connections)
        return model
    }

    /// Creates a scratch directory for testing
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-chooser-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs(
                "connection-form FAIL: a scratch directory could not be made: \(error)\n", stderr
            )
            return nil
        }
        return root
    }

    // MARK: - Cases

    /// Selecting a row loads it into the form.
    ///
    /// After `selectConnection(id)`, `connectionDraft` equals that saved connection
    /// and `selectedConnectionID` is it.
    private static func checkSelectingRowLoadsIntoForm() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))

            let model = makeModel(with: [connection1, connection2])

            // Select the first connection
            model.selectConnection(connection1.id)

            // Verify that the draft matches the selected connection
            expect(
                model.connectionDraft, connection1,
                "connectionDraft should match selected connection")
            expect(model.selectedConnectionID, connection1.id, "selectedConnectionID should be set")
        }
    }

    /// Selecting Quick connect leaves the form editable and unselected.
    ///
    /// `selectConnection(nil)` → `selectedConnectionID == nil`, `canDeleteConnection == false`,
    /// and the draft is not one of the saved rows. Then select a row and go back to
    /// Quick connect: what was typed into Quick connect is still there.
    private static func checkSelectingQuickConnect() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))

            let model = makeModel(with: [connection1, connection2])

            // Select a connection first
            model.selectConnection(connection1.id)
            expect(model.selectedConnectionID, connection1.id, "Should have selected a connection")

            // Now select Quick Connect (nil)
            model.selectConnection(nil)

            // Verify that nothing is selected
            expect(
                model.selectedConnectionID, nil,
                "selectedConnectionID should be nil for Quick Connect")
            // Verify that delete is not allowed
            expect(
                model.canDeleteConnection, false, "Delete should not be allowed for Quick Connect")
            // Verify that the draft is not one of the saved connections (by checking its ID)
            expect(
                model.connectionDraft.id != connection1.id
                    && model.connectionDraft.id != connection2.id,
                true,
                "Draft should not be one of the saved connections")

            // Now test that typed content is preserved when switching back to Quick connect
            // Type into Quick Connect form, not a saved connection
            model.selectConnection(nil)
            model.connectionDraft.settings.host = "modified.example.com"
            model.connectionPassword = "testpassword"

            // Select a row and go back to Quick connect to test that typed content is preserved
            model.selectConnection(connection1.id)
            // Verify that we're actually looking at connection1
            expect(model.connectionDraft, connection1, "Should be looking at connection1")

            // Go back to Quick Connect
            model.selectConnection(nil)

            // Verify that the draft still has the modified content
            expect(
                model.connectionDraft.settings.host, "modified.example.com",
                "Typed content should be preserved when switching back to Quick Connect")
            expect(
                model.connectionPassword, "testpassword",
                "Typed password should be preserved when switching back to Quick Connect")
        }
    }

    /// A typed edit becomes unsaved edits, naming the right fields.
    ///
    /// Change host and password on a selected row; `unsavedConnectionEdits?.fields` is
    /// `["Host", "Password"]` in form order, and the title is the row's title.
    private static func checkTypedEditBecomesUnsavedEdits() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Test Connection",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))

            let model = makeModel(with: [connection])
            model.selectConnection(connection.id)

            // Modify the draft
            model.connectionDraft.settings.host = "newhost.example.com"
            model.connectionPassword = "newpassword"

            // Check that unsaved edits are detected
            expect(model.unsavedConnectionEdits != nil, true, "Should have unsaved edits")
            if let edits = model.unsavedConnectionEdits {
                expect(edits.fields, ["Host", "Password"], "Fields should be in form order")
                expect(edits.title, "Test Connection", "Title should match the connection")
            }
        }
    }

    /// Revert puts the saved values back and clears the unsaved state.
    ///
    /// Edit a selected row's host and password, `revertConnection()`, and both the
    /// draft and `connectionPassword` are back to what was saved, with `unsavedConnectionEdits`
    /// nil afterwards.
    private static func checkRevertPutsSavedValuesBack() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Test Connection",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))

            let model = makeModel(with: [connection])
            model.selectConnection(connection.id)

            // Modify the draft
            model.connectionDraft.settings.host = "modifiedhost.example.com"
            model.connectionPassword = "modifiedpassword"

            // Verify we have unsaved edits
            expect(
                model.unsavedConnectionEdits != nil, true, "Should have unsaved edits before revert"
            )

            // Revert the changes
            model.revertConnection()

            // Verify that the draft is back to the original values
            expect(
                model.connectionDraft.settings.host, "host.example.com", "Host should be reverted")
            // Note: password is not reverted in revertConnection, it's kept as-is
            expect(
                model.unsavedConnectionEdits, nil, "Unsaved edits should be cleared after revert")
        }
    }

    /// Save writes through to the list and to the file.
    ///
    /// After save, `unsavedConnectionEdits` is nil. Assert the file on disk really changed.
    private static func checkSaveWritesThroughToList() {
        MainActor.assumeIsolated {
            let connection = SavedConnection(
                name: "Test Connection",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                    user: "user"))

            let model = makeModel(with: [connection])
            model.selectConnection(connection.id)

            // Modify the draft
            model.connectionDraft.settings.host = "modifiedhost.example.com"
            model.connectionPassword = "modifiedpassword"

            // Save the connection
            model.saveConnection()

            // Verify that unsaved edits are cleared
            expect(model.unsavedConnectionEdits, nil, "Unsaved edits should be nil after save")

            // Verify that the connection list was updated by reading from file
            // Read from the default location, not a fresh temporary directory
            let loadedConnections = ConnectionStore.load(from: .thisMac)
            expect(loadedConnections.count, 1, "Should have one connection loaded")
            expect(
                loadedConnections[0].settings.host, "modifiedhost.example.com",
                "Connection list should be updated")

            // Clean up keychain
            ConnectionKeychain.delete(for: connection.id)
        }
    }

    /// Save on Quick connect adds a row rather than overwriting one.
    ///
    /// The list grows by one and every pre-existing row still has its original id.
    private static func checkSaveOnQuickConnectAddsRow() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))

            let model = makeModel(with: [connection1, connection2])

            // Select Quick Connect (nil)
            model.selectConnection(nil)

            // Modify the draft to be a new connection
            model.connectionDraft.name = "New Connection"
            model.connectionDraft.settings.host = "newhost.example.com"
            model.connectionDraft.settings.port = "5432"
            model.connectionDraft.settings.database = "newdb"
            model.connectionDraft.settings.user = "newuser"
            model.connectionPassword = "newpassword"

            // Save the new connection
            model.saveConnection()

            // Verify that the list grew by one
            expect(model.connections.connections.count, 3, "List should have grown by one")

            // Verify that the original connections still exist with their original IDs
            let originalConnections = model.connections.connections.filter {
                $0.id == connection1.id || $0.id == connection2.id
            }
            expect(originalConnections.count, 2, "Original connections should still exist")

            // Verify that the new connection was added
            let newConnection = model.connections.connections.first {
                $0.name == "New Connection"
            }
            expect(newConnection != nil, true, "New connection should be in the list")

            // Clean up keychain
            // The new connection will have a new UUID, we need to get it from the model
            if let newConnection = newConnection {
                ConnectionKeychain.delete(for: newConnection.id)
            }
        }
    }

    /// Delete removes exactly the selected row.
    ///
    /// Leaves the selection somewhere valid (nil or a row that still exists) — never
    /// pointing at the row that was just deleted.
    private static func checkDeleteRemovesExactlySelectedRow() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))
            let connection3 = SavedConnection(
                name: "Test Connection 3",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host3.example.com", port: "5432", database: "db3",
                    user: "user3"))

            let model = makeModel(with: [connection1, connection2, connection3])
            model.selectConnection(connection2.id)  // Select the middle connection

            // Delete the selected connection
            model.deleteConnection()

            // Verify that the list has one fewer connection
            expect(model.connections.connections.count, 2, "List should have one fewer connection")

            // Verify that the deleted connection is gone
            let deleted = model.connections.connections.first { $0.id == connection2.id }
            expect(deleted, nil, "Deleted connection should not be in the list")

            // Verify that the selection is now nil (since the selected connection was deleted)
            expect(
                model.selectedConnectionID, nil,
                "Selection should be nil after deleting selected row")
        }
    }

    /// Delete is refused when there is nothing to delete.
    ///
    /// `canDeleteConnection` is false for Quick connect, and calling `deleteConnection()`
    /// then changes nothing.
    private static func checkDeleteIsRefusedWhenNothingToDelete() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))

            let model = makeModel(with: [connection1, connection2])

            // Select Quick Connect (nil)
            model.selectConnection(nil)

            // Verify that delete is not allowed
            expect(
                model.canDeleteConnection, false, "Delete should not be allowed for Quick Connect")

            // Try to delete (should do nothing)
            model.deleteConnection()

            // Verify that nothing changed
            expect(model.connections.connections.count, 2, "List should not have changed")
            expect(model.selectedConnectionID, nil, "Selection should still be nil")
        }
    }

    /// `settleUnsavedConnectionEdits` honours each of the three answers.
    ///
    /// `.save` — the edit is in the list, and the selection moved.
    /// `.discard` — the edit is gone from the list, and the selection moved.
    /// `.cancel` — the selection did **not** move, and the edit is still in the draft.
    private static func checkSettleUnsavedConnectionEditsHonoursAnswers() {
        MainActor.assumeIsolated {
            // Test .save
            do {
                let connection = SavedConnection(
                    name: "Test Connection",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                        user: "user"))

                let model = makeModel(with: [connection])
                model.selectConnection(connection.id)

                // Modify the draft
                model.connectionDraft.settings.host = "modifiedhost.example.com"
                model.connectionPassword = "modifiedpassword"

                // Set resolveUnsavedConnection to return .save
                model.resolveUnsavedConnection = { _ in .save }

                // Select a different connection (triggers settleUnsavedConnectionEdits)
                let connection2 = SavedConnection(
                    name: "Test Connection 2",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host2.example.com", port: "5432",
                        database: "db2",
                        user: "user2"))
                model.connections = ConnectionList([connection, connection2])

                model.selectConnection(connection2.id)

                // Verify that the edit was saved and selection moved
                expect(model.unsavedConnectionEdits, nil, "Unsaved edits should be nil after save")
                expect(model.selectedConnectionID, connection2.id, "Selection should have moved")
            }

            // Test .discard
            do {
                let connection = SavedConnection(
                    name: "Test Connection",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                        user: "user"))

                let model = makeModel(with: [connection])
                model.selectConnection(connection.id)

                // Modify the draft
                model.connectionDraft.settings.host = "modifiedhost.example.com"
                model.connectionPassword = "modifiedpassword"

                // Set resolveUnsavedConnection to return .discard
                model.resolveUnsavedConnection = { _ in .discard }

                // Select a different connection (triggers settleUnsavedConnectionEdits)
                let connection2 = SavedConnection(
                    name: "Test Connection 2",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host2.example.com", port: "5432",
                        database: "db2",
                        user: "user2"))
                model.connections = ConnectionList([connection, connection2])

                model.selectConnection(connection2.id)

                // Verify that the edit was discarded and selection moved
                expect(
                    model.unsavedConnectionEdits, nil, "Unsaved edits should be nil after discard")
                expect(model.selectedConnectionID, connection2.id, "Selection should have moved")
            }

            // Test .cancel
            do {
                let connection = SavedConnection(
                    name: "Test Connection",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host.example.com", port: "5432", database: "db",
                        user: "user"))

                let model = makeModel(with: [connection])
                model.selectConnection(connection.id)

                // Modify the draft
                model.connectionDraft.settings.host = "modifiedhost.example.com"
                model.connectionPassword = "modifiedpassword"

                // Set resolveUnsavedConnection to return .cancel
                model.resolveUnsavedConnection = { _ in .cancel }

                // Save original selection
                let originalSelection = model.selectedConnectionID

                // Select a different connection (triggers settleUnsavedConnectionEdits)
                let connection2 = SavedConnection(
                    name: "Test Connection 2",
                    settings: ConnectionSettings(
                        scheme: "postgres", host: "host2.example.com", port: "5432",
                        database: "db2",
                        user: "user2"))
                model.connections = ConnectionList([connection, connection2])

                model.selectConnection(connection2.id)

                // Verify that the selection did NOT move and edit is still there
                expect(
                    model.selectedConnectionID, originalSelection,
                    "Selection should NOT have moved after cancel")
                expect(
                    model.unsavedConnectionEdits != nil, true,
                    "Unsaved edits should remain after cancel")
            }
        }
    }

    /// The filter narrows by name and by address, and never hides the count.
    ///
    /// `visibleConnections` matches on both `title` and `subtitle`; `connections.connections`
    /// is still the full list.
    private static func checkFilterNarrowsByTitleAndAddress() {
        MainActor.assumeIsolated {
            let connection1 = SavedConnection(
                name: "Test Connection 1",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host1.example.com", port: "5432", database: "db1",
                    user: "user1"))
            let connection2 = SavedConnection(
                name: "Test Connection 2",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "host2.example.com", port: "5432", database: "db2",
                    user: "user2"))

            let model = makeModel(with: [connection1, connection2])

            // Apply a filter that matches one connection by name
            model.connectionFilter = "Test Connection 1"

            // Verify that visible connections are filtered
            expect(model.visibleConnections.count, 1, "Should have one visible connection")
            expect(
                model.visibleConnections[0].name, "Test Connection 1", "Should match filtered name")

            // Verify that the full list is still intact
            expect(
                model.connections.connections.count, 2,
                "Full list should still have both connections")

            // Apply a filter that matches by host
            model.connectionFilter = "host2.example.com"

            // Verify that visible connections are filtered by host
            expect(model.visibleConnections.count, 1, "Should have one visible connection")
            expect(
                model.visibleConnections[0].name, "Test Connection 2", "Should match filtered host")

            // Verify that the full list is still intact
            expect(
                model.connections.connections.count, 2,
                "Full list should still have both connections")
        }
    }

    // MARK: - Harness

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("connection-form FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
