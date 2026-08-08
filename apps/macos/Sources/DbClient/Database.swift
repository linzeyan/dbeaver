import CDbFfi
import Foundation

struct DbError: Error, CustomStringConvertible {
    let description: String
}

/// Swift wrapper over the core's C surface.
///
/// Every call blocks, so nothing here may run on the main thread. Phase 1
/// replaces this with an event-queue design; the blocking shape is kept for now
/// because it is small enough to audit by reading it.
final class Database {
    private let handle: OpaquePointer

    init(connString: String) throws {
        var err: UnsafeMutablePointer<CChar>?
        guard let h = db_connect(connString, &err) else {
            throw DbError(description: Database.take(&err) ?? "connect failed")
        }
        handle = h
    }

    deinit { db_free(handle) }

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

    func foreignKeys(schema: String, relation: String) throws -> [ForeignKeyInfo] {
        try decodeJSON(
            db_foreign_keys_json(handle, schema, relation, &errOut), as: [ForeignKeyInfo].self)
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
        guard let q = db_query(handle, sql, batchRows, &err) else {
            throw DbError(description: Database.take(&err) ?? "query failed")
        }
        return Query(handle: q)
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

    /// Next batch, or nil when the result is exhausted. Ownership of the
    /// returned array transfers to the caller.
    func nextBatch() throws -> UnsafeMutablePointer<ArrowArray>? {
        let out = UnsafeMutablePointer<ArrowArray>.allocate(capacity: 1)
        var err: UnsafeMutablePointer<CChar>?
        switch db_query_next(handle, out, &err) {
        case 1:
            return out
        case 0:
            out.deallocate()
            return nil
        default:
            out.deallocate()
            throw DbError(description: Database.take(&err) ?? "next batch failed")
        }
    }
}
