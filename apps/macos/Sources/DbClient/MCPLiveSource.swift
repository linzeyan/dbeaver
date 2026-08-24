import Foundation

/// What went wrong between a tool call and a database, in words the far
/// model can act on. `String(describing:)` is how `MCPDispatch` prints an
/// error into a tool failure, so the description carries the whole message.
struct MCPSourceError: Error, CustomStringConvertible {
    let description: String
}

/// The live `MCPDataSource`: real connections, opened by the server for the
/// server.
///
/// Deliberately not the tabs' sessions. A shared handle would run MCP reads
/// inside whatever transaction a tab has open, trample its `USE` state and
/// its `ROW_COUNT()`, and queue agent queries behind a human's slow browse —
/// so this source opens its own handle per exposed connection and the two
/// worlds never meet. The price is a second connection per exposed entry,
/// which is the price of not corrupting anybody's session.
///
/// Credentials are the stored ones only: the credential file, then the
/// Keychain. `SessionPasswords` is excluded on purpose — a password typed
/// for one session was handed to a window, not to every agent on the
/// machine, and a connection that only ever gets its password that way is
/// one its owner has chosen not to leave lying around.
final class MCPLiveSource: MCPDataSource {

    /// One exposed connection, in the form the wire side needs: enough to
    /// open a handle, nothing the main actor has to be asked for twice.
    struct Entry {
        let name: String
        let id: UUID
        let settings: ConnectionSettings
    }

    /// Asks the app which connections are exposed, called on the wire queue.
    ///
    /// A closure and not a model reference so this file never imports the
    /// main actor's world: the coordinator builds one that hops to the main
    /// queue, reads, and returns. The standing invariant that makes the hop
    /// safe: nothing on the main actor ever waits on the MCP wire queue —
    /// the coordinator only ever `async`s toward it — so the `main.sync`
    /// inside the provider cannot close a cycle.
    private let entriesProvider: () -> [Entry]

    /// Open handles by connection id, with the settings each was opened
    /// from. Confined to `stateQueue`; the wire queue is the only caller in
    /// practice, but `closeAll()` arrives from the main actor and a lock
    /// that is only mostly needed is not a lock.
    private var handles: [UUID: (settings: ConnectionSettings, database: Database)] = [:]
    private let stateQueue = DispatchQueue(label: "dev.dbclient.mcp.source")

    init(entriesProvider: @escaping () -> [Entry]) {
        self.entriesProvider = entriesProvider
    }

    /// The exposed subset of saved connections, named for MCP.
    ///
    /// Static and pure so `--verify-mcp` can pin the filter without a model:
    /// the flag decides membership, the title is the name, and duplicate
    /// titles are told apart downstream by `MCPDispatch.uniqued`.
    static func entries(of connections: [SavedConnection]) -> [Entry] {
        connections.filter(\.exposedToMCP).map {
            Entry(name: $0.title, id: $0.id, settings: $0.settings)
        }
    }

    /// Drops every open handle, off the calling thread.
    ///
    /// Called when the server stops. The drop happens on the state queue
    /// because `Database.deinit` closes a connection over the network, which
    /// is not main-thread work; the caller does not wait for it.
    func closeAll() {
        stateQueue.async { [self] in handles = [:] }
    }

    // MARK: - MCPDataSource

    func connectionNames() -> [String] {
        MCPDispatch.uniqued(entriesProvider().map(\.name))
    }

    /// The schemas an agent is told about, which are never the engine's own.
    ///
    /// Not the window's setting. That switch is about a tree somebody is looking
    /// at and can collapse; this list goes into a context window, where
    /// `pg_catalog`'s few thousand relations are three thousand four hundred
    /// tokens of noise between a model and the table it was asked about. The
    /// same argument the row cap is set by.
    ///
    /// An agent that genuinely wants a catalog table can still reach it: the
    /// query tool runs whatever it is given, `pg_catalog` included.
    func schemas(connection: String) throws -> [String] {
        try withDatabase(connection) { db in
            let schemas = try db.schemas().filter { !$0.isSystem }
            if !schemas.isEmpty { return schemas.map(\.name) }
            // A family with no schema level answers with its databases, as
            // the tool description promises; one with neither answers empty.
            guard let databases = (try? db.databases()) ?? nil else { return [] }
            return databases.map(\.name)
        }
    }

    func relations(connection: String, schema: String?) throws -> [MCPRelation] {
        try withDatabase(connection) { db in
            // An explicitly named schema is honoured whatever it is — an agent
            // that asked for `pg_catalog` gets it. Only the unqualified sweep
            // leaves the engine's own out, for the reason `schemas` does.
            let schemas =
                try schema.map { [$0] } ?? db.schemas().filter { !$0.isSystem }.map(\.name)
            return try schemas.flatMap { name in
                try db.relations(schema: name).map {
                    MCPRelation(schema: $0.schema, name: $0.name, kind: $0.kind.rawValue)
                }
            }
        }
    }

