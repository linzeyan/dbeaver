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
