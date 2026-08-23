import Foundation

/// Executable checks for keep-alive and the disconnect notice, run by
/// `--verify-keep-alive`.
///
/// Everything here is a rule that fails silently rather than loudly when it is
/// wrong. A ping sent to a cloud warehouse is a billed API call nobody sees on
/// any screen; an interval that reads the wrong one of its two sources pings at
/// a rate nobody chose; a notification posted over the window the person is
/// looking at, or not posted over the one they are not, is wrong in a way no
/// screenshot can show. So the checks pin the decisions — who gets pinged and
/// when, what an empty field means, when the notice fires — and leave the round
/// trips themselves to the drivers, which is where they are tested.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum KeepAliveChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        checkTheOfferFollowsTheWireOrRestSplit()
        checkAnEntrySaysNothingUntilSomebodyTypesAnInterval()
        checkTheSettingsDefaultFillsInWhenTheFormSaysNothing()
        checkATypedIntervalOutranksTheSettingsDefault()
        checkAnotherDatabaseOpenedFromATabKeepsItsRate()
        checkTheDraftNamesKeepAliveAmongUnsavedEdits()
        checkOnlyOptedInIdleSessionsComeDue()
        checkASentPingMovesTheClockWhetherOrNotItAnswers()
        checkTheNoticeFiresOncePerDropAndOnlyInTheBackground()
        checkReconnectIsOfferedExactlyToTheDroppedTab()
        checkReconnectRedialsWhatThisTabWasOpenedWith()
        if failures == 0 {
            fputs("keep-alive: all checks passed\n", stderr)
        } else {
            fputs("keep-alive: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Who is offered the field at all

    /// The form offers keep-alive exactly to the wire families.
    ///
    /// Spelled out driver by driver rather than as a count, because the split is
    /// the decision: a wire connection is an idle socket somebody's middlebox
    /// will drop, and a REST connection is a billed API with no socket to lose.
    /// The list lives in Swift (see `DriverInfo.supportsKeepAlive`), which is
    /// exactly how it can drift from `docs/drivers.md` — and this check is the
    /// tripwire for a new scheme arriving in the core without a decision here.
    private static func checkTheOfferFollowsTheWireOrRestSplit() {
        let wire = [
            "postgres", "mysql", "sqlserver", "sqlite", "clickhouse", "duckdb",
            "mongodb", "cassandra", "redis"
        ]
        let rest = ["trino", "flightsql", "snowflake", "databricks", "bigquery", "athena"]
        for scheme in wire {
            expect(
                DriverCatalog.named(scheme)?.supportsKeepAlive, true,
                "\(scheme) is a wire protocol and is offered keep-alive")
        }
        for scheme in rest {
            expect(
                DriverCatalog.named(scheme)?.supportsKeepAlive, false,
                "\(scheme) answers over REST, where a ping costs money, and is not")
        }
        // The tripwire itself: every driver the core reports has been sorted
        // into one of the two lists above, so a new one fails here rather than
        // quietly getting whichever answer the default hands out.
        expect(
            Set(DriverCatalog.all.map(\.scheme)), Set(wire + rest),
            "every driver in the catalog has been decided one way or the other")
    }

    // MARK: - What the entry on disk means

    /// Absent means the Settings default, zero means never, and a number means
    /// itself — including through a hand-edited file.
    private static func checkAnEntrySaysNothingUntilSomebodyTypesAnInterval() {
        func decoded(_ json: String) -> Int? {
            (try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(json.utf8)))?
                .toSavedConnection().settings.keepAliveSeconds
        }
        let minimal = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u""#
        expect(
            decoded(minimal + "}"), nil,
            "an entry written before the key existed follows the Settings default")
        expect(
            decoded(minimal + #","keepAliveSeconds":0}"#), 0,
            "zero survives the file, because it means never rather than nothing")
        expect(
            decoded(minimal + #","keepAliveSeconds":45}"#), 45,
            "a typed interval survives the file")
        expect(
            decoded(minimal + #","keepAliveSeconds":-3}"#), nil,
            "a negative interval in a hand-edited file is the default, not a crash")

        // The other direction: an entry that never named an interval writes no
        // key, so the file keeps saying "the default" as the default changes —
        // a 60 written into it would freeze today's answer into every entry.
        let quiet = SavedConnection.Raw(
            from: SavedConnection(
                settings: ConnectionSettings(
                    scheme: "postgres", host: "h", port: "1", database: "d", user: "u")))
        let written = String(decoding: (try? JSONEncoder().encode(quiet)) ?? Data(), as: UTF8.self)
        expect(
            written.contains("keepAliveSeconds"), false,
            "an entry with nothing typed writes no key at all")
    }

    // MARK: - How the session gets its number

    /// A form that says nothing dials with the Settings default, resolved at
    /// the moment of dialling.
    private static func checkTheSettingsDefaultFillsInWhenTheFormSaysNothing() {
        let model = makeModel()
        model.preferences.keepAliveSeconds = 25
        model.connect(using: "sqlite:///tmp/dbclient-keepalive-probe.db")
        expect(
            model.sessions[0].keepAliveSeconds, 25,
            "a connection with no interval of its own carries the Settings default")

        // Zero in Settings is "off by default", and it has to reach the session
        // as a real zero: a fallback that treated it as unset would put the
        // registered 60 back behind the user's explicit no.
        let off = makeModel()
        off.preferences.keepAliveSeconds = 0
        off.connect(using: "sqlite:///tmp/dbclient-keepalive-probe.db")
        expect(
            off.sessions[0].keepAliveSeconds, 0,
            "a Settings default of zero means the session is never pinged")
    }

    /// An interval typed on the form wins over the default — zero included,
    /// which is the value a `??` chain gets right and a "falsy" test would not.
    private static func checkATypedIntervalOutranksTheSettingsDefault() {
        let typed = makeModel()
        typed.preferences.keepAliveSeconds = 60
        typed.connectionDraft = SavedConnection(
            settings: ConnectionSettings(
                scheme: "sqlite", path: "/tmp/dbclient-keepalive-probe.db", keepAliveSeconds: 45))
        typed.connectFromForm()
        expect(
            typed.sessions[0].keepAliveSeconds, 45,
            "the interval somebody typed is the one the session carries")

        let opted = makeModel()
        opted.preferences.keepAliveSeconds = 60
        opted.connectionDraft = SavedConnection(
            settings: ConnectionSettings(
                scheme: "sqlite", path: "/tmp/dbclient-keepalive-probe.db", keepAliveSeconds: 0))
        opted.connectFromForm()
        expect(
            opted.sessions[0].keepAliveSeconds, 0,
            "a typed zero opts this connection out of a default that is on")
    }

    /// Another database opened from a tab pings at the tab's own rate.
    ///
    /// The same carry rule as `timeoutSeconds`, and checked against the same
    /// drift: `openDatabase` dials the same server, so a session that fell back
    /// to the form's current row — or to the Settings default — would take its
    /// rate from a chooser that has long since moved on.
    private static func checkAnotherDatabaseOpenedFromATabKeepsItsRate() {
        let model = makeModel()
        model.preferences.keepAliveSeconds = 60
        let tab = model.sessions[0]
        tab.connString = "postgres://ana@db.example:5432/one"
        tab.timeoutSeconds = 42
        tab.keepAliveSeconds = 45
        tab.savedName = "staging"
        model.openDatabase("two")
        let opened = model.sessions[0]
        expect(opened.keepAliveSeconds, 45, "the new database is pinged at this tab's rate")
        expect(opened.timeoutSeconds, 42, "with the same patience")
        expect(opened.savedName, "staging", "under the same saved name")
    }

    // MARK: - Who gets pinged, and when

    /// The scheduling rule, edge by edge: off means never, an interval means
    /// idle seconds, busy means skipped, and nothing open means nothing asked.
    ///
    /// Each edge fails as an invisible wrong rather than a crash — a ping down
    /// a busy session's serial queue answers about a moment that has passed,
    /// and a session pinged with the interval off is traffic nobody asked
    /// for — which is why the rule takes `now` as a parameter: it lets this
    /// check stand at any point on the clock and read the list.
    private static func checkOnlyOptedInIdleSessionsComeDue() {
        let model = makeModel()
        let now = Date()
        let session = model.sessions[0]

        // Opted in, overdue, and with nothing open: never due. The timer must
        // not be the thing that discovers a tab holding only the form.
        session.keepAliveSeconds = 30
        expect(
            model.connectionsDueForKeepAlive(at: now).isEmpty, true,
            "a session with nothing open is never due, however old its stamp")

        // A real connection, for the same reason the probe's checks open one:
        // the exclusions above say nothing on a session with nothing to ping.
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-keepalive-\(UUID().uuidString).db")
        defer { try? FileManager.default.removeItem(at: file) }
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("keep-alive FAIL: a SQLite file would not open\n", stderr)
            return
        }
        session.db = db

        session.keepAliveSeconds = 0
        expect(
            model.connectionsDueForKeepAlive(at: now).isEmpty, true,
            "zero means never, even on a connection that has sat idle forever")

        session.keepAliveSeconds = 30
        session.lastKeptAlive = now.addingTimeInterval(-29)
        expect(
            model.connectionsDueForKeepAlive(at: now).isEmpty, true,
            "a session inside its interval is left alone")

        session.lastKeptAlive = now.addingTimeInterval(-30)
        expect(
            model.connectionsDueForKeepAlive(at: now).count, 1,
            "and comes due the second the interval has passed")

        session.isBusy = true
        expect(
            model.connectionsDueForKeepAlive(at: now).isEmpty, true,
            "a busy session is skipped — the statement it is running is its own ping")
        session.isBusy = false
    }

    /// Sending a ping is what moves the clock, so a slow answer cannot cause a
    /// faster question — and three slept-through intervals owe one ping.
    private static func checkASentPingMovesTheClockWhetherOrNotItAnswers() {
        let model = makeModel()
        let now = Date()
        let session = model.sessions[0]
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-keepalive-\(UUID().uuidString).db")
        defer { try? FileManager.default.removeItem(at: file) }
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("keep-alive FAIL: a SQLite file would not open\n", stderr)
            return
        }
        session.db = db
        session.keepAliveSeconds = 30

        // Overdue three times: the tick that catches up sends one ping, and
        // the stamp lands on the tick's own clock, not on any of the moments
        // that were missed.
        session.lastKeptAlive = now.addingTimeInterval(-95)
        model.keepAliveTick(at: now)
        expect(session.lastKeptAlive, now, "the stamp is the moment the ping was sent")
        expect(
            model.connectionsDueForKeepAlive(at: now.addingTimeInterval(1)).isEmpty, true,
            "so the next tick has nothing to send — missed intervals are not made up")
        expect(
            model.connectionsDueForKeepAlive(at: now.addingTimeInterval(30)).count, 1,
            "and the session is due again one whole interval later")
    }

    // MARK: - The disconnect notice

    /// The notice fires on the transition to red, in the background, with the
    /// setting on — and under no other combination.
    ///
    /// Counted through the seam rather than observed, because there is nothing
    /// to observe: a notification posted over the window somebody is watching,
    /// or one per failing ping against a session that is already dead, are
    /// both wrong in ways no assertion on the model's own state can see. The
    /// per-drop half matters most — keep-alive pings a dead session every
    /// interval, and every failure runs through `recordHealth`.
    private static func checkTheNoticeFiresOncePerDropAndOnlyInTheBackground() {
        let model = makeModel()
        let session = model.sessions[0]
        session.connectionLabel = "sales@db.example"
        session.connectionState = .connected

        var delivered: [String] = []
        model.deliverDisconnectNotice = { delivered.append($0) }
        model.isAppFrontmost = { false }

        model.recordHealth(false, of: session)
        expect(
            delivered, ["sales@db.example"],
            "a drop in the background posts one notice, naming the connection")

        model.recordHealth(false, of: session)
        expect(
            delivered.count, 1,
            "a session already red is not re-announced by every failing ping")

        // The pooled-driver recovery and a second drop: a new red is new news.
        model.recordHealth(true, of: session)
        model.recordHealth(false, of: session)
        expect(delivered.count, 2, "a connection that recovered and dropped again is announced")

        // In front, the dot and the status line already say it.
        model.isAppFrontmost = { true }
        model.recordHealth(true, of: session)
        model.recordHealth(false, of: session)
        expect(delivered.count, 2, "nothing is posted over the window somebody is watching")

        // And the setting is a real off switch, not a suggestion.
        model.isAppFrontmost = { false }
        model.preferences.notifiesOnDisconnect = false
        model.recordHealth(true, of: session)
        model.recordHealth(false, of: session)
        expect(delivered.count, 2, "with the setting off, the background stays quiet too")
        expect(
            session.connectionState, .failed,
            "while the dot still tells the truth — only the notice is off")
    }

    // MARK: - Reconnect

    /// The button stands next to the disconnect sentence and nowhere else:
    /// red with a handle. A tab whose connect attempt was refused has no
    /// handle and gets the form, which is already the retry.
    private static func checkReconnectIsOfferedExactlyToTheDroppedTab() {
        let model = makeModel()
        let session = model.sessions[0]

        session.connectionState = .failed
        expect(
            model.canRedial, false,
            "a tab that never held a connection is not offered a redial of nothing")

        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-redial-\(UUID().uuidString).db")
        defer { try? FileManager.default.removeItem(at: file) }
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("keep-alive FAIL: a SQLite file would not open\n", stderr)
            return
        }
        session.db = db

        session.connectionState = .connected
        expect(model.canRedial, false, "a healthy connection has nothing to reconnect")
        session.connectionState = .failed
        expect(model.canRedial, true, "a dropped one is offered the way back")
    }

    /// Reconnect re-dials what this tab was opened with — string, bastion,
    /// patience, keep-alive rate, saved name — not what the form is showing.
    ///
    /// The two failure shapes this pins are both silent: a redial that read
    /// the chooser's current row would dial whatever somebody happened to be
    /// looking at, under this tab's name; and one that dropped staged edits on
    /// the way would lose typing that a recovered server could still accept.
    private static func checkReconnectRedialsWhatThisTabWasOpenedWith() {
        let model = makeModel()
        let dropped = model.sessions[0]
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-redial-\(UUID().uuidString).db")
        defer { try? FileManager.default.removeItem(at: file) }
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("keep-alive FAIL: a SQLite file would not open\n", stderr)
            return
        }
        dropped.db = db
        dropped.connString = "sqlite://\(file.path)"
        dropped.bastion = SshConfig(
            host: "bastion.example", port: 2222, user: "ana", password: nil,
            keyPath: "/home/ana/.ssh/id", passphrase: nil, knownHosts: "")
        dropped.timeoutSeconds = 42
        dropped.keepAliveSeconds = 45
        dropped.savedName = "staging"
        dropped.connectionColor = .red
        dropped.safety = ConnectionSafety(isReadOnly: true, isProduction: true)
        dropped.connectionState = .failed
        // The chooser has moved on: the draft names a different server, which
        // is exactly what a redial must not dial.
        model.connectionDraft = SavedConnection(
            settings: ConnectionSettings(
                scheme: "postgres", host: "other.example", port: "5432", database: "d",
                user: "u"))

        // Typing first: a redial with edits staged is refused by name, and the
        // tab is left exactly as it was — Revert and a recovered server are
        // both still on the table.
        dropped.staged.updates[GridCell(row: 0, column: 1)] = PendingValue(text: "typed")
        model.redial()
        expect(
            model.sessions[0] === dropped, true,
            "a redial that would lose staged edits does not happen")
        expect(
            model.errorMessage?.contains("1 change") ?? false, true,
            "and the refusal counts what stopped it")

        dropped.staged = StagedChanges()
        model.redial()
        let arriving = model.sessions[0]
        expect(arriving === dropped, false, "the redial arrives in a fresh session")
        expect(model.sessions.count, 1, "in the same tab, not a new one")
        expect(
            arriving.connString, dropped.connString,
            "dialling the string this tab was opened with, not the form's row")
        expect(
            arriving.bastion?.host, "bastion.example",
            "through the same bastion")
        expect(arriving.timeoutSeconds, 42, "with the same patience")
        expect(arriving.keepAliveSeconds, 45, "the same keep-alive rate")
        expect(arriving.savedName, "staging", "and the same saved name")
        expect(
            arriving.connectionState, .connecting,
            "as an attempt somebody can watch — never a silent background retry")
        expect(arriving.connectionColor, .red, "the colour crosses, being the person's mark")
        expect(
            arriving.safety.isReadOnly && arriving.safety.isProduction, true,
            "and so do the safety marks, for the same reason")
    }

    // MARK: - The unsaved-edits sentence

    /// A changed interval is named in the discard dialog, like every other
    /// field: a difference the sentence omits is one somebody discards without
    /// having been told.
    private static func checkTheDraftNamesKeepAliveAmongUnsavedEdits() {
        let saved = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "h", port: "1", database: "d", user: "u"))
        var draft = saved
        draft.settings.keepAliveSeconds = 45
        expect(
            saved.unsavedEdits(against: draft, passwordChanged: false)?.fields, ["Keep-alive"],
            "the interval is named among the fields that would go back")
    }

    // MARK: - Harness

    /// A model over scratch stores, with the modal closures stubbed — the same
    /// shape `AppModelConnectionChecks` builds, for the same reasons.
    private static func makeModel() -> AppModel {
        let store = ScratchDefaults.store("verify-keep-alive")
        let model = AppModel(
            history: QueryHistory(defaults: store),
            favorites: QueryFavorites(defaults: store),
            preferences: Preferences(store: store))
        model.confirmConnectionDeletion = { _ in true }
        model.resolveUnsavedConnection = { _ in .discard }
        model.confirmProductionRun = { _ in false }
        return model
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("keep-alive FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
