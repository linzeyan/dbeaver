import Foundation

/// Executable checks for `ConnectionString`, run by `--verify-connection`.
///
/// A flag on the binary for the reason `SQLScriptChecks` gives: `Package.swift`
/// declares one executable target and it links the Rust staticlib, so a test
/// target would have to reproduce that link.
///
/// What is being defended is narrow and easy to get silently wrong. A password
/// holding a space is read by libpq as the end of the value and the start of a
/// keyword, so an unquoted one connects as a prefix of itself and the error
/// says only "password authentication failed". Nothing on screen would ever
/// point at the quoting.
enum ConnectionChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkPlainValues()
        checkAwkwardPasswords()
        checkFormRoundTrip()
        checkParsingWhatOthersWrite()
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
            settings("db.example.com", "5432", "shop", "reader").connectionString(
                password: "s3cr3t"),
            "host=db.example.com port=5432 dbname=shop user=reader password=s3cr3t",
            "an ordinary connection needs no quoting at all")
        expect(
            settings("127.0.0.1", "5432", "bench", "bench").connectionString(password: ""),
            "host=127.0.0.1 port=5432 dbname=bench user=bench",
            "an empty password is left out rather than sent as an empty one")
        expect(
            settings(" 127.0.0.1 ", "5432", "bench", "bench").connectionString(password: " p "),
            "host=127.0.0.1 port=5432 dbname=bench user=bench password=' p '",
            "the fields are trimmed and the password is not")
    }

    private static func checkAwkwardPasswords() {
        for password in ["two words", "quote'inside", "back\\slash", "tab\there", "'"] {
            let text = settings("h", "1", "d", "u").connectionString(password: password)
            expect(
                ConnectionString.parse(text)["password"], password,
                "a password containing \(password.debugDescription) survives the round trip")
            expect(
                ConnectionString.parse(text)["dbname"], "d",
                "and does not swallow the keyword after it")
        }
    }

    private static func checkFormRoundTrip() {
        let original = settings("db.example.com", "6543", "shop", "read only")
        let parsed = ConnectionSettings(
            connectionString: original.connectionString(password: "pw"))
        expect(parsed, original, "the form's own string reads back into the same form")

        expect(
            settings("h", "1", "d", "u").isComplete, true, "four filled fields are complete")
        expect(
            settings("h", "1", "", "u").isComplete, false, "a missing database is not")
        expect(
            settings("h", "1", "d", "   ").isComplete, false, "nor is a field holding only space")
    }

    /// `--conn` is written by people and by the Makefile, so the parser has to
    /// read forms this never emits.
    private static func checkParsingWhatOthersWrite() {
        let text = "host=127.0.0.1   port=55432\tuser=bench password='b e n c h' dbname=bench"
        let pairs = ConnectionString.parse(text)
        expect(pairs["host"], "127.0.0.1", "runs of whitespace between pairs are separators")
        expect(pairs["password"], "b e n c h", "a quoted value keeps its spaces")
        expect(pairs["dbname"], "bench", "and the pair after it is still found")

        expect(
            ConnectionString.parse("host = localhost")["host"], "localhost",
            "space around the equals sign is allowed, as libpq allows it")
        expect(
            ConnectionString.parse("dbname=a dbname=b")["dbname"], "b",
            "a repeated keyword takes its last value, as libpq does")
        expect(
            ConnectionString.parse("host=a sslmode")["host"], "a",
            "a trailing keyword with no value ends the parse rather than inventing one")
        expect(ConnectionString.parse("").isEmpty, true, "an empty string holds no pairs")

        // What the form shows for a string that named nothing useful: empty
        // fields, not fields holding a fragment of the flag.
        let empty = ConnectionSettings(connectionString: "sslmode=require")
        expect(empty.host, "", "an unrelated keyword leaves the host empty")
        expect(empty.isComplete, false, "and the form knows there is nothing to try")
    }

    // MARK: - Harness

    private static func settings(
        _ host: String, _ port: String, _ database: String, _ user: String
    ) -> ConnectionSettings {
        ConnectionSettings(host: host, port: port, database: database, user: user)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("connection FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
