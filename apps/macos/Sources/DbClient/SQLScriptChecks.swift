import Foundation

/// Executable checks for `SQLScript`, run by `--verify-splitter`.
///
/// What the scanner promises about SQL — where a literal ends, which words are
/// keywords, what separates two statements — is settled in
/// `crates/sql/tests/scanning.rs` and is not restated here. Two copies of a rule
/// are two rules the moment one of them is corrected.
///
/// What is left is this side's own, and it is all about the seam. The core
/// counts characters and AppKit counts UTF-16 units and a `String.Index` is
/// neither; the token kinds cross as numbers that no compiler on either side
/// checks; and the whole answer arrives through a C string that has to be
/// decoded and released. Every check below fails for something that could go
/// wrong between the two languages rather than inside either.
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
        checkSpansArriveAsSpans()
        checkTokenKindsSurviveTheCrossing()
        checkTokensCoverTheBuffer()
        checkBracketsPairAcrossTheBuffer()
        checkTargetsArriveWithTheirOrigin()
        checkErrorPositionsCrossBack()
        checkTheSchemeReachesTheDialect()
        checkOffsetsAreCountedInScalars()
        if failures == 0 {
            fputs("splitter: all checks passed\n", stderr)
        } else {
            fputs("splitter: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The statement spans are offsets into the buffer, and cutting the buffer
    /// with them has to give back the statements.
    ///
    /// The rule being exercised is the core's and is tested there; what is
    /// tested here is that a pair of numbers survives JSON, a C string and
    /// `String.Index` arithmetic and still names the same characters. An
    /// off-by-one anywhere on that path sends the server a statement missing its
    /// first letter.
    private static func checkSpansArriveAsSpans() {
        expect(split(""), [], "an empty buffer scans to nothing")
        expect(
            split("SELECT 1;\n\nSELECT 2;\n"), ["SELECT 1", "SELECT 2"],
            "the spans cut the buffer into the statements they name")
        expect(
            split("SELECT $fn$a;b$fn$; SELECT 2"), ["SELECT $fn$a;b$fn$", "SELECT 2"],
            "a span reaches over a body holding its own semicolons")
    }

    /// Each of the six kinds the editor paints arrives as itself.
    ///
    /// The kinds cross as the numbers `dbffi.h` documents, and nothing on either
    /// side of that boundary is checked by a compiler: renumbering them would
    /// paint every string literal as a comment and produce no diagnostic at all.
    /// One script holding all six is what turns that into a failure here.
    private static func checkTokenKindsSurviveTheCrossing() {
        expect(
            tokens("SELECT 'lit', \"name\", $tag$body$tag$, 12.5 -- note"),
            [
                "keyword:SELECT", "string:'lit'", "quotedIdentifier:\"name\"",
                "dollarQuoted:$tag$body$tag$", "number:12.5", "comment:-- note"
            ],
            "every kind the editor colours arrives as the kind it is")
        expect(
            tokens("SELECT id FROM t"), ["keyword:SELECT", "keyword:FROM"],
            "the kinds the editor leaves alone arrive as nothing to paint")
    }

    /// Structural invariants the highlighter relies on.
    ///
    /// It binary-searches the painted list by end offset and intersects each
    /// range with the viewport, and `tokenRange` binary-searches the full list
    /// expecting it to leave no character uncovered. Both are nonsense if what
    /// arrived is out of order, overlapping, or pointing outside the buffer —
    /// which is what a payload decoded one field out of step looks like.
    private static func checkTokensCoverTheBuffer() {
        let script = """
            -- a note
            SELECT "c1", 'lit''eral', 12.5, $tag$body ; $$ still$tag$, E'esc\\'d'
              FROM t /* mid */ WHERE x = $1 AND y IN (1_000, 0xFF);
            """
        let scan = scanned(script)
        let scalars = script.unicodeScalars.count
        expect(scan.tokens.isEmpty, false, "the script has tokens to check")
        expect(
            scan.tokens.allSatisfy { $0.range.lowerBound >= 0 && $0.range.upperBound <= scalars },
            true, "every painted token lies inside the buffer")
        expect(
            zip(scan.tokens, scan.tokens.dropFirst()).allSatisfy {
                $0.range.upperBound <= $1.range.lowerBound
            }, true, "painted tokens arrive in order and never overlap")
        expect(
            scan.spans.first?.lowerBound, 0, "the spans start at the first character")
        expect(scan.spans.last?.upperBound, scalars, "and reach the last")
        expect(
            zip(scan.spans, scan.spans.dropFirst()).allSatisfy {
                $0.upperBound == $1.lowerBound
            }, true, "with no gap between them, which is what tokenRange walks")
    }

    /// The pair of parentheses the caret is beside, as the offsets that name
    /// them — or nothing where there is no pair to name.
    ///
    /// The pairing rule is this side's own: the core hands back the tokens and
    /// says nothing about which paren is whose, so a wrong answer here is a
    /// mark on the wrong character, which is worse than no mark. The last case
    /// is the one that fails if the scan counts `Character`s instead of
    /// scalars: the flag is two scalars and one Character, and every offset
    /// after it lands one place to the left.
    private static func checkBracketsPairAcrossTheBuffer() {
        let script = "SELECT (1 + (2))"
        expect(
            brackets(script, caret: 8), "7,15",
            "a caret just after the outer ( pairs with the last )")
        expect(
            brackets(script, caret: 12), "12,14",
            "a caret on the inner pair gets the inner one")
        expect(
            brackets(script, caret: 14), "12,14",
            "and the same from the closing end")
        expect(
            brackets(script, caret: 0), nil,
            "a caret at neither end of any parenthesis gets nothing")
        expect(
            brackets(script, caret: 9), nil,
            "and neither does one between the two pairs")

        let quoted = "SELECT ')'"
        expect(brackets(quoted, caret: 8), nil, "a paren inside a string is not a paren")
        expect(
            brackets(quoted, caret: 9), nil, "and neither is the caret just after it")

        let unbalanced = "SELECT (1"
        expect(
            brackets(unbalanced, caret: 7), nil,
            "an unbalanced buffer gets nothing rather than a guess")
        expect(
            brackets(unbalanced, caret: 8), nil,
            "and neither does the caret just after the lone (")

        let wide = "🇹🇼 SELECT (1)"
        expect(
            brackets(wide, caret: 11), "10,12",
            "a multi-scalar character before the parens does not move them")
    }

    /// A target arrives with the origin it was given, and says so on screen.
    ///
    /// The origin crosses as a name and two numbers and comes back as an enum
    /// with an associated value; the sentence the status bar reads is built from
    /// it here. A buffer of five statements running under a label saying "query"
    /// is the defect this prevents.
    private static func checkTargetsArriveWithTheirOrigin() {
        let script = "SELECT 1;\nSELECT 22;\nSELECT 333;"
        expect(target(script, caret: 14), "SELECT 22|statement 2 of 3", "one statement of several")
        expect(
            target("SELECT 1", caret: 0), "SELECT 1|query",
            "one statement is still described as the query it always was")
        expect(
            target(script, from: 10, to: 19), "SELECT 22|selection",
            "a selection is described as one")
        expect(target("-- nothing to run", caret: 0), nil, "nothing to run has no target")
    }

    /// A server's error position comes back as a buffer offset, or as nothing.
    ///
    /// The arithmetic is the core's; what is checked here is that -1 for "that
    /// number cannot have come from this statement" becomes nil rather than an
    /// offset the editor would then try to select.
    private static func checkErrorPositionsCrossBack() {
        let second = 10..<19
        expect(SQLScript.errorOffset(ofPosition: 1, in: second), 10, "position 1 is the first char")
        expect(
            SQLScript.errorOffset(ofPosition: 10, in: second), 19,
            "one past the last character is where an unexpected end of input points")
        expect(SQLScript.errorOffset(ofPosition: 11, in: second), nil, "beyond that is not")
        expect(SQLScript.errorOffset(ofPosition: 0, in: second), nil, "the server never says 0")
    }

    /// The connection's scheme reaches the dialect table.
    ///
    /// Which database is on the other end changes what the same characters mean,
    /// and the editor is the only thing that knows it. A scheme dropped anywhere
    /// between the window and the core would leave every connection reading its
    /// buffer as PostgreSQL — correct-looking against PostgreSQL, and wrong
    /// everywhere else.
    private static func checkTheSchemeReachesTheDialect() {
        expect(
            tokens("SELECT \"a\"", scheme: "postgres"),
            ["keyword:SELECT", "quotedIdentifier:\"a\""],
            "a double quote opens an identifier in PostgreSQL")
        expect(
            tokens("SELECT \"a\"", scheme: "mysql"), ["keyword:SELECT", "string:\"a\""],
            "and a string in MySQL, which is the same characters read differently")
        expect(
            split("SELECT 1 # note; SELECT 2", scheme: "mysql"), ["SELECT 1 # note; SELECT 2"],
            "a hash comment hides a semicolon where the database has hash comments")
    }

    /// Offsets are Unicode scalars on both sides of every conversion.
    ///
    /// A flag is two scalars and one `Character`, and AppKit counts UTF-16 units
    /// besides. Counting `Character`s anywhere on the path would put every
    /// offset after such a literal one place to the left — the caret on the
    /// wrong letter, and every colour after it shifted with it. This is the
    /// check that is entirely Swift's own: the core cannot get it wrong and
    /// cannot see it go wrong.
    private static func checkOffsetsAreCountedInScalars() {
        let wide = "SELECT '🇹🇼';\nSELECT nope"
        expect(
            tokens(wide), ["keyword:SELECT", "string:'🇹🇼'", "keyword:SELECT"],
            "a multi-scalar character inside a literal does not move what follows it")
        expect(
            SQLScript.text(scanned(wide).tokenRange(at: 20), in: wide), "nope",
            "an offset past it still lands on the right word")
        expect(SQLScript.lineColumn(of: 20, in: wide).line, 2, "line counted from 1")
        expect(SQLScript.lineColumn(of: 20, in: wide).column, 8, "column counted from 1")

        let plain = "SELECT 1;\nSELECT nope;"
        expect(
            SQLScript.text(scanned(plain).tokenRange(at: 7), in: plain), "1",
            "a number is a word too")
        expect(
            SQLScript.text(scanned(plain).tokenRange(at: 6), in: plain), " ",
            "punctuation gets a character, not an invisible empty selection")
        expect(
            SQLScript.text(scanned(plain).tokenRange(at: 99), in: plain), "",
            "an offset the buffer cannot hold selects nothing")
    }

    // MARK: - Harness

    /// PostgreSQL unless a case is about a difference between databases, which
    /// is what the editor's own default amounts to.
    private static func scanned(_ script: String, scheme: String = "postgres") -> SQLScript.Scan {
        SQLScript.scan(script, scheme: scheme, selection: 0..<0)
    }

    private static func split(_ script: String, scheme: String = "postgres") -> [String] {
        scanned(script, scheme: scheme).statements.map { SQLScript.text($0, in: script) }
    }

    /// Tokens rendered as `kind:text`, so one comparison covers both halves and
    /// a failure prints something readable.
    private static func tokens(_ script: String, scheme: String = "postgres") -> [String] {
        scanned(script, scheme: scheme).tokens.map {
            "\($0.kind):\(SQLScript.text($0.range, in: script))"
        }
    }

    /// A pair of parentheses rendered as `opening,closing`, so one comparison
    /// covers both halves and a failure prints something readable.
    private static func brackets(_ script: String, caret: Int) -> String? {
        guard let (opening, closing) = scanned(script).brackets(atCaret: caret, in: script)
        else { return nil }
        return "\(opening),\(closing)"
    }

    /// A target rendered as `sql|label`, so one comparison covers both halves.
    private static func target(_ script: String, caret: Int) -> String? {
        target(script, from: caret, to: caret)
    }

    private static func target(_ script: String, from: Int, to: Int) -> String? {
        guard
            let t = SQLScript.scan(script, scheme: "postgres", selection: from..<to).target
        else { return nil }
        return "\(SQLScript.text(t.range, in: script))|\(t.label)"
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("splitter FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
