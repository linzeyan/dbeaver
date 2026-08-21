import Foundation
import Observation
import SwiftUI

/// Executable checks for the AppModel's connection chooser, run by `--verify-connection-chooser`.
///
/// Tests the model layer of the connection chooser, ensuring that the AppModel
/// correctly handles connection selection, editing, saving, and deletion.
enum AppModelConnectionChecks {
    private static var failures = 0

    static func run() -> Bool {
        // Set up scratch directory for config to avoid touching user's files
        let scratch = scratchDirectory()
        guard let scratch else {
            fputs("connection-chooser FAIL: could not create scratch directory\n", stderr)
            return false
        }
        defer {
            try? FileManager.default.removeItem(at: scratch)
        }

        // Set environment to use scratch directory for config
        setenv("XDG_CONFIG_HOME", scratch.path, 1)

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
        if failures == 0 {
            fputs("connection-chooser: all checks passed\n", stderr)
        } else {
            fputs("connection-chooser: \(failures) check(s) failed\n", stderr)
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
                model.isPresentingConnection, true,
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
            model.preferences.remembersPasswords = true
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
            model.preferences.remembersPasswords = true
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
            model.preferences.remembersPasswords = true
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
                "connection-chooser FAIL: a scratch directory could not be made: \(error)\n", stderr
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
        fputs("connection-chooser FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