    func describe(connection: String, schema: String?, relation: String) throws
        -> MCPRelationDescription
    {
        try withDatabase(connection) { db in
            let within = try resolveSchema(schema, holding: relation, in: db)
            let columns = try db.columns(schema: within, relation: relation).map {
                MCPRelationDescription.Column(
                    name: $0.name, type: $0.dataType, nullable: $0.nullable)
            }
            guard !columns.isEmpty else {
                throw MCPSourceError(
                    description: "No relation named \(relation) in \(within).")
            }
            // Optional by contract: a dialect that cannot write DDL is an
            // ordinary answer here, not a failure to hide the columns behind.
            return MCPRelationDescription(
                columns: columns, ddl: try? db.ddl(schema: within, relation: relation))
        }
    }

    func query(connection: String, sql: String, rowCap: Int) throws -> MCPQueryResult {
        try withDatabase(connection) { db in
            let query = try db.query(sql, batchRows: max(1, min(rowCap, 1024)))

            let table = ArrowTable()
            let schema = try query.schema()
            table.setSchema(schema)
            if let release = schema.pointee.release { release(schema) }
            schema.deallocate()

            // The batch that crosses the cap is kept and trimmed below: a
            // pulled batch is owned, and appending it is how it gets freed.
            // Reading one row past the cap is also the truncation test — a
            // stream that ends exactly at the cap was not truncated.
            while table.rowCount <= rowCap, let batch = try query.nextBatch() {
                table.append(batch: batch)
            }

            let snapshot = table.snapshot()
            let kept = min(snapshot.rowCount, rowCap)
            let columns = snapshot.columns.map(\.name)
            let rows = (0..<kept).map { row in
                columns.indices.map { snapshot.value(row: row, column: $0) }
            }
            return MCPQueryResult(
                columns: columns, rows: rows, truncated: snapshot.rowCount > rowCap)
        }
    }

    /// The schema a relation lives in, where the caller did not say.
    ///
    /// A search rather than a default: fifteen families default differently,
    /// and guessing `public` on the wrong one answers with somebody else's
    /// table. Ambiguity is an error that names the candidates, which is the
    /// answer an agent can act on.
    private func resolveSchema(_ schema: String?, holding relation: String, in db: Database)
        throws -> String
    {
        if let schema { return schema }
        // The engine's own left out, so that a relation named like a catalog
        // table does not come back as "exists in more than one schema" against a
        // schema this tool never lists.
        let matches = try db.schemas().filter { !$0.isSystem }.map(\.name).filter { name in
            (try? db.relations(schema: name).contains { $0.name == relation }) ?? false
        }
        guard let only = matches.first else {
            throw MCPSourceError(
                description:
                    "No relation named \(relation) on this connection; "
                    + "list_relations has the current names.")
        }
        guard matches.count == 1 else {
            throw MCPSourceError(
                description:
                    "\(relation) exists in more than one schema "
                    + "(\(matches.joined(separator: ", "))); say which.")
        }
        return only
    }

    // MARK: - Handles

    /// Runs one call against the named connection's handle, opening it if
    /// need be and dropping it if the call fails.
    ///
    /// Dropped on failure because a dead handle is the likeliest cause and
    /// retrying into it would fail forever: the next tool call reopens and
    /// either works or reports the real obstacle. A statement that fails on
    /// its own merits costs one needless reopen, which is cheap enough not
    /// to tell the two apart here.
    private func withDatabase<T>(_ connection: String, _ work: (Database) throws -> T)
        throws -> T
    {
        let entry = try resolve(connection)
        let db = try open(entry)
        do {
            return try work(db)
        } catch {
            stateQueue.sync { handles[entry.id] = nil }
            throw error
        }
    }

    private func resolve(_ connection: String) throws -> Entry {
        let entries = entriesProvider()
        let names = MCPDispatch.uniqued(entries.map(\.name))
        guard let index = names.firstIndex(of: connection) else {
            throw MCPSourceError(
                description:
                    "No connection named \(connection) is exposed to MCP; "
                    + "list_connections has the current names.")
        }
        return entries[index]
    }

    private func open(_ entry: Entry) throws -> Database {
        try stateQueue.sync {
            // Compared by settings, not just found by id: a connection
            // re-pointed at another server while a handle was open must not
            // keep answering from the old one.
            if let held = handles[entry.id], held.settings == entry.settings {
                return held.database
            }
            handles[entry.id] = nil
            let password =
                CredentialFile.shared.password(for: entry.id)
                ?? ConnectionKeychain.password(for: entry.id) ?? ""
            var ssh: SshConfig?
            if !entry.settings.sshHost.trimmingCharacters(in: .whitespaces).isEmpty {
                let secret =
                    CredentialFile.shared.sshSecret(for: entry.id)
                    ?? ConnectionKeychain.password(for: entry.id, .ssh) ?? ""
                ssh = AppModel.bastion(for: entry.settings, secret: secret)
            }
            let database = try Database(
                connString: entry.settings.connectionString(password: password), ssh: ssh)
            handles[entry.id] = (entry.settings, database)
            return database
        }
    }
}
