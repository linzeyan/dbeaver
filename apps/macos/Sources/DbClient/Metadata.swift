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

/// A trigger, in as much detail as the database records.
///
/// Three of these are optional and every one of them arrives null from some
/// database the build can open: PostgreSQL keeps the timing, the events, the
/// level and the function in columns of `pg_trigger`, while MySQL keeps the
/// statement the trigger was created from and none of the four. Declaring them
/// non-optional is what made a MySQL table with a trigger impossible to browse —
/// the decode threw, and the failure took the browse that was in flight with it.
struct TriggerInfo: Decodable, Hashable, Identifiable {
    let name: String
    /// BEFORE, AFTER, or INSTEAD OF.
    let timing: String?
    let events: [String]
    /// ROW or STATEMENT.
    let level: String?
    /// The function it calls, where it calls one rather than carrying a body.
    let function: String?
    let enabled: Bool
    /// The statement it was created from. Every driver fills this, which is what
    /// makes it the thing to show where the descriptors above are missing.
    let definition: String?

    var id: String { name }

    /// "BEFORE INSERT, UPDATE · ROW", with whatever of it the database knows.
    var whenLabel: String {
        [timing, events.isEmpty ? nil : events.joined(separator: ", ")]
            .compactMap { $0 }
            .joined(separator: " ")
            + (level.map { " · \($0)" } ?? "")
    }

    /// What the trigger runs: the function it names, or the body it carries.
    ///
    /// One column rather than two, because no database fills both and a column
    /// that is empty for every row of every MySQL table is a column that costs
    /// width to say nothing.
    var runsLabel: String {
        if let function { return "\(function)()" }
        return definition?.split(separator: "\n").joined(separator: " ") ?? "—"
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

/// Which columns name one row of a relation, as the core decides it.
///
/// Asked for rather than worked out from `ColumnInfo.isPrimaryKey`, which is
/// what this side used to do. The rule is no longer one field — a table with no
/// primary key is named by a UNIQUE constraint whose columns cannot be null, and
/// choosing among several of those is a decision with an order to it — and a
/// copy of that here would be a second answer to a question the core already
/// answers, disagreeing with it the day either is corrected.
struct RowIdentity: Decodable, Hashable {
    /// The columns whose values name one row, in key order. Empty where nothing
    /// does, which is the same question as whether the table can be edited.
    let columns: [String]
    /// Why there is nothing, naming the table and any constraint that had to be
    /// turned down. `nil` where `columns` is not empty.
    let obstacle: String?
}

/// What a filter may ask of a column, spelled as the core's JSON.
///
/// The raw values are the wire in both directions — `db_filter_columns_json`
/// hands back exactly these words and `db_filter_clause` reads exactly these
/// words — so a spelling invented here would be a request the core turns down at
/// run time rather than a mistake the compiler catches.
///
/// Which of them a column is offered is the core's answer and not this side's,
/// and the answer is `FilterColumn.operators`. The first four are offered over
/// anything; the rest depend on what the type holds and, for the three `LIKE`
/// ones, on whether this database can be told how to escape a wildcard.
enum FilterOperator: String, Codable, Sendable {
    case equals
    case notEquals = "not_equals"
    case isNull = "is_null"
    case isNotNull = "is_not_null"
    case lessThan = "less_than"
    case lessOrEqual = "less_or_equal"
    case greaterThan = "greater_than"
    case greaterOrEqual = "greater_or_equal"
    case between
    case contains
    case startsWith = "starts_with"
    case endsWith = "ends_with"
}

/// One column a relation can be filtered on, and the questions worth asking of
/// it.
struct FilterColumn: Decodable, Hashable, Identifiable {
    let name: String
    /// The type as the server declared it, so a row can print it beside the
    /// name. That is what makes an operator list shorter than the next column's
    /// read as a consequence of the type rather than as something missing.
    let dataType: String
    let operators: [FilterOperator]

    var id: String { name }

    private enum CodingKeys: String, CodingKey {
        case name, operators
        case dataType = "data_type"
    }
}

/// One row of a filter: a column, what is asked of it, and the text typed in.
///
/// `Encodable` because it is also the wire. `db_filter_clause` reads exactly
/// this shape, so the row on screen and the rule sent over are one value rather
/// than two that have to be kept in step.
struct FilterRule: Encodable, Hashable {
    var column: String
    var op: FilterOperator
    /// The text as typed, never quoted: the quoting is the core's, and doing it
    /// here as well is how a filter starts matching literal apostrophes. `nil`
    /// for `isNull` and `isNotNull`, which compare against nothing.
    var value: String?
    /// The far end of a `between`, and `nil` for every other operator.
    var second: String?
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
