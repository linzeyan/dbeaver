import Foundation

/// What the app must answer for MCP tools to have anything to say.
///
/// A protocol rather than the model so the dispatcher stays a pure function:
/// `--verify-mcp` hands it a fake and holds every method and every tool
/// without a window, a socket or a database. The live conformance opens its
/// own handles on exposed connections — deliberately not the tabs' sessions,
/// whose transactions, `USE` state and `ROW_COUNT()` a shared connection
/// would silently trample.
protocol MCPDataSource {
    /// The connections marked as exposed, by name. Nothing else exists as far
    /// as MCP is concerned — not as an error, as an absence.
    func connectionNames() -> [String]
    func schemas(connection: String) throws -> [String]
    func relations(connection: String, schema: String?) throws -> [MCPRelation]
    func describe(connection: String, schema: String?, relation: String) throws
        -> MCPRelationDescription
    /// Runs a statement the guard has already passed, stopping after `rowCap`
    /// rows. Capping at the fetch is the honest layer for it: fifteen dialects
    /// spell LIMIT too many ways for a rewrite to be safe, and the batches
    /// stream, so stopping early is cheap.
    func query(connection: String, sql: String, rowCap: Int) throws -> MCPQueryResult
}

struct MCPRelation: Equatable {
    let schema: String?
    let name: String
    let kind: String
}

struct MCPRelationDescription: Equatable {
    struct Column: Equatable {
        let name: String
        let type: String
        let nullable: Bool
    }
    let columns: [Column]
    /// The core's own CREATE statement, where its dialect writes one.
    let ddl: String?
}

struct MCPQueryResult: Equatable {
    let columns: [String]
    let rows: [[String?]]
    /// True when the fetch stopped at the cap with the server still talking.
    /// Told to the agent in so many words, because a truncated result that
    /// does not say so reads as the whole answer.
    let truncated: Bool
}

/// The JSON-RPC half of the MCP server, pure from bytes to bytes.
///
/// Protocol errors and tool errors are different species and never trade
/// places: a method that does not exist is a JSON-RPC `error`, but a tool
/// call that fails — unknown connection, guarded statement, server refusal —
/// is a *successful* response carrying `isError: true`. The second form is
/// what the model on the far end can read and recover from; promoting tool
/// failures to protocol errors is how a server teaches an agent to give up.
enum MCPDispatch {

    /// Newest first. The negotiation echoes what the client asked for when we
    /// speak it, and otherwise answers with the newest we do.
    static let protocolVersions = ["2025-03-26", "2024-11-05"]

    /// Answers one request body, or nil where the body was a notification
    /// and the answer is silence.
    static func handle(_ body: Data, source: MCPDataSource, rowCap: Int, serverVersion: String)
        -> Data?
    {
        guard let parsed = try? JSONSerialization.jsonObject(with: body) else {
            return encode(errorReply(id: NSNull(), code: -32700, message: "Parse error"))
        }
        guard let request = parsed as? [String: Any] else {
            // Batches went from optional to gone across the protocol's own
            // revisions; refusing them outright is smaller than half-keeping
            // a form the spec has already buried.
            return encode(
                errorReply(id: NSNull(), code: -32600, message: "Batch requests are not supported"))
        }
        let id = request["id"]
        guard let method = request["method"] as? String else {
            return encode(errorReply(id: id ?? NSNull(), code: -32600, message: "Invalid Request"))
        }
        if method.hasPrefix("notifications/") { return nil }
        let params = request["params"] as? [String: Any] ?? [:]

        let result: [String: Any]
        switch method {
        case "initialize":
            let asked = params["protocolVersion"] as? String
            let version =
                asked.flatMap { protocolVersions.contains($0) ? $0 : nil } ?? protocolVersions[0]
            result = [
                "protocolVersion": version,
                "capabilities": ["tools": ["listChanged": false]],
                "serverInfo": ["name": "dbclient-mcp", "version": serverVersion],
                // The one channel the far model is guaranteed to read before
                // touching anything; the connection-name convention rides it.
                "instructions":
                    "Every tool takes a connection name; call list_connections first "
                    + "to learn them. All access is read-only, and query results stop "
                    + "at a row cap — a result with truncated: true is not the whole "
                    + "answer."
            ]
        case "ping":
            result = [:]
        case "tools/list":
            result = ["tools": toolDefinitions]
        case "tools/call":
            result = call(params, source: source, rowCap: rowCap)
        default:
            return encode(
                errorReply(id: id ?? NSNull(), code: -32601, message: "Method not found: \(method)")
            )
        }
        return encode(["jsonrpc": "2.0", "id": id ?? NSNull(), "result": result])
    }

    // MARK: - Tools

