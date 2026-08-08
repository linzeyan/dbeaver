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
    let estimatedRows: Int64

    var id: String { "\(schema).\(name)" }
    /// Identifier safe to interpolate into SQL.
    var qualifiedName: String { "\"\(schema)\".\"\(name)\"" }

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

struct ForeignKeyInfo: Decodable, Hashable, Identifiable {
    let name: String
    let columns: [String]
    let referencedSchema: String
    let referencedTable: String
    let referencedColumns: [String]
    let onUpdate: String
    let onDelete: String

    var id: String { name }

    /// `public.orders(id)` — the schema is dropped when it is the same one the
    /// referencing table lives in, which is the ordinary case and reads as
    /// noise when spelled out on every row.
    func targetLabel(sameSchemaAs schema: String) -> String {
        let table = referencedSchema == schema
            ? referencedTable : "\(referencedSchema).\(referencedTable)"
        return "\(table)(\(referencedColumns.joined(separator: ", ")))"
    }

    /// Only the actions that are not the default, so a row says something.
    var actionLabel: String {
        [("ON UPDATE", onUpdate), ("ON DELETE", onDelete)]
            .filter { $0.1 != "NO ACTION" }
            .map { "\($0.0) \($0.1)" }
            .joined(separator: " · ")
    }

    private enum CodingKeys: String, CodingKey {
        case name, columns
        case referencedSchema = "referenced_schema"
        case referencedTable = "referenced_table"
        case referencedColumns = "referenced_columns"
        case onUpdate = "on_update"
        case onDelete = "on_delete"
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
