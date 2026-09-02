import Foundation

/// Executable checks for reopening the last window's tabs, run by
/// `--verify-session-restore`.
///
/// A real `AppModel` over a real file in a scratch directory, because the two
/// halves that can be wrong are on either side of the disk: what a window writes
/// down when it quits, and what the next one makes of it. Both are drivable with
/// no database — `Session` holds the connection, and every one of these builds a
/// window that has none.
///
/// Restore is also the only way to give a model more than one tab without a
/// server to connect to, which is why the multi-tab cases go in through a
/// document rather than through `presentConnection`.
enum SessionRestoreChecks {
    private static var failures = 0

    static func run() -> Bool {
        // The config goes somewhere disposable before a model exists, the way
        // `BrowseRestoreChecks` does it and for the same reason: a model reads
        // the user's own saved connections otherwise, and asks the Keychain about
        // the first — which blocks for ever in a process with no GUI session, so
        // the symptom is a `make test-swift` that never returns.
        guard let scratch = scratchDirectory() else { return false }
        defer { try? FileManager.default.removeItem(at: scratch) }
        setenv("XDG_CONFIG_HOME", scratch.path, 1)
        // Held so the one case that points the config somewhere else can put it
        // back; see `checkAWindowOpensOnTheTabItLeftOff`.
        sharedConfig = scratch

        failures = 0
        defer { ScratchDefaults.release() }
        checkNothingIsConnectedOnTheWayBackIn()
        checkTheTabsComeBackWithTheirNamesAndTheirSQL()
        checkTheFormFollowsTheTabInFront()
        checkADeletedConnectionRestoresAnEmptyForm()
        checkATabDialledByHandKeepsItsFieldsAndNoPassword()
        checkATabLeftAloneSurvivesASecondLaunch()
        checkTurningItOffKeepsNothing()
        checkAFileThisBuildDoesNotKnowIsIgnored()
        checkAWindowOpensOnTheTabItLeftOff()
        checkEveryWindowIsPutBackInOrder()
        if failures == 0 {
            fputs("session-restore: all checks passed\n", stderr)
        } else {
            fputs("session-restore: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The claim the whole feature rests on. Every restored tab is a form, not a
    /// connection: a client that dialled five servers because somebody launched
    /// it would be writing to production before anybody had looked at the screen,
    /// and this application's rule has always been that it opens nothing nobody
    /// named this time.
    private static func checkNothingIsConnectedOnTheWayBackIn() {
        MainActor.assumeIsolated {
            let store = store(named: "connected")
            store.save([
                RestoredWindow(
                    tabs: [tab(named: "sales", on: salesID), tab(named: "archive", on: archiveID)],
                    activeTab: 1)
            ])
            let model = makeModel(restoring: store.load().first)
            expect(model.sessions.count, 2, "both tabs come back")
            expect(
                model.sessions.allSatisfy { $0.db == nil }, true,
                "and neither of them is connected")
            expect(
                model.isShowingConnectionForm, true, "so the window is showing the form")
            expect(
                model.sessions.allSatisfy { !$0.hasBeenAsked }, true,
                "and no tab claims an attempt was made, which is what draws the status dot")
        }
    }

    /// The feature. The tab strip reads the same, and the SQL somebody had typed
    /// is where they left it — including which buffer they were in, because a
    /// window that reopens on `query 1` has moved their caret for them.
    private static func checkTheTabsComeBackWithTheirNamesAndTheirSQL() {
        MainActor.assumeIsolated {
            let store = store(named: "buffers")
            var first = tab(named: "sales", on: salesID)
            first.buffers = [
                RestoredBuffer(name: "query 1", text: "select 1"),
                RestoredBuffer(name: "migration", text: "alter table orders add column note text")
            ]
            first.activeBuffer = 1
            store.save([RestoredWindow(tabs: [first], activeTab: 0)])
            let model = makeModel(restoring: store.load().first)
            expect(model.connectionLabel, "sales", "the tab is called what it was called")
            expect(model.queryBuffers.count, 2, "both buffers come back")
            expect(model.queryBuffers.map(\.name), ["query 1", "migration"], "under their names")
            expect(
                model.queryText, "alter table orders add column note text",
                "and the editor opens in the one it was left in")
        }
    }

    /// What makes a strip of restored tabs worth having. There is one form in the
    /// window, so without this a window restored with three tabs would draw the
    /// same fields under all three and choosing between them would choose nothing.
    private static func checkTheFormFollowsTheTabInFront() {
        MainActor.assumeIsolated {
            let store = store(named: "following")
            store.save([
                RestoredWindow(
                    tabs: [tab(named: "sales", on: salesID), tab(named: "archive", on: archiveID)],
                    activeTab: 0)
            ])
            let model = makeModel(restoring: store.load().first)
            model.connections = ConnectionList([
                saved(salesID, "sales"), saved(archiveID, "archive")
            ])
            model.selectSession(0)
            expect(model.selectedConnectionID, salesID, "the front tab's connection is in the form")
            model.selectSession(1)
            expect(model.selectedConnectionID, archiveID, "and moving tab moves the form with it")
            model.selectSession(0)
            expect(model.selectedConnectionID, salesID, "and back again")
        }
    }

    /// A connection deleted between the two launches stays deleted. Restoring is
    /// remembering which row a tab was on, and a tab that put a deleted server's
    /// host, port and user back on screen would be undoing somebody's deletion
    /// with no way for them to see it had happened.
    private static func checkADeletedConnectionRestoresAnEmptyForm() {
        MainActor.assumeIsolated {
            let store = store(named: "deleted")
            store.save([RestoredWindow(tabs: [tab(named: "sales", on: salesID)], activeTab: 0)])
            let model = makeModel(restoring: store.load().first)
            // The list it comes back to has everything but that row.
            model.connections = ConnectionList([saved(archiveID, "archive")])
            model.selectSession(0)
            expect(
                model.selectedConnectionID, nil,
                "a tab whose connection is gone lands on Quick connect")
            // Quick connect is the suggested form rather than a blank one, so
            // what says the deleted row is not on screen is its name and its
            // address — not an empty host, which Quick connect has never had.
            expect(model.connectionDraft.name, "", "which is a form naming nothing")
            expect(
                model.connectionDraft.settings.host, "127.0.0.1",
                "with the suggested address rather than the deleted connection's")
        }
    }

    /// Quick connect and `--conn` have no row to point at, so their fields travel
    /// in the document — and the password does not, because there is no field in
    /// `ConnectionSettings` for one. That is what makes this file safe to write in
    /// plain text next to the connections.
    private static func checkATabDialledByHandKeepsItsFieldsAndNoPassword() {
        MainActor.assumeIsolated {
            let model = makeModel(restoring: nil)
            model.sessions[0].connString = "postgres://ana:hunter2@db.example:5432/sales"
            model.sessions[0].connectionLabel = "db.example/sales"
            let remembered = model.rememberedWindow
            expect(remembered.tabs.count, 1, "the window writes its one tab down")
            expect(remembered.tabs[0].connection, nil, "with no saved row to name")
            expect(
                remembered.tabs[0].settings?.host, "db.example",
                "and the fields it was dialled with")
            expect(remembered.tabs[0].settings?.user, "ana", "including the user")
            // The whole document, because a password could only leak through some
            // field nobody thought to check individually.
            let written =
                String(
                    data: (try? JSONEncoder().encode(remembered)) ?? Data(), encoding: .utf8) ?? ""
            expect(written.contains("hunter2"), false, "and no password anywhere in the file")

            // A tab opened from a saved row keeps the id instead, so a connection
            // edited between the launches is restored as it is now.
            model.sessions[0].openedFrom = salesID
            expect(model.rememberedWindow.tabs[0].connection, salesID, "a saved tab names its row")
            expect(
                model.rememberedWindow.tabs[0].settings, nil,
                "and carries no copy of its fields")
        }
    }

    /// Restore has to survive itself. A tab restored and left alone is written
    /// back as it came in — the defect this guards against is a second launch
    /// finding an empty form, because a tab that never connected has no
    /// connection string to rebuild its fields from.
    private static func checkATabLeftAloneSurvivesASecondLaunch() {
        MainActor.assumeIsolated {
            let store = store(named: "twice")
            var byHand = RestoredTab(
                connection: nil,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                    user: "ana"),
                label: "db.example/sales", buffers: [], activeBuffer: 0)
            byHand.buffers = [RestoredBuffer(name: "query 1", text: "select 1")]
            store.save([
                RestoredWindow(tabs: [tab(named: "sales", on: salesID), byHand], activeTab: 0)
            ])

            let first = makeModel(restoring: store.load().first)
            store.remember([first.rememberedWindow], restoring: true)
            let second = makeModel(restoring: store.load().first)
            expect(second.sessions.count, 2, "the second launch has the same two tabs")
            expect(second.sessions[1].connectionLabel, "db.example/sales", "under the same names")
            expect(
                second.rememberedWindow.tabs[1].settings?.host, "db.example",
                "and the hand-dialled tab still has its fields")
            expect(
                second.sessions[1].queryBuffers.first?.text, "select 1",
                "and its SQL")
            expect(
                second.rememberedWindow.tabs[0].connection, salesID,
                "and the saved tab still names its row")
        }
    }

    /// Turning the setting off deletes what was kept. Not writing the next one
    /// would leave the last window's SQL on the disk indefinitely, which is the
    /// opposite of what somebody switching it off is asking for.
    private static func checkTurningItOffKeepsNothing() {
        MainActor.assumeIsolated {
            let store = store(named: "off")
            store.save([RestoredWindow(tabs: [tab(named: "sales", on: salesID)], activeTab: 0)])
            let defaults = ScratchDefaults.store("verify-session-restore-off")
            let preferences = Preferences(store: defaults)
            expect(preferences.restoresSession, true, "restoring is what a fresh install does")
            preferences.restoresSession = false

            let model = makeModel(
                restoring: store.windowsToRestore(restoring: preferences.restoresSession)
                    .first, preferences: preferences)
            expect(model.sessions.count, 1, "a window with it off opens on one tab")
            expect(model.connectionLabel, "New Connection", "and puts nothing back")
            store.remember([model.rememberedWindow], restoring: false)
            expect(store.load().isEmpty, true, "and quitting deletes what was kept")
        }
    }

    /// A document in a shape this build does not know is ignored rather than
    /// guessed at, and a hand-edited one cannot point the editor past the end of
    /// its own buffers.
    private static func checkAFileThisBuildDoesNotKnowIsIgnored() {
        MainActor.assumeIsolated {
            let store = store(named: "shapes")
            // Written past this store rather than through it, because `save`
            // always writes the version this build reads — a document from the
            // future is the one thing the store cannot be asked to produce.
            let ahead = RestoredWindows(
                version: RestoredWindows.currentVersion + 1,
                windows: [RestoredWindow(tabs: [tab(named: "sales", on: salesID)], activeTab: 0)])
            try? JSONEncoder().encode(ahead).write(to: store.file)
            expect(store.load().isEmpty, true, "a later version is not read")
            expect(
                makeModel(restoring: store.load().first).connectionLabel, "New Connection",
                "so the window opens as a fresh one")

            // A window is a list of tabs and a pointer into it, so a document
            // holding no tabs describes no window: read literally it would leave
            // the pointer aimed past the end of an empty list.
            store.save([RestoredWindow(tabs: [], activeTab: 0)])
            expect(store.load().isEmpty, true, "a document with no tabs in it is not a window")
            expect(
                makeModel(restoring: store.load().first).sessions.count, 1,
                "and does not empty the strip")

            var edited = tab(named: "sales", on: salesID)
            edited.activeBuffer = 9
            store.save([RestoredWindow(tabs: [edited], activeTab: 7)])
            let model = makeModel(restoring: store.load().first)
            expect(
                model.activeQueryBufferIndex, 0, "a buffer index past the end folds to the first")
            expect(model.sessions.count, 1, "and a tab index past the end folds to the front tab")
            expect(model.connectionLabel, "sales", "which is the tab that came back")
        }
    }

    /// The launch itself, against a real `connections.json`.
    ///
    /// The cases above install the list by hand and then move tab, which reaches
    /// everything except the one call that happens before anybody has touched the
    /// window: a launch has to open on the tab it left off on, not on the first
    /// saved row. Both halves are here — a tab naming a row, and a tab dialled by
    /// hand, which has no row to name and carries its fields instead.
    private static func checkAWindowOpensOnTheTabItLeftOff() {
        MainActor.assumeIsolated {
            // A config directory of this case's own, because writing a
            // `connections.json` into the shared one would change what every
            // model above opens with.
            guard let config = scratchDirectory() else { return }
            defer {
                try? FileManager.default.removeItem(at: config)
                if let shared = sharedConfig { setenv("XDG_CONFIG_HOME", shared.path, 1) }
            }
            setenv("XDG_CONFIG_HOME", config.path, 1)
            // An explicit pair rather than `.system`: saving to one of the two
            // deletes the file in the other, and the other is the developer's own
            // iCloud Drive.
            ConnectionStore.save(
                [saved(salesID, "sales"), saved(archiveID, "archive")], to: .thisMac,
                in: ConnectionDirectories(local: config, cloud: nil))

            let store = store(named: "launch")
            store.save([
                RestoredWindow(tabs: [tab(named: "archive", on: archiveID)], activeTab: 0)
            ])
            let onARow = makeModel(restoring: store.load().first)
            expect(onARow.connections.connections.count, 2, "the saved connections are read")
            // `sales` is the first row and the guess a window with nothing to
            // restore opens on, which is what makes `archive` the answer only a
            // restored tab can give.
            expect(
                onARow.selectedConnectionID, archiveID,
                "a launch opens with the front tab's connection in the form")

            var byHand = tab(named: "db.example/sales", on: salesID)
            byHand.connection = nil
            byHand.settings = ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana")
            store.save([RestoredWindow(tabs: [byHand], activeTab: 0)])
            let dialled = makeModel(restoring: store.load().first)
            expect(
                dialled.selectedConnectionID, nil,
                "a tab dialled by hand opens on Quick connect rather than on a saved row")
            expect(
                dialled.connectionDraft.settings.host, "db.example",
                "with the fields it was dialled with in the form")
            expect(dialled.connectionDraft.settings.user, "ana", "including the user")
        }
    }

    /// Every window comes back, in the order they were made.
    ///
    /// The order is the whole of it: the windows cascade down the screen in the
    /// order the list holds them, so a launch that put them back in another one
    /// would move somebody's windows around for no reason they could see. What is
    /// checked here is the document — which window's tabs are where — because the
    /// half that opens `NSWindow`s cannot be driven with nothing on screen.
    private static func checkEveryWindowIsPutBackInOrder() {
        MainActor.assumeIsolated {
            let store = store(named: "several")
            let first = RestoredWindow(tabs: [tab(named: "sales", on: salesID)], activeTab: 0)
            let second = RestoredWindow(
                tabs: [tab(named: "archive", on: archiveID), tab(named: "sales", on: salesID)],
                activeTab: 1)
            store.remember([first, second], restoring: true)

            let back = store.load()
            expect(back.count, 2, "both windows come back")
            expect(back.first?.tabs.count, 1, "the first with its one tab")
            expect(back.last?.tabs.count, 2, "the second with its two")
            expect(back.last?.activeTab, 1, "each opening on the tab it was left on")
            expect(back.first?.tabs.first?.label, "sales", "and in the order they were written")

            // A window with no tabs is not a window, so it is dropped rather than
            // restored as a pointer into an empty list — and dropping it must not
            // take the windows on either side of it with it.
            store.remember(
                [first, RestoredWindow(tabs: [], activeTab: 0), second], restoring: true)
            expect(store.load().count, 2, "an empty window is dropped and the others are not")
            expect(store.load().last?.activeTab, 1, "and the ones that are kept are unchanged")

            expect(
                store.windowsToRestore(restoring: false).isEmpty, true,
                "and with the setting off there is nothing to put back")
        }
    }

    // MARK: - Fixture

    private static let salesID = UUID()
    private static let archiveID = UUID()

    /// The config directory every case but one runs against.
    private static var sharedConfig: URL?

    /// A tab as a window that had connected would have written it: a row's id, a
    /// label, and one empty buffer.
    private static func tab(named label: String, on connection: UUID) -> RestoredTab {
        RestoredTab(
            connection: connection, settings: nil, label: label,
            buffers: [RestoredBuffer(name: "query 1", text: "")], activeBuffer: 0)
    }

    private static func saved(_ id: UUID, _ name: String) -> SavedConnection {
        SavedConnection(
            id: id, name: name,
            // Nothing here reaches the Keychain — see `deferPassword`, which
            // answers an entry that declined storage out of memory instead.
            savesPassword: false,
            settings: ConnectionSettings(
                scheme: "postgres", host: "\(name).example", port: "5432", database: name,
                user: "ana"))
    }

    /// A store in the scratch directory, one file per case, so no case can be
    /// read by the next one.
    private static func store(named label: String) -> SessionRestoreStore {
        SessionRestoreStore(
            file: FileManager.default.temporaryDirectory
                .appending(path: "dbclient-verify-session-\(label)-\(UUID().uuidString).json"))
    }

    /// A directory of its own for the config these checks must not read.
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-session-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs("session-restore FAIL: a scratch directory could not be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    @MainActor private static func makeModel(
        restoring tabs: RestoredWindow?, preferences: Preferences? = nil
    ) -> AppModel {
        AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-session-restore")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-session-restore")),
            preferences: preferences
                ?? Preferences(
                    store: ScratchDefaults.store("verify-session-restore")),
            restoring: tabs)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("session-restore FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
