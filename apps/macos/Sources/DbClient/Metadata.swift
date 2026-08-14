import Foundation

// Mirrors of the core's metadata types. Field names match the Rust structs'
// serde output, so a rename on either side fails to decode rather than silently
// producing empty values.

struct SchemaInfo: Decodable, Hashable, Identifiable {
    let name: String
    var id: String { name }
}

enum RelationKind: String, Decodable, Hashable {
    case table
    case view
    case materializedView = "materializedview"
    case foreignTable = "foreigntable"
    case partitionedTable = "partitionedtable"
    case unknown

    /// SF Symbol used in the navigator. Views read differently from tables
    /// because what you may do to them differs.
    var symbol: String {
        switch self {
        case .table, .partitionedTable: return "tablecells"
        case .view, .materializedView: return "eye"
        case .foreignTable: return "link"
        case .unknown: return "questionmark.square"
        }
    }

    var label: String {
        switch self {
        case .table: return "Table"
        case .view: return "View"
        case .materializedView: return "Materialized View"
        case .foreignTable: return "Foreign Table"
        case .partitionedTable: return "Partitioned Table"
        case .unknown: return "Relation"
        }
    }
}

struct RelationInfo: Decodable, Hashable, Identifiable {
    let schema: String
    let name: String
    let kind: RelationKind
    /// Planner estimate, not an exact count — an exact count needs a scan,
    /// which a navigator cannot afford. Presented as approximate in the UI.
    ///
    /// Absent where the server has no estimate to give: a view has none, and
    /// neither has a table nothing has analysed yet. Optional rather than zero,
    /// because zero is a real answer — an empty table — and a client that
    /// spelled "unknown" with it would announce every view as empty.
    let estimatedRows: Int64?

    var id: String { "\(schema).\(name)" }
    /// Identifier safe to interpolate into SQL.
    var qualifiedName: String { "\"\(schema)\".\"\(name)\"" }

    /// The size as the navigator writes it, or nil where there is nothing to
    /// write. Marked approximate wherever it appears, because it is.
    ///
    /// On the main actor because the number formatter it shares with the status
    /// bar is: one place decides what a thousands separator looks like.
    @MainActor var rowsLabel: String? {
        guard let estimatedRows, estimatedRows > 0 else { return nil }
        return "~\(AppModel.formatted(estimatedRows))"
    }

    private enum CodingKeys: String, CodingKey {
        case schema, name, kind
        case estimatedRows = "estimated_rows"
    }
}

struct IndexInfo: Decodable, Hashable, Identifiable {
    let name: String
    /// Key expressions in index order, not plain column names: an index on
    /// `lower(email)` is not an index on `email`.
    let columns: [String]
    let isUnique: Bool
    let isPrimary: Bool
    let method: String
    /// WHERE clause of a partial index.
    let predicate: String?

    var id: String { name }

    /// "UNIQUE · btree", or just the method. The primary key gets its own
    /// column, so repeating "unique" for it would say the same thing twice.
    var kindLabel: String {
        isUnique && !isPrimary ? "UNIQUE · \(method)" : method
    }

    private enum CodingKeys: String, CodingKey {
        case name, columns, method, predicate
        case isUnique = "is_unique"
        case isPrimary = "is_primary"
    }
}

/// One foreign key, from the vantage point of the relation that was asked
/// about. The same constraint is this table's own key when read from the
/// referencing side and an inbound reference when read from the referenced
/// side, so `local` and `other` are accurate in both directions where
/// "referenced" would be wrong half the time.
struct RelationshipInfo: Decodable, Hashable, Identifiable {
    let name: String
    let localColumns: [String]
    let otherSchema: String
    let otherTable: String
    let otherColumns: [String]
    let onUpdate: String
    let onDelete: String

    var id: String { "\(otherSchema).\(otherTable).\(name)" }

    /// `orders(id)` — the schema is dropped when it matches the relation being
    /// viewed, which is the ordinary case and reads as noise on every row.
    func otherLabel(sameSchemaAs schema: String) -> String {
        let table = otherSchema == schema ? otherTable : "\(otherSchema).\(otherTable)"
        return "\(table)(\(otherColumns.joined(separator: ", ")))"
    }

    /// Only the actions that are not the default, so a row says something.
    var actionLabel: String {
        [("ON UPDATE", onUpdate), ("ON DELETE", onDelete)]
            .filter { $0.1 != "NO ACTION" }
            .map { "\($0.0) \($0.1)" }
            .joined(separator: " · ")
    }

    private enum CodingKeys: String, CodingKey {
        case name
        case localColumns = "local_columns"
        case otherSchema = "other_schema"
        case otherTable = "other_table"
        case otherColumns = "other_columns"
        case onUpdate = "on_update"
        case onDelete = "on_delete"
    }
}

struct ConstraintInfo: Decodable, Hashable, Identifiable {
    enum Kind: String, Decodable {
        case check, unique, exclude, other

        var label: String {
            switch self {
            case .check: return "CHECK"
            case .unique: return "UNIQUE"
            case .exclude: return "EXCLUDE"
            case .other: return "—"
            }
        }
    }

    let name: String
    let kind: Kind
    /// The server's own rendering. Shown verbatim rather than rebuilt.
    let definition: String

    var id: String { name }
}

struct TriggerInfo: Decodable, Hashable, Identifiable {
    let name: String
    let timing: String
    let events: [String]
    let level: String
    let function: String
    let enabled: Bool

    var id: String { name }

    /// "BEFORE INSERT, UPDATE · ROW".
    var whenLabel: String {
        "\(timing) \(events.joined(separator: ", ")) · \(level)"
    }
}

struct ColumnInfo: Decodable, Hashable, Identifiable {
    let name: String
    let dataType: String
    let nullable: Bool
    let position: Int32
    let isPrimaryKey: Bool
    let defaultValue: String?

    var id: Int32 { position }

    private enum CodingKeys: String, CodingKey {
        case name, nullable, position
        case dataType = "data_type"
        case isPrimaryKey = "is_primary_key"
        case defaultValue = "default_value"
    }
}

/// What the connection's transaction is doing.
///
/// `transactional` decides whether the rest is worth showing at all: a
/// connection that cannot hold a transaction open has no mode, rather than being
/// permanently in autocommit. Today only PostgreSQL and the databases reached
/// through its driver answer yes — the others run each statement on a connection
/// from a pool, where a transaction could not span two of them.
struct TransactionState: Decodable, Hashable {
    let transactional: Bool
    let autocommit: Bool
    /// Whether there is work the server has not been told to keep.
    let open: Bool
    /// Innermost last, which is the order they can be rolled back to in.
    let savepoints: [String]

    /// Before anything is connected, and for a database with no transactions to
    /// control.
    static let none = TransactionState(
        transactional: false, autocommit: true, open: false, savepoints: [])
}
