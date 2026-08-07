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
