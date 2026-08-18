import Foundation

/// Executable checks for the connection URL, run by `--verify-connection`.
///
/// A flag on the binary for the reason `SQLScriptChecks` gives: `Package.swift`
/// declares one executable target and it links the Rust staticlib, so a test
/// target would have to reproduce that link.
///
/// What is being defended is narrow and easy to get silently wrong. A password
/// holding `@` or `/` written straight into a URL is read as the end of the
/// credentials, so what reaches the server is a host that does not exist or a
/// password that is half of one — and the failure says only "password
/// authentication failed". Nothing on screen would ever point at the encoding.
enum ConnectionChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkPlainValues()
        checkAwkwardPasswords()
        checkFormRoundTrip()
        checkParsingWhatOthersWrite()
        checkSessionLabels()
        checkDriverCatalog()
        checkFileShapedDatabases()
        checkListRoundTrip()
        checkFlatFile()
        checkMissingKeys()
        checkNewerBuild()
        checkTitleAndSubtitle()
        checkUnsavedEdits()
        checkStorageClearsOther()
        if failures == 0 {
            fputs("connection: all checks passed\n", stderr)
        } else {
            fputs("connection: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    private static func checkPlainValues() {
        expect(
            settings("db.example.com", "5432", "shop", "reader")
                .connectionString(password: "s3cr3t"),
            "postgres://reader:s3cr3t@db.example.com:5432/shop",
            "an ordinary connection needs no encoding at all")
        expect(
            settings("127.0.0.1", "5432", "bench", "bench").connectionString(password: ""),
            "postgres://bench@127.0.0.1:5432/bench",
            "an empty password is left out rather than sent as an empty one")
        expect(
            settings(" 127.0.0.1 ", "5432", "bench", "bench").connectionString(password: " p "),
            "postgres://bench:%20p%20@127.0.0.1:5432/bench",
            "the fields are trimmed and the password is not")
    }

    /// The characters that end a URL's credentials early.
    ///
    /// `@` is the one that matters most, because a password containing it splits
    /// the authority in the wrong place and the rest of the URL still parses.
    private static func checkAwkwardPasswords() {
        for password in ["two words", "with@at", "with/slash", "with:colon", "100%pure", "#hash"] {
            let text = settings("h", "1", "d", "u").connectionString(password: password)
            expect(
                ConnectionURL.password(in: text), password,
                "a password containing \(password.debugDescription) survives the round trip")
            let parsed = ConnectionSettings(connectionString: text)
            expect(parsed.host, "h", "and does not swallow the host after it")
            expect(parsed.database, "d", "or the database at the end")
            expect(parsed.user, "u", "or the user before it")
        }
    }

    private static func checkFormRoundTrip() {
        let original = settings("db.example.com", "6543", "shop", "read only")
        let parsed = ConnectionSettings(
            connectionString: original.connectionString(password: "pw"))
        expect(parsed, original, "the form's own string reads back into the same form")

        expect(settings("h", "1", "d", "u").isComplete, true, "four filled fields are complete")
        expect(settings("h", "1", "", "u").isComplete, false, "a missing database is not")
        expect(
            settings("h", "1", "d", "   ").isComplete, false, "nor is a field holding only space")
    }

    /// `--conn` is written by people and by the Makefile, so the parser has to
    /// read forms this never emits.
    private static func checkParsingWhatOthersWrite() {
        // Both spellings, because neither is ours to choose: a URL is pasted from
        // whichever console handed it out, and cloud providers print both.
        let long = ConnectionSettings(connectionString: "postgresql://bench@db:5432/shop")
        expect(long.host, "db", "postgresql:// is the same driver as postgres://")
        expect(long.database, "shop", "and the database still reads")

        let noPort = ConnectionSettings(connectionString: "postgres://bench@db/shop")
        expect(noPort.port, "", "a URL without a port leaves the field empty rather than guessing")
        expect(noPort.isComplete, false, "and the form knows there is nothing to try")

        expect(
            ConnectionURL.password(in: "postgres://bench@db/shop"), nil,
            "a URL with no password does not report an empty one")

        // What the form shows for a string that named nothing useful: empty
        // fields, not fields holding a fragment of it.
        let junk = ConnectionSettings(connectionString: "host=db.example.com dbname=shop")
        expect(junk.host, "", "a libpq keyword string is not a URL and reads as nothing")
        expect(junk.isComplete, false, "and the form knows there is nothing to try")
    }

    /// The tab's name, which is the one place a connection string is read for a
    /// reason other than connecting.
    private static func checkSessionLabels() {
        expect(
            ConnectionURL.label(for: "postgres://bench:bench@127.0.0.1:55432/bench"),
            "bench@127.0.0.1", "a server session is named database@host")
        expect(
            ConnectionURL.label(for: "sqlite:///Users/someone/notes.db"), "notes.db",
            "a database that is a file is named by its file, not by a server it has none of")
        expect(
            ConnectionURL.label(for: "sqlite://notes.db"), "notes.db",
            "a relative path parses as the authority and is still the only name there is")
        expect(
            ConnectionURL.label(for: "postgres://db.example.com"), "db.example.com",
            "a URL naming no database is named by its host")
    }

    /// The catalog the form is built from, which comes from the core rather than
    /// from this file.
    private static func checkDriverCatalog() {
        expect(
            DriverCatalog.all.isEmpty, false,
            "the core reports at least one database it can open")
        expect(
            DriverCatalog.named("postgres")?.shape, .server,
            "a database reached over a socket needs a host and a port")
        expect(
            DriverCatalog.named("sqlite")?.shape, .file,
            "a database that is a file needs a path and nothing else")
        expect(
            DriverCatalog.named("sqlite")?.defaultPort, nil,
            "a file has no port to default to")
        expect(
            DriverCatalog.named("oracle") == nil, true,
            "the form cannot offer a database this build has no driver for")
    }

    /// A file-shaped database has no host, no port and nobody to authenticate
    /// as, and the form has to stop insisting on all three.
    private static func checkFileShapedDatabases() {
        var file = ConnectionSettings(scheme: "sqlite")
        expect(file.isComplete, false, "a file driver with no file is not ready")
        file.path = "/Users/someone/notes.db"
        expect(
            file.isComplete, true,
            "a path is the whole of what a file database needs -- not a user name")
        expect(
            file.connectionString(password: ""), "sqlite:///Users/someone/notes.db",
            "an absolute path gets the three slashes the core splits on")

        let parsed = ConnectionSettings(connectionString: "sqlite:///Users/someone/notes.db")
        expect(parsed.path, "/Users/someone/notes.db", "and reads back into the path field")
        expect(parsed.host, "", "without inventing a host it does not have")

        // A relative path parses as the URL's authority, so it has to be put
        // back in front of the path or the file name reads as a server.
        let relative = ConnectionSettings(connectionString: "sqlite://notes.db")
        expect(relative.path, "notes.db", "a relative path is a path, not a host")

        // Switching driver must not empty a form somebody has been typing into.
        guard let postgres = DriverCatalog.named("postgres"),
            let mongo = DriverCatalog.named("mongodb")
        else {
            failures += 1
            fputs("connection FAIL: the catalog is missing a driver these checks need\n", stderr)
            return
        }
        let typed = settings("db.example.com", "5432", "shop", "reader")
        let moved = typed.moved(to: mongo)
        expect(moved.host, "db.example.com", "switching database keeps the host")
        expect(moved.database, "shop", "and the database name")
        expect(moved.port, "27017", "but takes the new driver's default port")

        var custom = typed
        custom.port = "6543"
        expect(
            custom.moved(to: mongo).port, "6543",
            "a port typed by hand was typed for a reason and stays")
        expect(
            ConnectionSettings(scheme: "sqlite", path: "/x.db").moved(to: postgres).host,
            "127.0.0.1", "moving to a server driver fills in the host it now needs")
    }

    /// A list round-trips through the file.
    ///
    /// Save three connections to a scratch `ConnectionDirectories`, load them back:
    /// same count, same order, same ids, same names, colours and settings.
    private static func checkListRoundTrip() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: root.appending(path: "drive"))

        let connections = [
            SavedConnection(
                id: UUID(uuidString: "11111111-1111-1111-1111-111111111111")!,
                name: "sales",
                color: .red,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                    user: "ana")),
            SavedConnection(
                id: UUID(uuidString: "22222222-2222-2222-2222-222222222222")!,
                name: "inventory",
                color: .blue,
                settings: ConnectionSettings(
                    scheme: "mysql", host: "db.example", port: "3306", database: "inventory",
                    user: "bob")),
            SavedConnection(
                id: UUID(uuidString: "33333333-3333-3333-3333-333333333333")!,
                name: "analytics",
                color: .green,
                settings: ConnectionSettings(
                    scheme: "sqlite", path: "/tmp/data.db"))
        ]

        ConnectionStore.save(connections, to: .thisMac, in: directories)
        let loaded = ConnectionStore.load(from: .thisMac, in: directories)
        expect(loaded.count, connections.count, "the list has the same number of connections")
        expect(
            loaded, connections,
            "the connections are the same in order, id, name, color and settings")
    }

    /// The file is flat.
    ///
    /// Encode a document and decode it as `[String: Any]` via `JSONSerialization`.
    /// Assert an entry has `host` at its own top level and no `settings` key.
    /// Why it matters: the file is one a person edits by hand.
    private static func checkFlatFile() {
        let connection = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let document = SavedConnections(connections: [connection])

        // Encode to JSON data
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(document) else {
            failures += 1
            fputs("connection FAIL: could not encode document\n", stderr)
            return
        }

        // Decode as [String: Any] using JSONSerialization
        guard let jsonDict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            failures += 1
            fputs("connection FAIL: could not decode as JSON dictionary\n", stderr)
            return
        }

        // Check that the connections array exists and has the right structure
        guard let connections = jsonDict["connections"] as? [[String: Any]],
            connections.count == 1,
            let firstConnection = connections.first
        else {
            failures += 1
            fputs("connection FAIL: connections array not found or invalid\n", stderr)
            return
        }

        // Check that host is at the top level and settings key is not present
        expect(firstConnection["host"] as? String, "db.example", "host is at the top level")
        expect(firstConnection["settings"] as? String, nil, "settings key is not present")
    }

    /// A hand-edited entry missing keys still loads.
    ///
    /// Decode this JSON text (write it as a Swift string literal, do not build a
    /// `Raw` in Swift — the defect being checked is in decoding):
    ///
    /// ```json
    /// {"version": 1, "connections": [{"scheme": "postgres", "host": "db.example",
    ///  "port": "5432", "database": "sales", "user": "ana"}]}
    /// ```
    ///
    /// Assert: one connection loads, its name is empty, its colour is `.none`, and its
    /// `title` is the derived `sales@db.example`. A missing key must not empty the list —
    /// a decoder that throws here loses every connection in the file, not one field.
    private static func checkMissingKeys() {
        let jsonText = """
            {"version": 1, "connections": [{"scheme": "postgres", "host": "db.example",
             "port": "5432", "database": "sales", "user": "ana"}]}
            """

        guard let data = jsonText.data(using: .utf8) else {
            failures += 1
            fputs("connection FAIL: could not create data from JSON text\n", stderr)
            return
        }

        // Decode using JSONDecoder
        guard let document = try? JSONDecoder().decode(SavedConnections.self, from: data) else {
            failures += 1
            fputs("connection FAIL: could not decode JSON with missing keys\n", stderr)
            return
        }

        expect(document.connections.count, 1, "one connection loads")

        let connection = document.connections[0]
        expect(connection.name, "", "its name is empty")
        expect(connection.color, .none, "its colour is .none")
        expect(connection.title, "sales@db.example", "its title is derived from database@host")
    }

    /// A document from a newer build reads as nothing.
    ///
    /// `{"version": 99, "connections": [ … one valid entry … ]}` loads as no connections.
    /// Reading entries under a shape this build does not know is worse than asking for
    /// the connection again.
    private static func checkNewerBuild() {
        let jsonText = """
            {"version": 99, "connections": [{"scheme": "postgres", "host": "db.example",
             "port": "5432", "database": "sales", "user": "ana"}]}
            """

        guard let data = jsonText.data(using: .utf8) else {
            failures += 1
            fputs("connection FAIL: could not create data from JSON text\n", stderr)
            return
        }

        // Decode using JSONDecoder
        guard let document = try? JSONDecoder().decode(SavedConnections.self, from: data) else {
            failures += 1
            fputs("connection FAIL: could not decode JSON with newer version\n", stderr)
            return
        }

        expect(document.connections.count, 0, "loads as no connections")
    }

    /// `title` and `subtitle`.
    ///
    /// A named connection uses its name; an unnamed server is `database@host`;
    /// one with no database falls back to the host; a file connection is named
    /// by its file and subtitled by its path; an empty one is "Untitled".
    /// Subtitles: `ana@db.example:5432/sales`, and one with no port reads
    /// `ana@db.example/sales` — a separator with nothing behind it looks like
    /// the line was cut off.
    private static func checkTitleAndSubtitle() {
        // Named connection
        let named = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        expect(named.title, "sales", "a named connection uses its name")
        expect(
            named.subtitle, "ana@db.example:5432/sales",
            "subtitle includes user, host, port and database")

        // Unnamed server connection
        let unnamed = SavedConnection(
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        expect(unnamed.title, "sales@db.example", "an unnamed server is database@host")

        // Unnamed server with no database
        let noDatabase = SavedConnection(
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "",
                user: "ana"))
        expect(noDatabase.title, "db.example", "one with no database falls back to the host")

        // File connection
        let file = SavedConnection(
            settings: ConnectionSettings(
                scheme: "sqlite", path: "/tmp/data.db"))
        expect(file.title, "data.db", "a file connection is named by its file")
        expect(file.subtitle, "/tmp/data.db", "a file connection is subtitled by its path")

        // Untitled connection
        let empty = SavedConnection(
            name: "",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        expect(empty.title, "sales@db.example", "an empty one is named database@host")

        // Subtitle with no port
        let noPort = SavedConnection(
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "", database: "sales",
                user: "ana"))
        expect(
            noPort.subtitle, "ana@db.example/sales", "one with no port reads ana@db.example/sales")
    }

    /// `unsavedEdits`.
    ///
    /// Against an identical draft with `passwordChanged: false` it is nil.
    /// Change the host and the port: the fields are `["Host", "Port"]` in that order,
    /// and `detail` reads `Host and Port would go back to what was saved.`.
    /// Change only the password: the fields are `["Password"]`.
    /// Change nothing but pass `passwordChanged: true`: still `["Password"]` —
    /// the form is the only thing that knows, which is exactly why it is a parameter.
    private static func checkUnsavedEdits() {
        let original = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))

        // Identical draft with passwordChanged: false
        let identical = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        expect(
            original.unsavedEdits(against: identical, passwordChanged: false), nil,
            "nil when identical")

        // Change host and port
        let changedHostPort = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "new.example", port: "5433", database: "sales",
                user: "ana"))
        let edits1 = original.unsavedEdits(against: changedHostPort, passwordChanged: false)
        expect(edits1?.fields, ["Host", "Port"], "fields are Host and Port in that order")
        expect(
            edits1?.detail, "Host and Port would go back to what was saved.", "detail is correct")

        // Change only password
        let changedPassword = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let edits2 = original.unsavedEdits(against: changedPassword, passwordChanged: true)
        expect(edits2?.fields, ["Password"], "fields are Password")

        // Change nothing but passwordChanged: true
        let noChangePassword = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let edits3 = original.unsavedEdits(against: noChangePassword, passwordChanged: true)
        expect(edits3?.fields, ["Password"], "fields are Password when passwordChanged is true")
    }

    /// Saving one storage clears the other.
    ///
    /// Save a list to `.thisMac`, then a different list to `.iCloud`, against a scratch
    /// pair: the local file is gone, iCloud holds the second list.
    /// (`PreferencesChecks` checks this for the file's existence; check it here
    /// for what `load` returns, which is the half a user notices.)
    private static func checkStorageClearsOther() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: root.appending(path: "drive"))

        // Save first list to thisMac
        let firstList = [
            SavedConnection(
                name: "first",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db1.example", port: "5432", database: "first",
                    user: "ana"))
        ]
        ConnectionStore.save(firstList, to: .thisMac, in: directories)

        // Check that it's there
        let loadedFirst = ConnectionStore.load(from: .thisMac, in: directories)
        expect(loadedFirst.count, 1, "first list is saved to thisMac")

        // Save second list to iCloud
        let secondList = [
            SavedConnection(
                name: "second",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db2.example", port: "5432", database: "second",
                    user: "bob"))
        ]
        ConnectionStore.save(secondList, to: .iCloud, in: directories)

        // Check that first list is gone from thisMac
        let loadedAfter = ConnectionStore.load(from: .thisMac, in: directories)
        expect(
            loadedAfter.count, 0,
            "first list is cleared from thisMac when second is saved to iCloud")

        // Check that second list is in iCloud
        let loadedSecond = ConnectionStore.load(from: .iCloud, in: directories)
        expect(loadedSecond.count, 1, "second list is in iCloud")
        expect(loadedSecond[0].name, "second", "second list has correct name")
    }

    // MARK: - Harness

    /// A directory nothing else can see, for the checks that write files. Removed
    /// by the caller, so a failing check leaves nothing behind either.
    private static func scratchDirectory() -> URL? {
        let root = URL(filePath: NSTemporaryDirectory())
            .appending(path: "dbclient-verify-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs("connection FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func settings(
        _ host: String, _ port: String, _ database: String, _ user: String
    ) -> ConnectionSettings {
        ConnectionSettings(
            scheme: "postgres", host: host, port: port, database: database, user: user)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("connection FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
