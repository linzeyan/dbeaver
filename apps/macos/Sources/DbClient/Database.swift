import CDbFfi
import Foundation

struct DbError: Error, CustomStringConvertible {
    let description: String
    /// Where the server says the statement went wrong: 1-based, in characters,
    /// and counted from the start of the SQL that was sent — which is the
    /// statement, not the editor buffer it was cut from. Nil for every error
    /// that is not about a place in a statement.
    let position: Int?
    /// Whether the server stopped this statement because somebody asked it to.
    ///
    /// Comes from the core's own reading of the SQLSTATE, not from this side
    /// remembering that it pressed Cancel. A statement can fail on its own
    /// merits in the same instant the request lands, and a window that reported
    /// that as "Cancelled" would hide a real fault behind a button.
    let cancelled: Bool

    init(description: String, position: Int? = nil, cancelled: Bool = false) {
        self.description = description
        self.position = position
        self.cancelled = cancelled
    }
}

/// An SSH bastion to reach a database through.
///
/// A type of this side's own rather than the C struct passed around directly,
/// because that struct holds pointers and something has to own the strings they
/// point at for the length of the call. Swift `String`s here, C strings only
/// inside `Database.init`, which is the one place the lifetime is short enough
/// to see.
struct SshConfig {
    var host: String
    var port: UInt16
    var user: String
    /// Exactly one of these two. The core refuses both and neither rather than
    /// picking one, so a form that can produce either state hears about it.
    var password: String?
    var keyPath: String?
    /// Nil for a key that is not encrypted.
    var passphrase: String?
    /// The file the bastion's identity is checked against, in full.
    var knownHosts: String
}

/// Swift wrapper over the core's C surface.
///
/// Every call blocks, so nothing here may run on the main thread. Phase 1
/// replaces this with an event-queue design; the blocking shape is kept for now
/// because it is small enough to audit by reading it.
///
/// `@unchecked Sendable` states the arrangement the core already documents
/// rather than a hope: every call but `cancel()` runs on the one serial queue
/// that owns this connection, which is what makes the shared `errOut` slot
/// below safe. `cancel()` is the deliberate exception — it has to be reachable
/// while the queue is blocked, or it would arrive after the statement it exists
/// to stop — and it touches only `handle`, which never changes after `init`.
final class Database: @unchecked Sendable {
    private let handle: OpaquePointer

    /// For `Cursor.exportSql` and nothing else: writing INSERT statements
    /// needs the connection's dialect, which lives behind this handle, and the
    /// export itself belongs on the cursor beside the formats that do not.
    fileprivate var rawHandle: OpaquePointer { handle }

    /// Ten seconds by default, which is the number this application picks rather
    /// than one any database suggests. It is long enough that a loaded server on
    /// a slow link still opens and short enough that a wrong host is a wrong host
    /// rather than a window that never comes back.
    init(connString: String, ssh: SshConfig? = nil, timeoutSeconds: UInt32 = 10) throws {
        var err: UnsafeMutablePointer<CChar>?

        // The C struct holds pointers, so the strings have to outlive the call.
        // Copied with `strdup` and freed on the way out, rather than threaded
        // through seven nested `withCString` closures: that is the same lifetime
        // written in a shape nobody can read.
        var owned: [UnsafeMutablePointer<CChar>?] = []
        defer { owned.forEach { free($0) } }
        func borrow(_ value: String?) -> UnsafePointer<CChar>? {
            guard let value else { return nil }
            let copy = strdup(value)
            owned.append(copy)
            return UnsafePointer(copy)
        }

        let opened: OpaquePointer?
        if let ssh {
            var config = DbSshConfig(
                host: borrow(ssh.host),
                port: ssh.port,
                user: borrow(ssh.user),
                password: borrow(ssh.password),
                key_path: borrow(ssh.keyPath),
                passphrase: borrow(ssh.passphrase),
                known_hosts: borrow(ssh.knownHosts))
            opened = withUnsafePointer(to: &config) {
                db_connect(connString, $0, timeoutSeconds, &err)
            }
        } else {
            opened = db_connect(connString, nil, timeoutSeconds, &err)
        }

        guard let h = opened else {
            throw DbError(description: Database.take(&err) ?? "connect failed")
        }
        handle = h
    }

