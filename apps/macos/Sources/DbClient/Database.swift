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

    init(connString: String) throws {
        var err: UnsafeMutablePointer<CChar>?
        guard let h = db_connect(connString, &err) else {
            throw DbError(description: Database.take(&err) ?? "connect failed")
        }
        handle = h
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

    // MARK: - Metadata

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

    /// The statement a view is defined by, or nil for a relation that has none.
    /// The one metadata call that answers with a value rather than a list, so
    /// it decodes a bare JSON string instead of an array of mirrored structs.
    func definition(schema: String, relation: String) throws -> String? {
        try decodeJSON(
            db_definition_json(handle, schema, relation, &errOut), as: String?.self)
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

    func query(_ sql: String, batchRows: Int) throws -> Query {
        var err: UnsafeMutablePointer<CChar>?
        var position: Int32 = 0
        guard let q = db_query(handle, sql, batchRows, &err, &position) else {
            throw DbError(
                description: Database.take(&err) ?? "query failed",
                // The server counts from one, so zero is "nowhere in particular"
                // rather than "the first character".
                position: position > 0 ? Int(position) : nil)
        }
        return Query(handle: q)
    }

    /// Opens a server-side cursor over `sql`.
    ///
    /// What this buys over `query` is a stable position: the server holds one
    /// statement's snapshot open and hands out the next rows on request, so a
    /// second page cannot repeat or skip rows the way a second LIMIT/OFFSET
    /// statement can. It costs a connection for as long as the cursor lives.
    func cursor(_ sql: String, batchRows: Int) throws -> Cursor {
        var err: UnsafeMutablePointer<CChar>?
        var position: Int32 = 0
        guard let c = db_cursor(handle, sql, batchRows, &err, &position) else {
            throw DbError(
                description: Database.take(&err) ?? "cursor failed",
                position: position > 0 ? Int(position) : nil)
        }
        return Cursor(handle: c)
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
