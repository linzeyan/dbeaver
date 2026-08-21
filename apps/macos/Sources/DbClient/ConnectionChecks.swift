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
        checkConnectionList()
        checkSafetyFlags()
        checkServerRecord()
        checkWriteRefusal()
        checkSslParameters()
        checkSslSurvivesTheFile()
        checkFoldersGroupTheList()
        checkAnEntryWithoutTheKeyStillSavesItsPassword()
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

        // The picker's order and the default are two different questions, and
        // the tempting fix — sorting `all` — answers both with one answer. It
        // would move what a new connection opens on from PostgreSQL to whichever
        // name sorts first, which is a change nobody asked for and one that no
        // other check here would notice.
        expect(
            DriverCatalog.inNameOrder.map(\.label),
            DriverCatalog.all.map(\.label).sorted {
                $0.localizedStandardCompare($1) == .orderedAscending
            },
            "the picker offers every database, by name")
        expect(
            DriverCatalog.inNameOrder.count, DriverCatalog.all.count,
            "and offers all of them, not a subset that happened to sort")
        expect(
            DriverCatalog.first?.scheme, DriverCatalog.all.first?.scheme,
            "while the empty form still opens on the one the core put first")

        // The answer has to cross the FFI as itself. A decoder that quietly read
        // a missing key as `false` would take the SSL section off the one form
        // that has it, and nothing else in the window would contradict that.
        expect(
            DriverCatalog.named("postgres")?.honoursSslMode, true,
            "PostgreSQL reads sslmode out of the connection string")
        expect(
            DriverCatalog.named("sqlite")?.honoursSslMode, false,
            "and a file on this disk has no wire for anybody to read")
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

    /// The list survives the file.
    ///
    /// Order is asserted along with the contents because the file is the only place
    /// it is kept: the window shows the connections in the order they are read, so a
    /// store that returned a set would quietly reshuffle somebody's sidebar on every
    /// launch. Ids are fixed here rather than generated, since an id that changed
    /// across a save is a password that can no longer be found.
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

    /// Each entry is one flat object.
    ///
    /// Checked through `JSONSerialization` rather than by decoding back into
    /// `SavedConnection`, because a round trip through the same `Raw` that wrote it
    /// would agree with any shape at all. What is being defended is the shape a
    /// person sees when they open the file: `host` where they can reach it, and no
    /// `settings` object wrapped around the fields for the program's convenience.
    private static func checkFlatFile() {
        let connection = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let document = SavedConnections(connections: [connection])

        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(document) else {
            failures += 1
            fputs("connection FAIL: the document could not be encoded\n", stderr)
            return
        }

        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            failures += 1
            fputs("connection FAIL: what was written is not a JSON object\n", stderr)
            return
        }

        guard let entries = object["connections"] as? [[String: Any]],
            let entry = entries.first, entries.count == 1
        else {
            failures += 1
            fputs("connection FAIL: the document holds one entry under `connections`\n", stderr)
            return
        }

        expect(entry["host"] as? String, "db.example", "the host is a key of the entry itself")
        // By key rather than by casting the value: a nested `settings` object casts to
        // nil under any type this check could name, so a test written that way passes
        // against exactly the shape it exists to forbid.
        expect(
            entry.keys.contains("settings"), false,
            "and the fields are not wrapped in a settings object")
    }

    /// An entry somebody typed by hand, with the optional keys left out.
    ///
    /// From JSON text rather than from a `Raw` built in Swift, because the defect
    /// this defends against lives in the decoder: a synthesized one throws on the
    /// missing key, and a throw anywhere in the array fails the whole document — so
    /// one forgotten `"color"` costs the reader every connection in the file rather
    /// than one field of one entry.
    private static func checkMissingKeys() {
        let jsonText = """
            {"version": 1, "connections": [{"scheme": "postgres", "host": "db.example",
             "port": "5432", "database": "sales", "user": "ana"}]}
            """

        guard
            let document = try? JSONDecoder().decode(
                SavedConnections.self, from: Data(jsonText.utf8))
        else {
            failures += 1
            fputs("connection FAIL: an entry with keys left out still loads\n", stderr)
            return
        }

        expect(document.connections.count, 1, "an entry with keys left out still loads")

        let connection = document.connections[0]
        expect(connection.name, "", "the name it does not carry reads as none")
        expect(connection.color, .none, "and so does the colour")
        expect(
            connection.title, "sales@db.example",
            "and the row falls back to naming it after what it opens")
    }

    /// A file written by a build this one has never heard of.
    ///
    /// It reads as no connections rather than as entries interpreted under a shape
    /// this build does not know — the file syncs between machines, so the newer
    /// version of it is the case that actually happens. Being asked for a connection
    /// is survivable; being shown fields that mean something else is not.
    private static func checkNewerBuild() {
        let jsonText = """
            {"version": 99, "connections": [{"scheme": "postgres", "host": "db.example",
             "port": "5432", "database": "sales", "user": "ana"}]}
            """

        guard
            let document = try? JSONDecoder().decode(
                SavedConnections.self, from: Data(jsonText.utf8))
        else {
            failures += 1
            fputs("connection FAIL: a document from a newer build is read, not refused\n", stderr)
            return
        }

        expect(document.connections.count, 0, "a document from a newer build holds nothing here")
    }

    /// What a row in the list says.
    ///
    /// Two lines are all there is to tell two connections apart with, and both are
    /// derived rather than stored, so every fallback here is a row somebody has to
    /// read: an unnamed connection, one that names no database, one that is a file,
    /// and one with nothing in it at all. The separators are checked with a part
    /// missing as well as present — punctuation with nothing behind it reads as a
    /// line that was cut off rather than as a field nobody filled in.
    private static func checkTitleAndSubtitle() {
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

    /// What the window has to ask before it throws an edit away.
    ///
    /// The order of the fields is asserted, not just their presence: the sentence is
    /// read once, by somebody deciding whether to lose what they typed, and it walks
    /// down the form so that it can be checked against the form. The password is
    /// asserted from the parameter alone, with every field equal — it is not in the
    /// value, it never leaves the Keychain, and the form is the only thing that knows
    /// whether the one on screen is the one that was saved.
    private static func checkUnsavedEdits() {
        let original = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))

        let identical = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        expect(
            original.unsavedEdits(against: identical, passwordChanged: false), nil,
            "a draft nobody has touched has nothing to go back")

        let changedHostPort = SavedConnection(
            name: "sales",
            settings: ConnectionSettings(
                scheme: "postgres", host: "new.example", port: "5433", database: "sales",
                user: "ana"))
        let moved = original.unsavedEdits(against: changedHostPort, passwordChanged: false)
        expect(moved?.fields, ["Host", "Port"], "the fields are named in the order the form has")
        expect(
            moved?.detail, "Host and Port would go back to what was saved.",
            "and the sentence names them rather than counting them")

        let retyped = original.unsavedEdits(against: identical, passwordChanged: true)
        expect(
            retyped?.fields, ["Password"],
            "a password that was retyped is an edit even when every field matches")
    }

    /// Where the connections are kept is also a statement about where they are not.
    ///
    /// `PreferencesChecks` asserts the same rule against the files on disk; this one
    /// asserts it against what `load` returns, which is the half a user notices — a
    /// copy left behind names a host and an account, and it would go on describing a
    /// decision that was changed.
    private static func checkStorageClearsOther() {
        guard let root = scratchDirectory() else { return }
        defer { try? FileManager.default.removeItem(at: root) }
        let directories = ConnectionDirectories(
            local: root.appending(path: "config"), cloud: root.appending(path: "drive"))

        let firstList = [
            SavedConnection(
                name: "first",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db1.example", port: "5432", database: "first",
                    user: "ana"))
        ]
        ConnectionStore.save(firstList, to: .thisMac, in: directories)

        let loadedFirst = ConnectionStore.load(from: .thisMac, in: directories)
        expect(loadedFirst.count, 1, "the list is on this Mac")

        let secondList = [
            SavedConnection(
                name: "second",
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db2.example", port: "5432", database: "second",
                    user: "bob"))
        ]
        ConnectionStore.save(secondList, to: .iCloud, in: directories)

        let loadedAfter = ConnectionStore.load(from: .thisMac, in: directories)
        expect(
            loadedAfter.count, 0,
            "and choosing iCloud takes it off this Mac")

        let loadedSecond = ConnectionStore.load(from: .iCloud, in: directories)
        expect(loadedSecond.count, 1, "leaving the one in iCloud Drive")
        expect(loadedSecond[0].name, "second", "which is the list that was saved there")
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
            fputs("connection FAIL: a scratch directory could not be made: \(error)\n", stderr)
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

    /// The two safety flags survive the file, and a draft carrying one is kept.
    ///
    /// Written because both failures are silent. A flag that does not round-trip
    /// leaves a production database unmarked on the next launch, which is the one
    /// moment the mark exists for; and a form somebody switched Read-only on in,
    /// having typed nothing else, would be thrown away as an untouched form.
    private static func checkSafetyFlags() {
        let guarded = SavedConnection(
            id: UUID(uuidString: "44444444-4444-4444-4444-444444444444")!,
            name: "ledger",
            color: .red,
            isReadOnly: true,
            isProduction: true,
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "ledger",
                user: "ana"))
        let back = SavedConnection.Raw(from: guarded).toSavedConnection()
        expect(back.isReadOnly, true, "read-only survives the file")
        expect(back.isProduction, true, "and so does the production mark")

        // An entry written before the flags existed is neither of them, rather
        // than a decode failure — one throw anywhere empties the whole list.
        let older = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u"}"#
        let decoded = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(older.utf8))
        expect(
            decoded?.production ?? true, false, "an entry from before the flags is not production")
        expect(decoded?.readOnly ?? true, false, "and not read-only either")

        guard let driver = DriverCatalog.first else { return }
        var draft = SavedConnection(settings: .suggested(for: driver))
        expect(ConnectionList.isWorthSaving(draft), false, "an untouched form is not worth saving")
        draft.isProduction = true
        expect(ConnectionList.isWorthSaving(draft), true, "marking one production is a decision")
    }

    /// What answered is kept, shown in front of the address, and is not an edit.
    ///
    /// The last of those is the one that would be found late and by accident: the
    /// record is written by connecting, so if it counted as an unsaved edit then
    /// opening a connection would leave its own form dirty, and every switch to
    /// another row would ask about changes nobody made.
    private static func checkServerRecord() {
        let reached = SavedConnection(
            id: UUID(uuidString: "55555555-5555-5555-5555-555555555555")!,
            server: "CockroachDB 23.1.11",
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let back = SavedConnection.Raw(from: reached).toSavedConnection()
        expect(back.server, "CockroachDB 23.1.11", "what answered survives the file")
        expect(
            back.subtitle, "CockroachDB 23.1.11 · ana@db.example:5432/sales",
            "and is read in front of the address")

        // The product is the whole point of recording it: this row and one
        // against real PostgreSQL are otherwise the same two lines.
        var untried = reached
        untried.server = ""
        expect(
            untried.subtitle, "ana@db.example:5432/sales",
            "a connection nothing has answered is the address alone")

        expect(
            reached.unsavedEdits(against: untried, passwordChanged: false) == nil, true,
            "and what answered is a record rather than an edit")

        // An entry written before this key existed decodes rather than throwing:
        // one throw anywhere empties the whole list.
        let older = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u"}"#
        let decoded = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(older.utf8))
        expect(decoded?.server ?? "missing", "", "an entry from before it has answered nothing")
    }

    /// A read-only connection refuses this application's own writes, says which
    /// mark refused them, and a production one does not refuse at all.
    ///
    /// The sentence is checked and not merely the Bool. A refusal that does not
    /// name the mark is indistinguishable from a bug: somebody whose grid has
    /// quietly stopped taking edits reports "Save does nothing", and the whole
    /// point of the mark is that they should instead be told they set it.
    ///
    /// The two flags being separate is checked here too, because collapsing them
    /// is the obvious simplification and it is wrong in both directions — a
    /// production connection that refused writes would be a connection nobody
    /// could use, and a read-only one that merely asked would be a promise
    /// broken by pressing Return.
    private static func checkWriteRefusal() {
        expect(ConnectionSafety().writeRefusal, nil, "an unmarked connection refuses nothing")
        expect(
            ConnectionSafety(isProduction: true).writeRefusal, nil,
            "and neither does a production one — production asks rather than refuses")

        let refusal = ConnectionSafety(isReadOnly: true).writeRefusal
        expect(refusal != nil, true, "a read-only connection refuses")
        expect(
            refusal?.contains("read-only") ?? false, true,
            "and the refusal names the mark that caused it")
        expect(
            refusal?.contains("connection form") ?? false, true,
            "and says where the mark can be cleared")

        // What a window carries is what it opened, which is what makes the
        // refusal survive somebody clearing the box afterwards.
        let opened = ConnectionSafety(
            of: SavedConnection(
                isReadOnly: true, isProduction: true,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                    user: "ana")))
        expect(opened.isReadOnly, true, "a session's safety is the entry it was opened from")
        expect(opened.isProduction, true, "both marks, not only the one that refuses")
    }

    /// The two SSL parameters are written for the driver that reads them, left
    /// off the drivers that do not, and survive the round trip through a URL.
    ///
    /// The round trip is the half that would be found late. `--conn` puts a
    /// string somebody could not connect with back into the form, and a form
    /// that dropped the SSL settings on the way in would show a connection it
    /// was not about to make — then write that weaker connection back out when
    /// they pressed Save.
    private static func checkSslParameters() {
        var pg = ConnectionSettings(
            scheme: "postgres", host: "db.example", port: "5432", database: "sales", user: "ana")

        // The default writes nothing. Every connection string this application
        // has ever produced has to keep its exact shape, and `prefer` is what
        // the driver does when nobody says otherwise anyway.
        expect(
            pg.connectionString(password: ""), "postgres://ana@db.example:5432/sales",
            "the default asks for nothing, so it writes nothing")

        pg.sslMode = .verifyFull
        expect(
            pg.connectionString(password: ""),
            "postgres://ana@db.example:5432/sales?sslmode=verify-full",
            "a mode that was chosen is written as libpq spells it")

        // Left out until it would mean something. A path carried under `require`
        // reads as a certificate being checked, which under `require` it is not.
        pg.sslMode = .require
        pg.sslRootCert = "/etc/ssl/ca.pem"
        expect(
            pg.connectionString(password: ""),
            "postgres://ana@db.example:5432/sales?sslmode=require",
            "a CA is not written for a mode that verifies nothing")

        pg.sslMode = .verifyCa
        expect(
            pg.connectionString(password: ""),
            "postgres://ana@db.example:5432/sales?sslmode=verify-ca&sslrootcert=/etc/ssl/ca.pem",
            "and is written for one that does")

        // The driver that does not read them is not handed them. This is the
        // check that fails if the catalogue's answer stops crossing the FFI.
        var sqlite = ConnectionSettings(scheme: "sqlite", path: "/tmp/notes.db")
        sqlite.sslMode = .verifyFull
        expect(
            sqlite.connectionString(password: ""), "sqlite:///tmp/notes.db",
            "a database that is a file on this disk is asked nothing about TLS")

        let back = ConnectionSettings(
            connectionString: "postgres://ana@db.example:5432/sales?sslmode=verify-ca"
                + "&sslrootcert=/etc/ssl/ca.pem")
        expect(back.sslMode, .verifyCa, "a URL's mode lands back in the form")
        expect(back.sslRootCert, "/etc/ssl/ca.pem", "and so does its CA")

        // A word this build does not offer must not empty the form. `allow` is
        // libpq's and is read as the neighbour the driver reads it as; a typo is
        // read the same way, because `--conn` exists to be corrected rather than
        // to be refused.
        expect(
            ConnectionSettings(connectionString: "postgres://h/d?sslmode=allow").sslMode, .prefer,
            "libpq's allow is read as the mode it behaves as")
        expect(
            ConnectionSettings(connectionString: "postgres://h/d?sslmode=verify_full").sslMode,
            .prefer, "and so is a word nothing has")

        // The host is still the host. A query on the end must not be read as
        // part of the database name.
        let noisy = ConnectionSettings(
            connectionString: "postgres://ana@db.example:5432/sales?sslmode=require")
        expect(noisy.database, "sales", "the database survives a query being on the end")
        expect(noisy.host, "db.example", "and so does the host")
    }

    /// The SSL settings survive the file, are edits when they change, and an
    /// entry written before they existed still loads.
    ///
    /// The last of those is the one that costs everything if it is wrong: one
    /// throw anywhere in the list empties the whole list, so an older
    /// `connections.json` meeting a decoder that insists on the new keys would
    /// take every saved connection with it.
    private static func checkSslSurvivesTheFile() {
        let strict = SavedConnection(
            id: UUID(uuidString: "66666666-6666-6666-6666-666666666666")!,
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana", sslMode: .verifyCa, sslRootCert: "/etc/ssl/ca.pem"))
        let back = SavedConnection.Raw(from: strict).toSavedConnection()
        expect(back.settings.sslMode, .verifyCa, "the mode survives the file")
        expect(back.settings.sslRootCert, "/etc/ssl/ca.pem", "and so does the CA")

        // Both are typed settings rather than records of what happened, so
        // changing either has to stop the form from being left silently.
        var relaxed = strict
        relaxed.settings.sslMode = .require
        expect(
            strict.unsavedEdits(against: relaxed, passwordChanged: false)?.fields, ["SSL"],
            "turning verification off is an unsaved edit, named")

        var moved = strict
        moved.settings.sslRootCert = "/etc/ssl/other.pem"
        expect(
            strict.unsavedEdits(against: moved, passwordChanged: false)?.fields, ["CA"],
            "and so is pointing at another certificate")

        // An entry from before either key existed connects the way it always
        // has, which is `prefer` — the driver's own default.
        let older = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u"}"#
        let decoded = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(older.utf8))
        expect(
            decoded?.toSavedConnection().settings.sslMode, .prefer,
            "an entry written before this connects the way it always did")
        expect(
            decoded?.toSavedConnection().settings.sslRootCert, "",
            "and names no certificate")
    }

    /// The absent key reads as `true`, and that is the whole of this check.
    ///
    /// Every entry in every existing `connections.json` is missing this key, so
    /// the default is not a detail of the format — it is what happens to
    /// everybody who updates. Read as `false` it would quietly stop using
    /// passwords those people had stored, and they would find out one connection
    /// at a time.
    private static func checkAnEntryWithoutTheKeyStillSavesItsPassword() {
        let older = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u"}"#
        let decoded = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(older.utf8))
        expect(decoded?.savesPassword ?? false, true, "an entry from before the key still saves")

        let declined = #"""
            {"scheme":"postgres","host":"h","port":"1","database":"d","user":"u",
             "savesPassword":false}
            """#
        let off = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(declined.utf8))
        expect(off?.savesPassword ?? true, false, "and one that declined is honoured")

        // Out and back, because the flag is only worth anything if it survives
        // the trip a saved file makes: written by `Raw(from:)` and read by
        // `toSavedConnection()`, with a mistake in either one silently restoring
        // the default.
        let kept = SavedConnection(
            name: "No secrets", savesPassword: false,
            settings: ConnectionSettings(
                scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                user: "ana"))
        let round = SavedConnection.Raw(from: kept).toSavedConnection()
        expect(round.savesPassword, false, "and survives being written out and read back")

        // An edit to it is an edit, so Save has something to do and the row is
        // marked. A flag that changed without saying so would be a decision about
        // a secret that nobody was told had been made.
        var draft = kept
        draft.savesPassword = true
        expect(
            kept.unsavedEdits(against: draft, passwordChanged: false) != nil, true,
            "and changing it is an unsaved edit")
    }

    /// A folder is a path on the entry, tidied on the way in and on the way out,
    /// and the list hands the connections over grouped by it.
    ///
    /// The tidying is the half that would be found by a user rather than by this:
    /// the file is meant to be hand-edited, so `"/clients/"` and `"clients"` will
    /// both appear in one, and a sidebar that drew two folders would be reporting
    /// its own parsing back as their mistake.
    private static func checkFoldersGroupTheList() {
        func made(_ name: String, folder: String) -> SavedConnection {
            SavedConnection(
                name: name, folder: folder,
                settings: ConnectionSettings(
                    scheme: "postgres", host: "db.example", port: "5432", database: "sales",
                    user: "ana"))
        }

        expect(made("a", folder: "/clients//acme/ ").folderPath, "clients/acme", "a path is tidied")
        expect(made("b", folder: "clients / acme").folderPath, "clients/acme", "spaces and all")
        expect(made("c", folder: "").folderPath, "", "and the top level stays empty")

        let list = ConnectionList([
            made("acme", folder: "clients/acme"),
            made("loose", folder: ""),
            made("bink", folder: "clients/bink"),
            made("second acme", folder: "/clients/acme")
        ])
        let groups = list.grouped("")
        expect(
            groups.map(\.path), ["", "clients/acme", "clients/bink"],
            "the top level comes first and the folders sort by path")
        expect(
            groups[1].connections.map(\.name), ["acme", "second acme"],
            "two spellings of one folder are one folder, in the file's own order")
        expect(groups[1].name, "acme", "and a header reads the folder's own name")
        expect(
            list.folders, ["clients/acme", "clients/bink"],
            "the folders that exist are the ones something is in")

        // A folder the filter empties is gone rather than left as a header over
        // nothing, which would say the folder is empty when what happened is that
        // the search did not match.
        expect(
            list.grouped("loose").map(\.path), [""],
            "a folder nothing matched is not drawn")

        // The file's round trip, which is where a key that is written but not read
        // would be lost without anything saying so.
        let back = SavedConnection.Raw(from: made("d", folder: " /clients/acme "))
            .toSavedConnection()
        expect(back.folder, "clients/acme", "the folder survives the file, tidied")

        let older = #"{"scheme":"postgres","host":"h","port":"1","database":"d","user":"u"}"#
        let decoded = try? JSONDecoder().decode(SavedConnection.Raw.self, from: Data(older.utf8))
        expect(
            decoded?.toSavedConnection().folder, "",
            "an entry written before folders existed is at the top level")

        // Moving a connection is an unsaved edit, named, or Save is a button
        // somebody presses and nothing happens.
        var moved = made("e", folder: "clients/acme")
        let original = moved
        moved.folder = "internal"
        expect(
            original.unsavedEdits(against: moved, passwordChanged: false)?.fields, ["Folder"],
            "moving a connection to another folder is an unsaved edit")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("connection FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }

    /// What the sidebar asks the list, and the answers the window is drawn from.
    ///
    /// Filtering, position and removal are the three of these a person can catch the
    /// application getting wrong — a row that vanishes, a row that jumps, a row that
    /// comes back — and none of them needs a window to check.
    private static func checkConnectionList() {
        let conn1 = SavedConnection(
            name: "MyDB",
            settings: ConnectionSettings(
                scheme: "postgres", host: "localhost", port: "5432", database: "test", user: "user")
        )
        let conn2 = SavedConnection(
            name: "AnotherDB",
            settings: ConnectionSettings(
                scheme: "postgres", host: "remote", port: "5432", database: "prod", user: "admin"))
        let list = ConnectionList([conn1, conn2])

        let matches = list.matching("MyDB")
        expect(matches.count, 1, "a filter matching one connection returns one connection")
        expect(matches[0].id, conn1.id, "the matching connection is returned")

        let matches2 = list.matching("localhost")
        expect(matches2.count, 1, "a filter matching subtitle returns one connection")
        expect(matches2[0].id, conn1.id, "the matching connection is returned")

        let matches3 = list.matching("MYDB")
        expect(matches3.count, 1, "a filter ignoring case returns one connection")

        let all = list.matching("")
        expect(all.count, 2, "a blank filter returns all connections")

        let all2 = list.matching("   ")
        expect(all2.count, 2, "a whitespace filter returns all connections")

        expect(list.index(of: conn1.id), 0, "index of first connection is 0")
        expect(list.index(of: conn2.id), 1, "index of second connection is 1")
        expect(list.index(of: UUID()), nil, "index of non-existent connection is nil")

        expect(
            list.connection(conn1.id)?.id, conn1.id, "connection lookup returns correct connection")
        expect(list.connection(UUID()), nil, "connection lookup of non-existent returns nil")

        let updatedConn1 = SavedConnection(
            id: conn1.id, name: "UpdatedDB", settings: conn1.settings)
        var mutableList = list
        mutableList.save(updatedConn1)
        expect(mutableList.connections[0].name, "UpdatedDB", "save replaces connection in place")
        expect(mutableList.connections[0].id, conn1.id, "save preserves connection id")
        expect(mutableList.connections.count, 2, "save maintains list count")

        let newConn = SavedConnection(
            name: "NewDB",
            settings: ConnectionSettings(
                scheme: "postgres", host: "new", port: "5432", database: "newdb", user: "newuser"))
        mutableList.save(newConn)
        expect(mutableList.connections.count, 3, "save appends new connection")
        expect(mutableList.connections[2].id, newConn.id, "save appends new connection at end")

        let removed = mutableList.remove(conn2.id)
        expect(removed?.id, conn2.id, "remove returns the removed connection")
        expect(mutableList.connections.count, 2, "remove decreases list count")
        expect(mutableList.connections.first?.id, conn1.id, "remove removes correct connection")

        let removedNil = mutableList.remove(UUID())
        expect(removedNil, nil, "remove of non-existent returns nil")
        expect(mutableList.connections.count, 2, "remove of non-existent doesn't change list")

        // From `suggested` rather than by writing those values out again: a check that
        // spelled the defaults itself would go on passing after the form began
        // offering different ones, which is the day this rule matters.
        guard let postgres = DriverCatalog.named("postgres") else {
            failures += 1
            fputs("connection FAIL: the catalog is missing a driver this check needs\n", stderr)
            return
        }
        let untouched = SavedConnection(settings: .suggested(for: postgres))
        expect(
            ConnectionList.isWorthSaving(untouched), false,
            "a form nobody has typed into is not a connection worth keeping")

        var named = untouched
        named.settings.database = "sales"
        expect(
            ConnectionList.isWorthSaving(named), true,
            "and one field of it is enough to make it one")

        var titled = untouched
        titled.name = "Production"
        expect(
            ConnectionList.isWorthSaving(titled), true,
            "as is a name on its own, from somebody part-way through")

        var coloured = untouched
        coloured.color = .red
        expect(
            ConnectionList.isWorthSaving(coloured), true,
            "and a colour, which nobody picks by accident")
    }
}