    /// Whether the database is still on the other end.
    ///
    /// A round trip, so it belongs off the main actor with everything else here.
    /// It queues behind whatever this connection is running, which is why the
    /// caller asks only when the session is idle — a ping that waited out a slow
    /// statement would report the connection healthy at the moment it finally
    /// stopped being interesting.
    func ping() -> Bool {
        var err: UnsafeMutablePointer<CChar>?
        let ok = db_ping(handle, &err) == 0
        if !ok { _ = Database.take(&err) }
        return ok
    }

    deinit { db_free(handle) }

    /// Asks the server to abandon whatever this connection is running.
    ///
    /// The one method here that may be called while another is in flight, and
    /// the one that must not go on the queue the others share: that queue is
    /// serial and is exactly what is blocked, so a cancel queued behind the
    /// statement would arrive after it finished. The core sends the request on
    /// a connection of its own for the same reason.
    ///
    /// Silent on failure. A cancel that could not be delivered leaves the
    /// statement running, which the window already shows by going on saying
    /// "Running…"; a second message about the request itself would be about
    /// this application's plumbing rather than about the user's database.
    func cancel() {
        var err: UnsafeMutablePointer<CChar>?
        if db_cancel(handle, &err) != 0 {
            let message = Database.take(&err) ?? "cancel failed"
            fputs("cancel request failed: \(message)\n", stderr)
        }
    }

    // MARK: - Identity

    /// What answered this connection.
    ///
    /// A round trip rather than something read off the connection string, and
    /// priced like a statement rather than like an accessor: it is asked once,
    /// when a connection opens or when somebody presses Test.
    func serverInfo() throws -> ServerInfo {
        try decodeJSON(db_server_info_json(handle, &errOut), as: ServerInfo.self)
    }

    /// What this connection can do. Unlike `serverInfo` this is not a round trip
    /// — the core settled it when the connection opened — so it is priced like
    /// an accessor and asked once, when the session adopts the connection.
    func capabilities() throws -> Capabilities {
        try decodeJSON(db_capabilities_json(handle, &errOut), as: Capabilities.self)
    }

    // MARK: - Metadata

    /// `nil` where this engine has no database level, which a JSON `null`
    /// carries across as an absent optional rather than an empty array.
    func databases() throws -> [DatabaseInfo]? {
        try decodeJSON(db_databases_json(handle, &errOut), as: [DatabaseInfo]?.self)
    }

    /// Moves this connection onto another database, where
    /// `capabilities().switchesDatabase` says it can.
    ///
    /// Throws where it cannot, carrying the core's own sentence: the drivers
    /// that report a level of databases without this capability are saying the
    /// entries are connections to open, and the window opens one instead.
    ///
    /// Everything read afterwards is of the new database, so the caller re-reads
    /// the tree the way it does after a refresh.
    func useDatabase(_ name: String) throws {
        try step("switching database failed") { db_use_database(handle, name, &$0) }
    }

    func schemas() throws -> [SchemaInfo] {
        try decodeJSON(db_schemas_json(handle, &errOut), as: [SchemaInfo].self)
    }

    func relations(schema: String) throws -> [RelationInfo] {
        try decodeJSON(db_relations_json(handle, schema, &errOut), as: [RelationInfo].self)
    }

    func columns(schema: String, relation: String) throws -> [ColumnInfo] {
        try decodeJSON(
            db_columns_json(handle, schema, relation, &errOut), as: [ColumnInfo].self)
    }

    func indexes(schema: String, relation: String) throws -> [IndexInfo] {
        try decodeJSON(
            db_indexes_json(handle, schema, relation, &errOut), as: [IndexInfo].self)
    }

    /// Which columns name one row of a relation.
    ///
    /// The core's answer and not this side's, for the reason `browseStatement`
    /// is asked for rather than assembled: the rule is the primary key, or the
    /// narrowest NOT NULL unique key where there is no primary key, and a window
    /// holding its own copy of that would be a second rule to keep in step.
    ///
    /// An empty `columns` is an answer rather than a failure — the relation
    /// cannot be edited, and `obstacle` says why.
    func rowIdentity(schema: String, relation: String) throws -> RowIdentity {
        try decodeJSON(
            db_row_identity_json(handle, schema, relation, &errOut), as: RowIdentity.self)
    }

