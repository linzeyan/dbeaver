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

    // MARK: - Harness

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
