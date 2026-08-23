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
        checkInitializeNegotiatesWhatBothSpeak()
        checkTheProtocolAnswersItsOwnShapes()
        checkTheToolListIsTheWholeOffer()
        checkAToolFailureIsAnAnswerNotAProtocolError()
        checkAQueryComesBackAsRowsWithHonestNames()
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

    // MARK: - The dispatcher

    /// The double the dispatcher answers from: two connections, one schema,
    /// one relation — and a join-shaped query result whose duplicate column
    /// names are the point.
    private enum FakeFailure: Error { case noSuchConnection }
    private struct FakeSource: MCPDataSource {
        func connectionNames() -> [String] { ["prod-pg", "local"] }
        func schemas(connection: String) throws -> [String] {
            guard connection == "prod-pg" else { throw FakeFailure.noSuchConnection }
            return ["public"]
        }
        func relations(connection: String, schema: String?) throws -> [MCPRelation] {
            [MCPRelation(schema: "public", name: "orders", kind: "table")]
        }
        func describe(connection: String, schema: String?, relation: String) throws
            -> MCPRelationDescription
        {
            MCPRelationDescription(
                columns: [.init(name: "id", type: "bigint", nullable: false)],
                ddl: "CREATE TABLE orders (id bigint)")
        }
        func query(connection: String, sql: String, rowCap: Int) throws -> MCPQueryResult {
            MCPQueryResult(
                columns: ["id", "id"], rows: [["1", "2"]], truncated: true)
        }
    }

    private static func reply(to json: String) -> [String: Any]? {
        guard
            let data = MCPDispatch.handle(
                Data(json.utf8), source: FakeSource(), rowCap: 100, serverVersion: "0.0")
        else { return nil }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }

    private static func toolText(_ reply: [String: Any]?) -> (isError: Bool, text: String) {
        let result = reply?["result"] as? [String: Any]
        let content = result?["content"] as? [[String: Any]]
        return (
            result?["isError"] as? Bool ?? false,
            content?.first?["text"] as? String ?? ""
        )
    }

    private static func checkInitializeNegotiatesWhatBothSpeak() {
        let old = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#
        )
        expect(
            (old?["result"] as? [String: Any])?["protocolVersion"] as? String, "2024-11-05",
            "a version both sides speak is echoed")
        let unknown = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#
        )
        expect(
            (unknown?["result"] as? [String: Any])?["protocolVersion"] as? String, "2025-03-26",
            "an unknown one is answered with the newest we do")
        let instructions =
            (old?["result"] as? [String: Any])?["instructions"] as? String ?? ""
        expect(
            instructions.contains("list_connections"), true,
            "the instructions teach the connection-name convention")
    }

    private static func checkTheProtocolAnswersItsOwnShapes() {
        func code(_ reply: [String: Any]?) -> Int? {
            (reply?["error"] as? [String: Any])?["code"] as? Int
        }
        expect(code(reply(to: "not json")), -32700, "unparseable bytes are a parse error")
        expect(code(reply(to: "[1,2]")), -32600, "a batch is refused whole")
        expect(code(reply(to: #"{"jsonrpc":"2.0","id":1}"#)), -32600, "no method is no request")
        let missing = reply(to: #"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#)
        expect(code(missing), -32601, "an unimplemented method says so")
        expect(
            ((missing?["error"] as? [String: Any])?["message"] as? String ?? "")
                .contains("resources/list"),
            true, "and names it")
        expect(
            MCPDispatch.handle(
                Data(#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.utf8),
                source: FakeSource(), rowCap: 100, serverVersion: "0.0") == nil,
            true, "a notification is answered with silence")
        let ping = reply(to: #"{"jsonrpc":"2.0","id":7,"method":"ping"}"#)
        expect(ping?["result"] is [String: Any], true, "ping answers with an empty result")
        expect(ping?["id"] as? Int, 7, "under the caller's id")
    }

    private static func checkTheToolListIsTheWholeOffer() {
        let tools =
            (reply(to: #"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)?["result"]
            as? [String: Any])?["tools"] as? [[String: Any]] ?? []
        expect(
            tools.compactMap { $0["name"] as? String }.sorted(),
            ["describe_relation", "list_connections", "list_relations", "list_schemas", "query"],
            "five tools: find a connection, orient, read")
        expect(
            tools.allSatisfy {
                ($0["annotations"] as? [String: Any])?["readOnlyHint"] as? Bool == true
            },
            true, "every one annotated read-only, because every one is")
    }

    /// The distinction this holds is the one that keeps the far model useful:
    /// a failed tool call is a result it can read and recover from, not a
    /// protocol error that teaches it to give up.
    private static func checkAToolFailureIsAnAnswerNotAProtocolError() {
        let unknown = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"drop_everything"}}"#
        )
        expect(unknown?["error"] == nil, true, "an unknown tool is not a protocol error")
        expect(toolText(unknown).isError, true, "it is a tool answer that says no")
        let guarded = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"query","arguments":{"connection":"prod-pg","sql":"DROP TABLE t"}}}"#
        )
        expect(toolText(guarded).isError, true, "a guarded statement is refused")
        expect(
            toolText(guarded).text.contains("Only reads"), true,
            "with the guard's own sentence")
        let unnamed = reply(
            to: #"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_schemas"}}"#
        )
        expect(toolText(unnamed).isError, true, "a missing required argument is refused")
        let thrown = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_schemas","arguments":{"connection":"gone"}}}"#
        )
        expect(toolText(thrown).isError, true, "and so is a data source that threw")
    }

    private static func checkAQueryComesBackAsRowsWithHonestNames() {
        expect(
            MCPDispatch.uniqued(["id", "id", "id_2"]), ["id", "id_2", "id_2_2"],
            "a taken suffix keeps walking rather than colliding")
        let answer = reply(
            to:
                #"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"query","arguments":{"connection":"prod-pg","sql":"SELECT 1"}}}"#
        )
        let (isError, text) = toolText(answer)
        expect(isError, false, "a read that ran is a success")
        expect(
            text.contains("\"id_2\""), true,
            "a join's second id is renamed rather than silently dropped")
        expect(
            text.contains("\"truncated\" : true"), true,
            "a capped result says it is not the whole answer")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("mcp FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