    /// The statements that would recreate one relation.
    ///
    /// Plain text, which is what the core sends: this is one value rather than a
    /// record, and a JSON document around it would be a decode to reach the only
    /// field in it.
    ///
    /// Throws where the statement cannot be written — a database whose DDL the
    /// core has not learned yet, or a kind whose statement needs facts the
    /// metadata does not carry. Both are ordinary answers rather than faults, so
    /// the caller decides what to show for them.
    func ddl(schema: String, relation: String) throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        guard let raw = db_ddl_text(handle, schema, relation, &err) else {
            throw DbError(description: Database.take(&err) ?? "DDL failed")
        }
        defer { db_string_free(raw) }
        return String(cString: raw)
    }

    func foreignKeys(schema: String, relation: String) throws -> [RelationshipInfo] {
        try decodeJSON(
            db_foreign_keys_json(handle, schema, relation, &errOut), as: [RelationshipInfo].self)
    }

    func referencedBy(schema: String, relation: String) throws -> [RelationshipInfo] {
        try decodeJSON(
            db_referenced_by_json(handle, schema, relation, &errOut), as: [RelationshipInfo].self)
    }

    func constraints(schema: String, relation: String) throws -> [ConstraintInfo] {
        try decodeJSON(
            db_constraints_json(handle, schema, relation, &errOut), as: [ConstraintInfo].self)
    }

    func triggers(schema: String, relation: String) throws -> [TriggerInfo] {
        try decodeJSON(
            db_triggers_json(handle, schema, relation, &errOut), as: [TriggerInfo].self)
    }

    /// What could be typed at `caret` in `text`, best first.
    ///
    /// A metadata call like the ones above, and blocking like them, though it is
    /// asked on a keystroke: the core remembers what it learned, so the first
    /// question on a connection costs the round trips and the rest are answered
    /// from memory.
    func completions(in text: String, caret: Int) throws -> SQLCompletion.Answer {
        try decodeJSON(
            db_complete_json(handle, text, UInt32(clamping: caret), &errOut),
            as: SQLCompletion.Answer.self)
    }

    /// Forgets the names this connection has been told, so the next completion
    /// asks the server again. What Refresh means for the editor.
    func forgetNames() {
        db_names_forget(handle)
    }

    /// `text` laid out again, or `text` itself where the core could not.
    ///
    /// Static, and the only call here that needs no connection: laying SQL out
    /// is a property of the text, so the Query tab can offer it before a
    /// database has been chosen. It also cannot report a failure — the core
    /// hands unreadable SQL back unchanged, and a null answer means the string
    /// did not survive the C boundary, which is not something the user can act
    /// on and must not cost them their buffer.
    static func formatted(_ text: String) -> String {
        var err: UnsafeMutablePointer<CChar>?
        guard let raw = db_sql_format(text, &err) else {
            if let err { db_string_free(err) }
            return text
        }
        defer { db_string_free(raw) }
        return String(cString: raw)
    }

    /// The words this database writes in front of a statement to ask for its
    /// plan, or nothing where it has no such spelling.
    ///
    /// Static and connection-free for the reason `formatted` is: how a database
    /// spells this is a property of its dialect rather than of an open session.
    ///
    /// Nil is an answer and not a failure. SQL Server asks for a plan with a
    /// session setting either side of the statement, which no prefix can be, and
    /// a scheme this build has no dialect for has no answer at all — what a
    /// caller does with nil is not offer the command.
    static func explainPrefix(for scheme: String) -> String? {
        guard let raw = db_sql_explain_prefix(scheme) else { return nil }
        defer { db_string_free(raw) }
        return String(cString: raw)
    }

    /// The statement that reads a relation's rows.
    ///
    /// Asked of the core rather than assembled here, because a statement is the
    /// database's own language and this side does not know it: quoting differs
    /// between them, and MongoDB's browse is not SQL at all. What comes back is
    /// run through `cursor` like anything typed into the editor.
    ///
    /// `keys` orders the rows so that a browse looks the same twice; `limit`
    /// belongs to a caller seeding the editor rather than to the Content tab,
    /// whose bound is the cursor.
    func browseStatement(
        schema: String, relation: String, filter: String?, order: String?,
        keys: [String], limit: UInt32? = nil
    ) throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        let request = BrowseRequest(
            schema: schema, relation: relation, filter: filter, order: order,
            keys: keys, limit: limit)
        guard let raw = db_browse_statement(handle, request.json, &err) else {
            throw DbError(description: Database.take(&err) ?? "browse statement failed")
        }
        defer { db_string_free(raw) }
        return String(cString: raw)
    }

    private struct BrowseRequest: Encodable {
        let schema: String
        let relation: String
        let filter: String?
        let order: String?
        let keys: [String]
        let limit: UInt32?

        var json: String {
            let data = (try? JSONEncoder().encode(self)) ?? Data()
            return String(data: data, encoding: .utf8) ?? "{}"
        }
    }

    /// Which columns this relation can be filtered on, and what each may be
    /// asked.
    ///
    /// The core's answer rather than a reading of `columns(schema:relation:)`.
    /// Whether a declared type can be ordered, and whether this database can be
    /// told how to escape a `LIKE`, are both facts the core already holds — and
    /// a popup built here from a second copy of them would offer an operator
    /// `filterClause` then refuses to write.
    func filterColumns(schema: String, relation: String) throws -> [FilterColumn] {
        try decodeJSON(
            db_filter_columns_json(handle, schema, relation, &errOut), as: [FilterColumn].self)
    }

    /// A stack of filter rows as one WHERE clause, in this database's own
    /// quoting.
    ///
    /// Asked of the core for the reason `browseStatement` is: quoting is the
    /// database's own, and whether a value is written bare or in quotes is a
    /// fact about the type its column was declared with. This side has neither,
    /// and a guess would write a filter that fails — or, worse, one that runs
    /// and means something else. `contains` adds a third: it is a `LIKE`, a
    /// `LIKE` needs an escape character, and which character it may be is the
    /// database's own as well.
    ///
    /// One row goes the same way as five. The cell menu's *Equals This Value*
    /// is a stack of one, and it had its own call until the menu started adding
    /// rows instead of writing text.
    ///
    /// Values go over as they were typed. Quoting them on this side as well is
    /// the second layer that makes a filter match literal apostrophes.
    ///
    /// No rules answers an empty string, which is the unfiltered browse.
    func filterClause(schema: String, relation: String, rules: [FilterRule]) throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        let request = RowFilterRequest(schema: schema, relation: relation, rules: rules)
        guard let raw = db_filter_clause(handle, request.json, &err) else {
            throw DbError(description: Database.take(&err) ?? "filter clause failed")
        }
        defer { db_string_free(raw) }
        return String(cString: raw)
    }

    private struct RowFilterRequest: Encodable {
        let schema: String
        let relation: String
        let rules: [FilterRule]

        var json: String {
            let data = (try? JSONEncoder().encode(self)) ?? Data()
            return String(data: data, encoding: .utf8) ?? "{}"
        }
    }

    /// The statements a grid's pending changes would take.
    ///
    /// Written by the core and run by the caller, which is what puts an edit
    /// inside whatever transaction this connection is in and under the same
    /// Cancel button as anything else — and what lets the statements be shown to
    /// somebody before they run.
    func editStatements(_ request: EditRequest) throws -> [String] {
        try decodeJSON(db_edit_sql_json(handle, request.json, &errOut), as: [String].self)
    }

    // MARK: - Transactions

    /// What this connection's transaction is doing.
    func transactionState() throws -> TransactionState {
        try decodeJSON(db_tx_state_json(handle, &errOut), as: TransactionState.self)
    }

    /// Enters or leaves autocommit.
    ///
    /// Throws while a transaction is open rather than deciding what to do with
    /// the work in it. The window asks, and then commits or rolls back.
    func setAutocommit(_ on: Bool) throws {
        try step("autocommit failed") { db_tx_autocommit(handle, on ? 1 : 0, &$0) }
    }

    func commit() throws {
        try step("commit failed") { db_tx_commit(handle, &$0) }
    }

    func rollback() throws {
        try step("rollback failed") { db_tx_rollback(handle, &$0) }
    }

    func savepoint(_ name: String) throws {
        try step("savepoint failed") { db_tx_savepoint(handle, name, &$0) }
    }

    func rollback(to name: String) throws {
        try step("rollback to savepoint failed") { db_tx_rollback_to(handle, name, &$0) }
    }

    func release(_ name: String) throws {
        try step("releasing the savepoint failed") { db_tx_release(handle, name, &$0) }
    }

    /// Runs a transaction call that answers with a code rather than a value.
    private func step(
        _ fallback: String, _ call: (inout UnsafeMutablePointer<CChar>?) -> Int32
    ) throws {
        var err: UnsafeMutablePointer<CChar>?
        if call(&err) != 0 {
            throw DbError(description: Database.take(&err) ?? fallback)
        }
    }

    /// Scratch storage for the C error out-parameter. Calls are serialized by
    /// the caller (all metadata access happens on one background queue), so a
    /// single slot is sufficient and keeps the call sites readable.
    private var errOut: UnsafeMutablePointer<CChar>?

    private func decodeJSON<T: Decodable>(
        _ raw: UnsafeMutablePointer<CChar>?, as type: T.Type
    ) throws -> T {
        guard let raw else {
            throw DbError(description: Database.take(&errOut) ?? "metadata call failed")
        }
        defer { db_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        return try JSONDecoder().decode(type, from: data)
    }

    func query(_ statement: String, batchRows: Int) throws -> Query {
        var err: UnsafeMutablePointer<CChar>?
        var position: Int32 = 0
        guard let q = db_query(handle, statement, batchRows, &err, &position) else {
            throw DbError(
                description: Database.take(&err) ?? "query failed",
                // The server counts from one, so zero is "nowhere in particular"
                // rather than "the first character".
                position: position > 0 ? Int(position) : nil)
        }
        return Query(handle: q)
    }

    /// Opens a server-side cursor over `statement`.
    ///
    /// What this buys over `query` is a stable position: the server holds one
    /// statement's snapshot open and hands out the next rows on request, so a
    /// second page cannot repeat or skip rows the way a second LIMIT/OFFSET
    /// statement can. It costs a connection for as long as the cursor lives.
    func cursor(_ statement: String, batchRows: Int) throws -> Cursor {
        var err: UnsafeMutablePointer<CChar>?
        var position: Int32 = 0
        guard let c = db_cursor(handle, statement, batchRows, &err, &position) else {
            throw DbError(
                description: Database.take(&err) ?? "cursor failed",
                position: position > 0 ? Int(position) : nil)
        }
        return Cursor(handle: c)
    }

    /// Starts moving this connection's rows into `table` on another connection.
    ///
    /// The cursor is made here and handed straight to the transfer, which owns
    /// it: there is no `Cursor` object in between, so there is nothing for two
    /// pieces of Swift to free. Stopping the fetch is `Transfer.cancel`, which
    /// reaches the write as well.
    ///
    /// Blocks only for as long as opening a cursor takes. The rows move one
    /// `step` at a time, which is what lets a caller show how far it has got.
    func startTransfer(of statement: String, batchRows: Int, into target: Database, table: String)
        throws -> Transfer
    {
        var err: UnsafeMutablePointer<CChar>?
        var position: Int32 = 0
        guard let c = db_cursor(handle, statement, batchRows, &err, &position) else {
            throw DbError(
                description: Database.take(&err) ?? "cursor failed",
                position: position > 0 ? Int(position) : nil)
        }
        guard let t = db_transfer_start(c, target.rawHandle, table, &err) else {
            // The cursor was refused before it was taken, so it is still ours.
            db_cursor_free(c)
            throw DbError(description: Database.take(&err) ?? "transfer failed")
        }
        return Transfer(handle: t)
    }

    /// Reads `url` into `table`, and answers how many rows arrived.
    ///
    /// On the connection rather than on a `Cursor`, because there is no result
    /// to read from: the rows are coming the other way. The core reads a batch,
    /// sends it and drops it, so the size of the file is the disk's problem.
    ///
    /// The table has to exist. Nothing infers a type from the file — the columns
    /// being read into already have types, and the core asks this connection for
    /// them before it parses a line.
    ///
    /// Blocks for the length of the import.
    func importFile(from url: URL, format: ExportFormat, table: String) throws -> Int64 {
        var err: UnsafeMutablePointer<CChar>?
        let rows = db_import(handle, format.wireName, url.path, table, &err)
        return try Cursor.written(rows, &err, or: "import failed")
    }

    /// Consumes an error out-parameter, releasing the Rust-owned string.
    fileprivate static func take(_ err: inout UnsafeMutablePointer<CChar>?) -> String? {
        guard let e = err else { return nil }
        let s = String(cString: e)
        db_string_free(e)
        err = nil
        return s
    }
}

