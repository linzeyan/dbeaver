import Foundation

/// Executable checks for `SQLScript`, run by `--verify-splitter`.
///
/// There is no Swift test target and adding one is disruptive: `Package.swift`
/// declares a single executable target that links the Rust staticlib, so a test
/// target would have to reproduce that link. A flag on the binary is how this
/// project has answered that before — `--bench`, `--verify`, `--tab`, `--sql`,
/// `--export` all exist because the thing they exercise is otherwise only
/// reachable by hand.
///
/// Failures print to stderr, not stdout: stdout is block-buffered and a process
/// that ends in `exit` loses whatever is still sitting in the buffer, which is
/// exactly the output worth having.
enum SQLScriptChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkPlainSplitting()
        checkStringsAndIdentifiers()
        checkComments()
        checkDollarQuoting()
        checkSeedScript()
        checkCaretPicksAStatement()
        checkSelectionWins()
        checkErrorPositions()
        checkUnterminatedInput()
        checkTokenKinds()
        checkTokensCoverTheBuffer()
        if failures == 0 {
            fputs("splitter: all checks passed\n", stderr)
        } else {
            fputs("splitter: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    private static func checkPlainSplitting() {
        expect(split(""), [], "an empty buffer holds no statements")
        expect(split("   \n\t "), [], "blank space holds no statements")
        expect(split("SELECT 1"), ["SELECT 1"], "a statement needs no terminator")
        expect(split("SELECT 1;"), ["SELECT 1"], "the terminator is not part of it")
        expect(split("SELECT 1;;;"), ["SELECT 1"], "empty statements are not statements")
        expect(
            split("SELECT 1;\n\nSELECT 2;\n"), ["SELECT 1", "SELECT 2"],
            "blank lines between statements are trimmed off both")
        expect(
            split("SELECT 1;SELECT 2"), ["SELECT 1", "SELECT 2"],
            "a terminator needs no whitespace around it")
    }

    private static func checkStringsAndIdentifiers() {
        expect(
            split("SELECT 'a;b';"), ["SELECT 'a;b'"],
            "a semicolon in a literal is a character, not a boundary")
        expect(
            split("SELECT 'it''s; fine';"), ["SELECT 'it''s; fine'"],
            "a doubled quote does not end the literal")
        expect(
            split("SELECT \"a;b\" FROM t;"), ["SELECT \"a;b\" FROM t"],
            "a semicolon in a quoted identifier is part of the name")
        expect(
            split("SELECT \"say \"\"hi\"\"; ok\";"), ["SELECT \"say \"\"hi\"\"; ok\""],
            "a doubled quote does not end the identifier either")
        // standard_conforming_strings is on, so this is a literal ending in a
        // backslash followed by a second literal — not one escaped quote.
        expect(
            split("SELECT 'a\\', 'b;c';"), ["SELECT 'a\\', 'b;c'"],
            "a backslash is an ordinary character in a plain literal")
        expect(
            split("SELECT E'a\\'; b';"), ["SELECT E'a\\'; b'"],
            "a backslash does escape inside E'…'")
        expect(
            split("SELECT emailE'a\\'; SELECT 2"), ["SELECT emailE'a\\'", "SELECT 2"],
            "the E ending an identifier does not make the next literal an escape string")
    }

    private static func checkComments() {
        expect(
            split("SELECT 1; -- and ; then\nSELECT 2;"), ["SELECT 1", "-- and ; then\nSELECT 2"],
            "a semicolon in a line comment is not a boundary")
        expect(
            split("SELECT 1 /* ; still one */ + 1;"), ["SELECT 1 /* ; still one */ + 1"],
            "nor one in a block comment")
        expect(
            split("SELECT /* outer /* inner ; */ still ; */ 1;"),
            ["SELECT /* outer /* inner ; */ still ; */ 1"],
            "block comments nest, so the first */ does not close the outer one")
        expect(
            split("SELECT 1;\n-- trailing note\n"), ["SELECT 1"],
            "a chunk holding only a comment is not a statement")
        expect(
            split("-- fetch the rows\nSELECT 1;"), ["-- fetch the rows\nSELECT 1"],
            "a leading comment stays with the statement it describes")
    }

    private static func checkDollarQuoting() {
        expect(
            split("SELECT $$a;b$$;"), ["SELECT $$a;b$$"],
            "an untagged dollar-quoted body hides its semicolons")
        expect(
            split("SELECT $tag$a;$$b$tag$;"), ["SELECT $tag$a;$$b$tag$"],
            "only the matching tag closes the body")
        expect(
            split("SELECT $1; SELECT $2"), ["SELECT $1", "SELECT $2"],
            "$1 is a parameter placeholder, not the start of a body")
        expect(
            split("SELECT a$b$c; SELECT 2"), ["SELECT a$b$c", "SELECT 2"],
            "$ continues an identifier, so a$b$c is one name")
    }

    /// The function body from `tools/seed-bench-db.sh`, which is where this
    /// whole file comes from: two semicolons inside `$fn$ … $fn$`, and a third
    /// that really does end the statement.
    private static func checkSeedScript() {
        let script = """
            CREATE OR REPLACE FUNCTION bench_child_touch() RETURNS trigger AS $fn$
            BEGIN
              RETURN NEW;
            END;
            $fn$ LANGUAGE plpgsql;

            CREATE TRIGGER bench_child_before_write
              BEFORE INSERT OR UPDATE ON bench_child
              FOR EACH ROW EXECUTE FUNCTION bench_child_touch();
            """
        let parts = split(script)
        expect(parts.count, 2, "the seed's function and trigger are two statements")
        expect(
            parts.first?.hasSuffix("$fn$ LANGUAGE plpgsql"), true,
            "the function body's own semicolons do not end it")
        expect(
            parts.last?.hasPrefix("CREATE TRIGGER"), true,
            "the trigger is what follows it")
    }

    private static func checkCaretPicksAStatement() {
        let script = "SELECT 1;\nSELECT 22;\nSELECT 333;"
        // Offsets: statement 1 at 0..<8, 2 at 10..<19, 3 at 21..<31.
        expect(target(script, caret: 0), "SELECT 1|statement 1 of 3", "caret at the very start")
        expect(target(script, caret: 8), "SELECT 1|statement 1 of 3", "caret at the end of one")
        expect(
            target(script, caret: 9), "SELECT 1|statement 1 of 3",
            "caret past the terminator is still on that line")
        expect(target(script, caret: 14), "SELECT 22|statement 2 of 3", "caret inside the second")
        expect(target(script, caret: 31), "SELECT 333|statement 3 of 3", "caret at the buffer end")

        expect(
            target("SELECT 1", caret: 0), "SELECT 1|query",
            "one statement is still described as the query it always was")
        expect(
            target("  \n SELECT 1;", caret: 1), "SELECT 1|query",
            "a caret in the leading blank space means the first statement")
        expect(SQLScript.target(in: "-- nothing to run", selection: 0..<0), nil, "no statement")

        // A comment above a statement is part of it, so the caret sitting on the
        // comment runs the statement it introduces rather than the one above.
        let annotated = "SELECT 1;\n-- second\nSELECT 2;"
        expect(
            target(annotated, caret: 12), "-- second\nSELECT 2|statement 2 of 2",
            "caret on a comment")
    }

    private static func checkSelectionWins() {
        let script = "SELECT 1;\nSELECT 22;"
        expect(
            target(script, from: 10, to: 19), "SELECT 22|selection",
            "a selection is what runs, whatever the caret rule would have said")
        expect(
            target(script, from: 9, to: 20), "SELECT 22;|selection",
            "a selection is taken as written, trimmed only of blank space")
        expect(
            SQLScript.target(in: script, selection: 9..<10), nil,
            "a selection holding nothing but blank space is not something to run")
    }

    private static func checkErrorPositions() {
        // The trap this exists for: the server counts from 1 within the
        // statement it was sent, and the second statement of a buffer does not
        // start at the buffer's first character.
        let second = 10..<19
        expect(SQLScript.errorOffset(ofPosition: 1, in: second), 10, "position 1 is the first char")
        expect(SQLScript.errorOffset(ofPosition: 8, in: second), 17, "position 8 is eight in")
        expect(
            SQLScript.errorOffset(ofPosition: 10, in: second), 19,
            "one past the last character is where an unexpected end of input points")
        expect(SQLScript.errorOffset(ofPosition: 11, in: second), nil, "beyond that is not")
        expect(SQLScript.errorOffset(ofPosition: 0, in: second), nil, "the server never says 0")

        let script = "SELECT 1;\nSELECT nope;"
        expect(
            SQLScript.lineColumn(of: 17, in: script).line, 2, "line counted from 1")
        expect(
            SQLScript.lineColumn(of: 17, in: script).column, 8, "column counted from 1")
        expect(
            SQLScript.text(SQLScript.tokenRange(at: 17, in: script), in: script), "nope",
            "the selection covers the word the server is pointing at")
        expect(
            SQLScript.text(SQLScript.tokenRange(at: 7, in: script), in: script), "1",
            "a number is a word too")
        expect(
            SQLScript.text(SQLScript.tokenRange(at: 6, in: script), in: script), " ",
            "punctuation gets a single character, not an invisible empty selection")

        // A flag is two code points and one Character. PostgreSQL counts code
        // points, so counting Characters here would put every offset after the
        // literal one place to the left — and the caret on the wrong letter.
        let wide = "SELECT '🇹🇼';\nSELECT nope"
        expect(
            SQLScript.text(SQLScript.tokenRange(at: 20, in: wide), in: wide), "nope",
            "offsets past a multi-scalar character still land on the right word")
        expect(SQLScript.lineColumn(of: 20, in: wide).line, 2, "line after the literal")
        expect(SQLScript.lineColumn(of: 20, in: wide).column, 8, "column after the literal")
    }

    private static func checkUnterminatedInput() {
        // Half-typed input is the normal state of an editor. Each of these
        // swallows the rest of the buffer on purpose: the alternative is
        // treating a semicolon inside a half-written literal as a boundary and
        // running the fragment before it.
        expect(
            split("SELECT 'a;b"), ["SELECT 'a;b"], "an unclosed literal runs to the end")
        expect(
            split("SELECT $$a;b"), ["SELECT $$a;b"], "so does an unclosed dollar body")
        expect(
            split("SELECT /* a;b"), ["SELECT /* a;b"], "and an unclosed block comment")
    }

    // MARK: - Tokens

    /// The colours are the same walk as the split, so these cases are as much
    /// about the scanner as the ones above. What is new here is the word list
    /// and the number rules, which the splitter never had an opinion about.
    private static func checkTokenKinds() {
        expect(
            tokens("select ID From t"), ["keyword:select", "keyword:From"],
            "keywords are matched whatever their case, and a table name is not one")
        expect(
            tokens("SELECT name, value, level FROM config"),
            ["keyword:SELECT", "keyword:FROM"],
            "the unreserved words that are ordinary column names are left alone")
        expect(
            tokens("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING"),
            [
                "keyword:INSERT", "keyword:INTO", "keyword:VALUES", "number:1", "keyword:ON",
                "keyword:CONFLICT", "keyword:DO", "keyword:NOTHING"
            ],
            "an unreserved command word is still a keyword")
        expect(
            tokens("SELECT PRIMARY KEY, uuid, date"),
            ["keyword:SELECT", "keyword:PRIMARY", "keyword:KEY", "keyword:uuid", "keyword:date"],
            "a type the grammar does not name is coloured like one that it does")

        expect(
            tokens("SELECT 'it''s', E'a\\'b', \"odd name\""),
            [
                "keyword:SELECT", "string:'it''s'", "string:E'a\\'b'",
                "quotedIdentifier:\"odd name\""
            ],
            "the E of an escape string belongs to the literal it opens")
        expect(
            tokens("SELECT emailE'x'"), ["keyword:SELECT", "string:'x'"],
            "an E that ends an identifier does not, and the identifier is no keyword")

        expect(
            tokens("SELECT 1, 1.5, .5, 1.5e-3, 1_000, 0xFF, 0b1010"),
            [
                "keyword:SELECT", "number:1", "number:1.5", "number:.5", "number:1.5e-3",
                "number:1_000", "number:0xFF", "number:0b1010"
            ],
            "every literal form the server accepts is one number")
        expect(
            tokens("SELECT col1, $1, a.b, 1e"), ["keyword:SELECT", "number:1"],
            "a digit inside a name, a parameter placeholder and a bare e are not numbers")

        expect(
            tokens("-- one\n/* two /* three */ */ SELECT 1"),
            ["comment:-- one", "comment:/* two /* three */ */", "keyword:SELECT", "number:1"],
            "a nested block comment is one comment")

        expect(
            tokens("SELECT $fn$BEGIN; 'x' END;$fn$ AS body"),
            ["keyword:SELECT", "dollarQuoted:$fn$BEGIN; 'x' END;$fn$", "keyword:AS"],
            "a dollar-quoted body is one token, delimiters and all")
        expect(
            tokens("SELECT a$b$c"), ["keyword:SELECT"],
            "$ continues an identifier, so a$b$c is one name and no dollar-quoted body")

        expect(
            tokens("SELECT 'a;b"), ["keyword:SELECT", "string:'a;b"],
            "an unclosed literal is coloured to the end of the buffer, as it is split to it")

        // The same trap as the error positions above, from the other side.
        // Tokens are counted in scalars and painted in UTF-16 units, so a
        // literal holding a flag would shift every colour after it if either
        // side counted Characters.
        let wide = "SELECT '🇹🇼', 1"
        expect(
            tokens(wide), ["keyword:SELECT", "string:'🇹🇼'", "number:1"],
            "a multi-scalar character inside a literal does not move what follows it")
    }

    /// Structural invariants the highlighter relies on: it binary-searches the
    /// token list by end offset and intersects each range with the viewport, and
    /// both are nonsense if the list is out of order, overlapping, or pointing
    /// outside the buffer.
    private static func checkTokensCoverTheBuffer() {
        let script = """
            -- a note
            SELECT "c1", 'lit''eral', 12.5, $tag$body ; $$ still$tag$, E'esc\\'d'
              FROM t /* mid */ WHERE x = $1 AND y IN (1_000, 0xFF);
            """
        let all = SQLScript.tokens(in: script)
        let scalars = script.unicodeScalars.count
        expect(all.isEmpty, false, "the script has tokens to check")
        expect(
            all.allSatisfy { $0.range.lowerBound >= 0 && $0.range.upperBound <= scalars }, true,
            "every token lies inside the buffer")
        expect(
            zip(all, all.dropFirst()).allSatisfy { $0.range.upperBound <= $1.range.lowerBound },
            true,
            "tokens arrive in order and never overlap")
    }

    // MARK: - Harness

    private static func split(_ script: String) -> [String] {
        SQLScript.statements(in: script).map { SQLScript.text($0, in: script) }
    }

    /// Tokens rendered as `kind:text`, so one comparison covers both halves and
    /// a failure prints something readable.
    private static func tokens(_ script: String) -> [String] {
        SQLScript.tokens(in: script).map { "\($0.kind):\(SQLScript.text($0.range, in: script))" }
    }

    /// A target rendered as `sql|label`, so one comparison covers both halves.
    private static func target(_ script: String, caret: Int) -> String? {
        target(script, from: caret, to: caret)
    }

    private static func target(_ script: String, from: Int, to: Int) -> String? {
        guard let t = SQLScript.target(in: script, selection: from..<to) else { return nil }
        return "\(SQLScript.text(t.range, in: script))|\(t.label)"
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("splitter FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
