import Foundation

/// Executable checks for the MCP server's pure half, run by `--verify-mcp`.
///
/// The HTTP parser and the read-only guard are the two places where a mistake
/// is not a bug but a hole: a request read wrong is a request from anywhere,
/// and a statement read wrong is a write let through. Both are pure functions
/// precisely so that everything below can hold them without a socket or a
/// server — the reference implementation kept its dispatch untestable and its
/// own test file says so.
///
/// The guard checks are an attack corpus first and a spec second. Where the
/// guard is deliberately conservative, the false positive is pinned as
/// deliberate — the day one of those rejections becomes an allowance,
/// somebody should have to delete the sentence saying why it rejected.
enum MCPChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkARequestIsWhatItsBytesSay()
        checkAnIncompleteRequestWaitsAndAMalformedOneDoesNot()
        checkTheRouteAnswersMethodAndPathTogether()
        checkOnlyThisMachineCanBeAnOrigin()
        checkTheBearerTokenGatesInWholeBytes()
        checkPlainReadsPass()
        checkWhatIsInsideQuotesIsData()
        checkWritesAndStateChangesAreRefused()
        checkCommentTricksHideNothing()
        checkTheConservativeRejectionsAreDeliberate()
        if failures == 0 {
            fputs("mcp: all checks passed\n", stderr)
        } else {
            fputs("mcp: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - HTTP

    /// Method, path, query, headers and body all come out of one buffer, and
    /// the details are the ones clients actually exercise: header names in
    /// any case, values holding colons of their own, no space after the
    /// colon, percent-encoded query values.
    private static func checkARequestIsWhatItsBytesSay() {
        let raw =
            "POST /mcp?name=a%20b&flag HTTP/1.1\r\n"
            + "Content-Type: application/json\r\n"
            + "MCP-Session-ID:a:b:c\r\n"
            + "Content-Length: 4\r\n\r\n{\"x\"extra"
        guard case .request(let request) = MCPHTTP.parse(Data(raw.utf8)) else {
            failures += 1
            fputs("mcp FAIL: a whole request did not parse\n", stderr)
            return
        }
        expect(request.method, "POST", "the method is the first word")
        expect(request.path, "/mcp", "the path stops at the query string")
        expect(request.query["name"], "a b", "query values are percent-decoded")
        expect(request.query["flag"], "", "a bare query name is present and empty")
        expect(
            request.headers["mcp-session-id"], "a:b:c",
            "header names lowercase; values keep their colons")
        expect(
            request.headers["content-type"], "application/json",
            "a header with a space after the colon reads the same")
        expect(
            String(decoding: request.body, as: UTF8.self), "{\"x\"",
            "the body is Content-Length bytes, no more")
    }

    /// The two failure kinds are different answers: incomplete means keep
    /// reading, malformed means stop. A parser that says nil for both leaves
    /// a connection waiting for bytes that can never make it whole.
    private static func checkAnIncompleteRequestWaitsAndAMalformedOneDoesNot() {
        expect(
            MCPHTTP.parse(Data("POST /mcp HTTP/1.1\r\nContent-Le".utf8)),
            .incomplete, "headers still arriving")
        expect(
            MCPHTTP.parse(Data("POST /mcp HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}".utf8)),
            .incomplete, "a body still arriving")
        expect(
            MCPHTTP.parse(Data("nonsense\r\n\r\n".utf8)),
            .malformed, "a request line that is not one")
        expect(
            MCPHTTP.parse(
                Data("POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".utf8)),
            .malformed, "chunked bodies are refused, not guessed at")
        expect(
            MCPHTTP.parse(Data("POST /mcp HTTP/1.1\r\nContent-Length: -1\r\n\r\n".utf8)),
            .malformed, "a negative length is nobody's length")
    }

    private static func checkTheRouteAnswersMethodAndPathTogether() {
        expect(MCPHTTP.route("POST", "/mcp"), .mcp, "the transport is POST /mcp")
        expect(MCPHTTP.route("DELETE", "/mcp"), .endSession, "DELETE ends the session")
        expect(
            MCPHTTP.route("GET", "/mcp"), .methodNotAllowed(allow: "POST, DELETE"),
            "a GET of /mcp names what would have been accepted")
        expect(MCPHTTP.route("GET", "/health"), .health, "health is a GET")
        expect(
            MCPHTTP.route("POST", "/health"), .methodNotAllowed(allow: "GET"),
            "and only a GET")
        expect(MCPHTTP.route("GET", "/elsewhere"), .notFound, "everything else is not found")
    }

    /// The one attack this header stops is DNS rebinding — a hostname that
    /// resolves here without being here — so the killer case is the one that
    /// merely starts with a loopback address.
    private static func checkOnlyThisMachineCanBeAnOrigin() {
        expect(MCPHTTP.isLoopbackOrigin(nil), true, "no Origin is no browser")
        expect(MCPHTTP.isLoopbackOrigin("http://127.0.0.1:8765"), true, "IPv4 loopback")
        expect(MCPHTTP.isLoopbackOrigin("https://[::1]:9000"), true, "IPv6 loopback")
        expect(MCPHTTP.isLoopbackOrigin("http://LOCALHOST"), true, "hostnames have no case")
        expect(
            MCPHTTP.isLoopbackOrigin("https://127.0.0.1.evil.example"), false,
            "an address is not a prefix")
        expect(MCPHTTP.isLoopbackOrigin("https://example.com"), false, "elsewhere is elsewhere")
        expect(MCPHTTP.isLoopbackOrigin("not a url"), false, "gibberish is not here either")
    }

    private static func checkTheBearerTokenGatesInWholeBytes() {
        expect(MCPHTTP.authorized("Bearer abc123", token: "abc123"), true, "the token passes")
        expect(MCPHTTP.authorized("Bearer abc124", token: "abc123"), false, "a near miss does not")
        expect(MCPHTTP.authorized("Bearer abc", token: "abc123"), false, "nor a prefix of it")
        expect(MCPHTTP.authorized(nil, token: "abc123"), false, "nor silence")
        expect(
            MCPHTTP.authorized("Basic abc123", token: "abc123"), false,
            "nor another scheme carrying the right bytes")
    }

    // MARK: - The guard: what passes

    private static func checkPlainReadsPass() {
        expect(MCPReadOnlyGuard.obstacle(in: "SELECT 1") == nil, true, "a select")
        expect(
            MCPReadOnlyGuard.obstacle(in: "select id from orders where id = 3") == nil, true,
            "case is not meaning")
        expect(MCPReadOnlyGuard.obstacle(in: "SELECT 1;") == nil, true, "one trailing ; is style")
        expect(
            MCPReadOnlyGuard.obstacle(in: "  SELECT 1 -- and a note") == nil, true,
            "a trailing comment is not a statement")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SHOW CREATE TABLE users") == nil, true,
            "SHOW skips the token scan, which is what lets SHOW CREATE TABLE through")
        expect(
            MCPReadOnlyGuard.obstacle(in: "EXPLAIN SELECT * FROM t") == nil, true,
            "an explain that only explains")
        expect(MCPReadOnlyGuard.obstacle(in: "DESCRIBE t") == nil, true, "describe")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT * FROM analyze_results") == nil, true,
            "a word containing a forbidden word is not that word")
    }

    /// The scan reads blanked statements, so nothing quoted can trip it —
    /// and nothing quoted can hide from it, which the comment and stacking
    /// checks below hold from the other side.
    private static func checkWhatIsInsideQuotesIsData() {
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 'a;b'") == nil, true,
            "a semicolon inside a string is data, not a second statement")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 'drop table users' AS note") == nil, true,
            "a write spelled inside a string is a string")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT \"update\" FROM t") == nil, true,
            "a quoted identifier is a name, whatever it is named")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT $$ INSERT $$") == nil, true,
            "a dollar-quoted string is a string")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 'it''s fine'") == nil, true,
            "a doubled quote stays inside the string")
    }

    // MARK: - The guard: what is refused

    private static func checkWritesAndStateChangesAreRefused() {
        expect(MCPReadOnlyGuard.obstacle(in: "DROP TABLE t") != nil, true, "a drop")
        expect(MCPReadOnlyGuard.obstacle(in: "INSERT INTO t VALUES (1)") != nil, true, "an insert")
        expect(MCPReadOnlyGuard.obstacle(in: "UPDATE t SET a = 1") != nil, true, "an update")
        expect(MCPReadOnlyGuard.obstacle(in: "SET search_path TO public") != nil, true, "a SET")
        expect(MCPReadOnlyGuard.obstacle(in: "BEGIN") != nil, true, "a transaction")
        expect(MCPReadOnlyGuard.obstacle(in: "KILL 42") != nil, true, "a kill")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 1; DROP TABLE t") != nil, true,
            "a second statement behind a first")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT * INTO outdone FROM t") != nil, true,
            "SELECT INTO creates a table")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT * FROM t INTO OUTFILE '/tmp/x'") != nil, true,
            "and OUTFILE writes a file")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT LOAD_FILE('/etc/passwd')") != nil, true,
            "LOAD_FILE reads one this server's user can and this user may not")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT * FROM t FOR UPDATE") != nil, true,
            "a locking read locks")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT * FROM t LOCK IN SHARE MODE") != nil, true,
            "in either spelling")
        expect(
            MCPReadOnlyGuard.obstacle(in: "EXPLAIN ANALYZE UPDATE t SET a = 1") != nil, true,
            "EXPLAIN ANALYZE executes its target")
        expect(
            MCPReadOnlyGuard.obstacle(in: "EXPLAIN (ANALYZE, BUFFERS) SELECT 1") != nil, true,
            "even when the target is a read")
    }

    /// Each case here is a concrete bypass of a lesser scanner, most of them
    /// taken from the reference implementation's own attack corpus and two —
    /// the `#` and the nested comment — from holes it did not cover.
    private static func checkCommentTricksHideNothing() {
        expect(
            MCPReadOnlyGuard.obstacle(in: "SEL/*!ECT*/ 1") != nil, true,
            "an executable comment runs on the server")
        expect(
            MCPReadOnlyGuard.obstacle(in: "/*M!100000 DROP TABLE t */ SELECT 1") != nil, true,
            "in the MariaDB spelling too")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 1 /* note */; DROP TABLE t") != nil, true,
            "a comment does not hide the semicolon after it")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 1 # 2; DROP TABLE t") != nil, true,
            "# is an operator on PostgreSQL, so it is not a comment here")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT /* /* */ ; DROP TABLE t */ 1") != nil, true,
            "comments do not nest, because on MySQL they do not")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT '\\'; DROP TABLE t'") != nil, true,
            "a backslash does not extend a string over a statement")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 'unterminated") != nil, true,
            "an unterminated string reads as anything, so it is refused")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 1 /* still open") != nil, true,
            "and an unterminated comment the same")
    }

    /// The rejections the multi-dialect rules cost, pinned as chosen. Whoever
    /// loosens one of these is choosing to reopen the bypass its comment
    /// names, and should have to say so here.
    private static func checkTheConservativeRejectionsAreDeliberate() {
        expect(
            MCPReadOnlyGuard.obstacle(in: "WITH x AS (SELECT 1) SELECT * FROM x") != nil, true,
            "a read-only CTE is refused because a CTE body can write")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT share FROM holdings") != nil, true,
            "a bare column named share is the price of catching FOR SHARE")
        expect(
            MCPReadOnlyGuard.obstacle(in: "SELECT 1 --x; DROP TABLE t") != nil, true,
            "--x comments on PostgreSQL but not MySQL, so it is read as text")
        expect(MCPReadOnlyGuard.obstacle(in: "") != nil, true, "nothing is not a read")
        expect(
            MCPReadOnlyGuard.obstacle(in: "/* only a comment */") != nil, true,
            "and neither is a comment alone")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("mcp FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