/// A server-side cursor, read forward one page at a time.
///
/// `@unchecked Sendable` because the pointer is only ever used from the one
/// serial queue that owns the connection; the main actor does nothing with a
/// cursor but hold the reference and let it go.
final class Cursor: @unchecked Sendable {
    private let handle: OpaquePointer

    fileprivate init(handle: OpaquePointer) {
        self.handle = handle
    }

    /// Freeing an open cursor is the ordinary way one ends: it closes the
    /// connection the cursor was declared on, which is what rolls its
    /// transaction back. `db_cursor_close` exists for front-ends that want to
    /// close explicitly, but calling it here would mean waiting on the server
    /// from whatever thread released the last reference — including the main
    /// one.
    deinit { db_cursor_free(handle) }

    /// Next page, or nil once the cursor is exhausted. Ownership of the
    /// returned array transfers to the caller.
    func nextBatch() throws -> UnsafeMutablePointer<ArrowArray>? {
        let out = UnsafeMutablePointer<ArrowArray>.allocate(capacity: 1)
        var err: UnsafeMutablePointer<CChar>?
        switch db_cursor_next(handle, out, &err) {
        case 1:
            return out
        case 0:
            out.deallocate()
            return nil
        case -2:
            out.deallocate()
            throw DbError(
                description: Database.take(&err) ?? "cancelled", cancelled: true)
        default:
            out.deallocate()
            throw DbError(description: Database.take(&err) ?? "next page failed")
        }
    }