    /// The five tools, minimal on purpose: enough for an agent to find a
    /// connection, orient, and read. Every one is annotated read-only, and
    /// the annotations are true because the guard and the tools' own queries
    /// make them true.
    static var toolDefinitions: [[String: Any]] {
        [
            tool(
                "list_connections",
                "Lists the database connections exposed to MCP, by name.",
                properties: [:], required: []),
            tool(
                "list_schemas",
                "Lists the schemas (or databases, where the family has no schemas) "
                    + "of one connection.",
                properties: ["connection": string("The connection name.")],
                required: ["connection"]),
            tool(
                "list_relations",
                "Lists tables and views on a connection, optionally within one schema.",
                properties: [
                    "connection": string("The connection name."),
                    "schema": string("A schema name from list_schemas; omit for all.")
                ],
                required: ["connection"]),
            tool(
                "describe_relation",
                "The columns of one table or view, with types and nullability, "
                    + "and its DDL where the dialect can write one.",
                properties: [
                    "connection": string("The connection name."),
                    "schema": string("The schema holding it; omit where unambiguous."),
                    "relation": string("The table or view name.")
                ],
                required: ["connection", "relation"]),
            tool(
                "query",
                "Runs one read-only statement (SELECT, SHOW, EXPLAIN, DESCRIBE) "
                    + "and returns rows up to the server's cap.",
                properties: [
                    "connection": string("The connection name."),
                    "sql": string("The statement. One statement, reads only.")
                ],
                required: ["connection", "sql"])
        ]
    }

    private static func call(_ params: [String: Any], source: MCPDataSource, rowCap: Int)
        -> [String: Any]
    {
        guard let name = params["name"] as? String else {
            return toolFailure("The call names no tool.")
        }
        let arguments = params["arguments"] as? [String: Any] ?? [:]

        func argument(_ key: String) -> String? {
            (arguments[key] as? String).flatMap { $0.isEmpty ? nil : $0 }
        }
        func requiring(_ key: String, _ then: (String) -> [String: Any]) -> [String: Any] {
            guard let value = argument(key) else {
                return toolFailure("\(name) requires \(key).")
            }
            return then(value)
        }

        switch name {
        case "list_connections":
            return toolSuccess(["connections": source.connectionNames()])
        case "list_schemas":
            return requiring("connection") { connection in
                answering { ["schemas": try source.schemas(connection: connection)] }
            }
        case "list_relations":
            return requiring("connection") { connection in
                answering {
                    let relations = try source.relations(
                        connection: connection, schema: argument("schema"))
                    return [
                        "relations": relations.map {
                            ["schema": $0.schema ?? "", "name": $0.name, "kind": $0.kind]
                        }
                    ]
                }
            }
        case "describe_relation":
            return requiring("connection") { connection in
                requiring("relation") { relation in
                    answering {
                        let described = try source.describe(
                            connection: connection, schema: argument("schema"),
                            relation: relation)
                        var payload: [String: Any] = [
                            "columns": described.columns.map {
                                ["name": $0.name, "type": $0.type, "nullable": $0.nullable]
                            }
                        ]
                        if let ddl = described.ddl { payload["ddl"] = ddl }
                        return payload
                    }
                }
            }
        case "query":
            return requiring("connection") { connection in
                requiring("sql") { sql in
                    if let obstacle = MCPReadOnlyGuard.obstacle(in: sql) {
                        return toolFailure(obstacle)
                    }
                    return answering {
                        let result = try source.query(
                            connection: connection, sql: sql, rowCap: rowCap)
                        let names = uniqued(result.columns)
                        return [
                            "columns": names,
                            "rows": result.rows.map { row -> [String: Any] in
                                let values = row.map { $0.map { $0 as Any } ?? NSNull() }
                                return Dictionary(uniqueKeysWithValues: zip(names, values))
                            },
                            "rowCount": result.rows.count,
                            "truncated": result.truncated
                        ]
                    }
                }
            }
        default:
            return toolFailure("No tool is named \(name).")
        }
    }

    /// Column names made unique the way a reader would: the second `id`
    /// becomes `id_2`. A join's duplicate names are legal SQL, and rows
    /// keyed by name silently drop every column after the first otherwise.
    static func uniqued(_ names: [String]) -> [String] {
        var seen = Set<String>()
        var out: [String] = []
        for name in names {
            var candidate = name
            var suffix = 2
            while seen.contains(candidate) {
                candidate = "\(name)_\(suffix)"
                suffix += 1
            }
            seen.insert(candidate)
            out.append(candidate)
        }
        return out
    }

    // MARK: - Envelopes

    private static func answering(_ work: () throws -> [String: Any]) -> [String: Any] {
        do {
            return toolSuccess(try work())
        } catch {
            return toolFailure(String(describing: error))
        }
    }

    private static func toolSuccess(_ payload: [String: Any]) -> [String: Any] {
        ["content": [["type": "text", "text": pretty(payload)]], "isError": false]
    }

    private static func toolFailure(_ message: String) -> [String: Any] {
        ["content": [["type": "text", "text": message]], "isError": true]
    }

    private static func errorReply(id: Any, code: Int, message: String) -> [String: Any] {
        ["jsonrpc": "2.0", "id": id, "error": ["code": code, "message": message]]
    }

    private static func tool(
        _ name: String, _ description: String, properties: [String: Any], required: [String]
    ) -> [String: Any] {
        [
            "name": name,
            "description": description,
            "inputSchema": [
                "type": "object", "properties": properties, "required": required
            ],
            "annotations": [
                "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false
            ]
        ]
    }

    private static func string(_ description: String) -> [String: Any] {
        ["type": "string", "description": description]
    }

    private static func pretty(_ payload: [String: Any]) -> String {
        guard
            let data = try? JSONSerialization.data(
                withJSONObject: payload, options: [.prettyPrinted, .sortedKeys])
        else { return "{}" }
        return String(decoding: data, as: UTF8.self)
    }

    private static func encode(_ reply: [String: Any]) -> Data {
        (try? JSONSerialization.data(withJSONObject: reply, options: [.sortedKeys])) ?? Data()
    }
}