    /// Caller owns the returned schema and must release it.
    func schema() throws -> UnsafeMutablePointer<ArrowSchema> {
        let out = UnsafeMutablePointer<ArrowSchema>.allocate(capacity: 1)
        var err: UnsafeMutablePointer<CChar>?
        if db_cursor_schema(handle, out, &err) != 0 {
            out.deallocate()
            throw DbError(description: Database.take(&err) ?? "cursor schema failed")
        }
        return out
    }

    /// Drains this cursor into `url` and answers how many rows that was.
    ///
    /// Every row is formatted and written in the core. Doing it here would mean
    /// pulling the whole result into the app first, which is the limit this
    /// replaced: export used to write what the grid had loaded, so a table
    /// longer than the scrollback could not be written out at all.
    ///
    /// `rowLimit` of zero means all of them. A limit is how "only the rows
    /// shown here" is offered without a second writer to keep in step with this
    /// one — it is this code path, stopping early.
    ///
    /// Blocks for the length of the export. Stopping one is `cancel()`, the
    /// same call that stops any other fetch.
    func export(to url: URL, format: ExportFormat, rowLimit: Int64) throws -> Int64 {
        var err: UnsafeMutablePointer<CChar>?
        let rows = db_export(handle, format.wireName, url.path, rowLimit, &err)
        return try Cursor.written(rows, &err)
    }

    /// The same, as `INSERT` statements for `table`.
    ///
    /// Takes the connection as well, which the other formats do not need: an
    /// identifier and a string literal are spelled differently on each
    /// database, and only the connection knows which one this is.
    func exportSql(to url: URL, handle db: Database, table: String, rowLimit: Int64) throws -> Int64
    {
        var err: UnsafeMutablePointer<CChar>?
        let rows = db_export_sql(db.rawHandle, handle, table, url.path, rowLimit, &err)
        return try Cursor.written(rows, &err)
    }

    static func written(
        _ rows: Int64, _ err: inout UnsafeMutablePointer<CChar>?,
        or fallback: String = "export failed"
    ) throws -> Int64 {
        guard rows >= 0 else {
            throw DbError(
                description: Database.take(&err) ?? fallback,
                cancelled: rows == -2)
        }
        return rows
    }

    /// Asks the server to abandon the page this cursor is fetching.
    ///
    /// `Database.cancel()` does not reach here: it cancels the session
    /// connection, and a cursor runs on one of its own. Cancelling a browse
    /// means cancelling the cursor that is reading it.
    ///
    /// Called off the queue the fetch is blocking, and silent on failure, for
    /// the reasons `Database.cancel()` gives.
    func cancel() {
        var err: UnsafeMutablePointer<CChar>?
        if db_cursor_cancel(handle, &err) != 0 {
            let message = Database.take(&err) ?? "cancel failed"
            fputs("cursor cancel request failed: \(message)\n", stderr)
        }
    }
}

/// Rows on their way from one connection to another.
///
/// A handle rather than a call that returns when it is finished, which is the
/// difference between a transfer somebody can watch and one they can only wait
/// out: `step` moves one batch and answers with the running total, so the count
/// on screen is the count on the target.
final class Transfer: @unchecked Sendable {
    /// What one step did.
    enum Step {
        /// A batch went across; the number is the running total.
        case moved(Int64)
        /// Everything the source had is on the target.
        case done(Int64)
        /// Somebody stopped it. What had already gone stays there — a transfer
        /// is not a transaction, and the target may not have one.
        case stopped(Int64)

        var rows: Int64 {
            switch self {
            case .moved(let n), .done(let n), .stopped(let n): return n
            }
        }

        var isFinished: Bool {
            if case .moved = self { return false }
            return true
        }
    }

    private let handle: OpaquePointer

    fileprivate init(handle: OpaquePointer) {
        self.handle = handle
    }

    /// Frees the transfer and the cursor it took. Never on the main thread for
    /// the reason `Cursor.deinit` gives: closing the cursor closes a connection.
    deinit { db_transfer_free(handle) }

    /// Moves one batch. Blocks for one fetch and one round of INSERTs.
    func step() throws -> Step {
        var rows: Int64 = 0
        var err: UnsafeMutablePointer<CChar>?
        switch db_transfer_step(handle, &rows, &err) {
        case 1:
            return .moved(rows)
        case 0:
            return .done(rows)
        case -2:
            // Not thrown. A stop is somebody pressing Stop, and the rows that
            // did arrive are the answer — a throw here would send the caller
            // down the failure path for a button that worked.
            _ = Database.take(&err)
            return .stopped(rows)
        default:
            throw DbError(description: Database.take(&err) ?? "transfer failed")
        }
    }

    /// Stops it, at both ends.
    ///
    /// Called from off the queue the step is blocking, and silent on failure,
    /// for the reasons `Cursor.cancel` gives. What is promised is that no
    /// further batch is sent.
    func cancel() {
        var err: UnsafeMutablePointer<CChar>?
        if db_transfer_cancel(handle, &err) != 0 {
            let message = Database.take(&err) ?? "cancel failed"
            fputs("transfer cancel request failed: \(message)\n", stderr)
        }
    }
}

final class Query {
    private let handle: OpaquePointer

    fileprivate init(handle: OpaquePointer) {
        self.handle = handle
    }

    deinit { db_query_free(handle) }

    /// Caller owns the returned schema and must release it.
    func schema() throws -> UnsafeMutablePointer<ArrowSchema> {
        let out = UnsafeMutablePointer<ArrowSchema>.allocate(capacity: 1)
        var err: UnsafeMutablePointer<CChar>?
        if db_query_schema(handle, out, &err) != 0 {
            out.deallocate()
            throw DbError(description: Database.take(&err) ?? "schema failed")
        }
        return out
    }

    /// Rows the server said this statement affected, or nil until the result has
    /// been read to the end.
    ///
    /// What a statement returning no rows has to say for itself: an UPDATE
    /// reports what it touched, a CREATE reports zero and still happened. The
    /// verb is not part of it — the driver keeps the count out of the command
    /// tag and drops the rest — so a caller that wants to say "UPDATE 3" would
    /// have to invent the word, and this is not the place that gets to.
    var rowsAffected: Int? {
        let n = db_query_rows_affected(handle)
        return n < 0 ? nil : Int(n)
    }

    /// Next batch, or nil when the result is exhausted. Ownership of the
    /// returned array transfers to the caller.
    ///
    /// Where a statement fails, more often than not. `Database.query` returns
    /// once the server has acknowledged the bind, which is before it executes
    /// anything, so a duplicate relation or a violated constraint surfaces from
    /// here rather than from there. A statement with no rows to fetch still has
    /// to be pulled once for that reason.
    func nextBatch() throws -> UnsafeMutablePointer<ArrowArray>? {
        let out = UnsafeMutablePointer<ArrowArray>.allocate(capacity: 1)
        var err: UnsafeMutablePointer<CChar>?
        switch db_query_next(handle, out, &err) {
        case 1:
            return out
        case 0:
            out.deallocate()
            return nil
        case -2:
            out.deallocate()
            throw DbError(
                description: Database.take(&err) ?? "cancelled", cancelled: true)
        default:
            out.deallocate()
            throw DbError(description: Database.take(&err) ?? "next batch failed")
        }
    }
}
