import Foundation

// Mirrors of the core's metadata types. Field names match the Rust structs'
// serde output, so a rename on either side fails to decode rather than silently
// producing empty values.

/// What answered a connection: the product on the other end, and its version.
///
/// Asked rather than taken from the scheme, which names a wire protocol: a
/// `postgres://` connection may be CockroachDB or GreptimeDB, and a window that
/// labelled it PostgreSQL would be naming the driver and calling it the
/// database.
struct ServerInfo: Decodable, Hashable {
    let product: String
    /// Empty where the server states none. Several of the databases here have no
    /// version to report at all, and each driver's own source says why.
    let version: String

    /// The two as one line to put on screen — "PostgreSQL 17.0", or the product
    /// alone where there is no version to add.
    var label: String {
        version.isEmpty ? product : "\(product) \(version)"
    }
}

/// What a connection can do, asked once rather than discovered by being refused.
///
/// The alternative is what this replaces: draw the control, send the request,
/// and read the answer out of whatever comes back. That works for finding out
/// what a *statement* did and not for finding out what a *database is*, because
/// the answer arrives after the control has already been drawn — and a control
/// that is drawn and then apologises has already made a promise.
struct Capabilities: Codable, Hashable {
    /// Whether this connection can be taken out of autocommit at all.
    let transactional: Bool

    /// Whether Cancel reaches the statement, or only this side's reading of it.
    ///
    /// False for Cassandra and Flight SQL, where there is nowhere to deliver the
    /// request: the fetch in flight resolves as cancelled and the server goes on
    /// assembling a page nobody will read. The window says so rather than
    /// claiming the work stopped.
    let cancelStopsTheStatement: Bool

    /// Whether an entry of `databases()` is another container on this
    /// connection, rather than somewhere to open a new one.
    ///
    /// True for DuckDB, where the entries are attached catalogs and often have
    /// no file the connection string could name. False for PostgreSQL, which
    /// settles its database at connect, and for SQL Server, whose driver reads
    /// its catalog through a pool a `USE` would not reach — so both of those are
    /// moved by opening a connection with the other name in the string, which is
    /// what `AppModel.switchDatabase` does when this is false.
    let switchesDatabase: Bool

    /// Whether the core can write statements for this database, as opposed to
    /// running the ones somebody typed.
    ///
    /// False for Redis, MongoDB, Cassandra and every other connection whose
    /// dialect this build does not carry. It does not mean the connection is
    /// read-only or half-working — a Query pane runs whatever is typed into it —
    /// it means the controls that *compose* a statement have nothing to compose
    /// one in. The grid's editing row is the one that has to say so: it used to
    /// draw Set and Delete Row and hand back `ERR unknown command 'UPDATE'`
    /// after they were pressed.
    let writesStatements: Bool

    /// Whether the core can write the statements a grid's staged changes need.
    ///
    /// Wider than `writesStatements`, and the reason that one is no longer the
    /// grid's question: an edit needs a grammar to say "set this field of that
    /// row", and a SQL dialect is one way to have one but not the only way.
    /// MongoDB says it in a command document and Redis in a command, and both
    /// are written by their own driver.
    ///
    /// Still false for Cassandra, which has neither — and false is the answer
    /// the editing row draws its sentence from, because a Set button that hands
    /// back `ERR unknown command 'UPDATE'` after it is pressed is worse than a
    /// row that says why it is not there.
    let editsRows: Bool

    /// Whether the level `schemas()` reports is what this engine calls a
    /// database.
    ///
    /// True for MySQL, ClickHouse, MongoDB, SQLite, Redis and Athena, where
    /// there is one level of container and the engine's own word for it is
    /// "database" — a MySQL `SCHEMA` is a `DATABASE`, and Redis's are numbered.
    /// The tree draws the same level either way; this decides the icon on it and
    /// the word every sentence about it uses. See `AppModel.containerNoun`.
    let schemaIsTheDatabase: Bool

    /// Whether this connection can list its functions and procedures.
    ///
    /// False covers two unrelated cases and the navigator must not tell them
    /// apart by guessing: Redis and MongoDB have no such object, while SQL
    /// Server and Snowflake have plenty and this build has not been taught to
    /// read them. Either way the group is not drawn — an empty `Routines` node
    /// under a schema full of them is a claim, not a blank.
    let reportsRoutines: Bool

    /// Whether this connection can list its sequences.
    ///
    /// Its own flag and not a second reading of `reportsRoutines`: the two do
    /// not travel together, and MySQL is the case that proves it — routines yes,
    /// sequences no, an `AUTO_INCREMENT` being a property of a column rather
    /// than an object in a catalog.
    let reportsSequences: Bool

    /// How far this connection can see into what the server itself is doing.
    ///
    /// One value rather than three flags, because the states are a ladder and
    /// the combinations off it do not exist: nothing can stop a statement it
    /// cannot list. See `ServerProcesses`.
    let serverProcesses: ServerProcesses

    /// Whether the settings the server is running with can be listed.
    ///
    /// A flag and not a ladder, because there is only the one verb: the list is
    /// read and nothing is done to a row. See `ServerVariable`.
    let reportsVariables: Bool

    /// Whether the core writes a drop, an empty or a rename for this database.
    ///
    /// Narrower than `writesStatements` and not implied by it: every dialect the
    /// core carries can have a `SELECT` composed for it, and only three of the
    /// six have had these three written. The navigator's row menu is built from
    /// this, before any relation has been chosen — three items that refuse
    /// whichever is clicked would be a menu that lies about what it does. What a
    /// *particular* relation will take is answered later, in the sheet, where
    /// the reason appears in place of the statement.
    let changesRelations: Bool

    /// Whether the core writes an add, a drop or a rename for a column.
    ///
    /// Its own flag rather than a second reading of `changesRelations`, though
    /// the two answer alike today. They are not one question, and upstream is
    /// where that shows: DBeaver writes SQLite's `DROP TABLE` and refuses its
    /// column drop outright, recreating the whole table instead. What the
    /// Structure tab draws comes from this; what a *particular* relation will
    /// take — no server alters a view's columns — is answered in the sheet.
    let changesColumns: Bool

    /// Whether the core writes a statement that alters a column's own definition
    /// — its type, whether it takes a null, its default.
    ///
    /// The line between this and `changesColumns` is which columns a table has
    /// against what one of them is, and SQLite is why it is drawn there: its
    /// `ALTER TABLE` adds, drops and renames a column and reaches nothing inside
    /// one. The Edit Column item comes from this flag alone, so that it is not a
    /// menu item that refuses every time it is clicked.
    let altersColumns: Bool

    /// Whether the core writes a `CREATE INDEX` or a drop for this database.
    ///
    /// Its own flag again: an index is a different object from the table it is
    /// on, and this build lights the families one at a time.
    let changesIndexes: Bool

    /// The access methods to offer for a new index here, in the order to show
    /// them, and empty where no picker should be drawn.
    ///
    /// A list rather than a flag because the answer is neither yes nor no — a
    /// method is per server, and `gin` named for MySQL is a statement that reads
    /// correctly and is refused. Empty means "take the server's default", which
    /// is not the same as the server having one: MySQL takes `USING HASH` and
    /// InnoDB ignores it, so it is left out rather than offered and discarded.
    let indexMethods: [String]

    /// Whether the core writes an `ADD CONSTRAINT` or a `DROP` for one.
    ///
    /// Its own flag and not a second reading of `changesIndexes`, and SQLite is
    /// what keeps the two apart in fact rather than in doctrine: it makes and
    /// drops an index, and its `ALTER TABLE` cannot write two of the three
    /// constraints. The Structure tab's constraint and foreign key menus come
    /// from this alone, so that a SQLite table does not get an Add Foreign Key
    /// that refuses every time it is clicked.
    let changesConstraints: Bool

    /// Whether the core writes a statement that makes or drops a whole database.
    ///
    /// Not a second reading of `changesRelations`, and SQLite is what keeps them
    /// apart: it drops and renames a table, and a SQLite database is a file —
    /// made by opening a path and removed by deleting one. One flag for both
    /// would have to be wrong about one of them.
    let changesDatabases: Bool

    /// What a window has before it has asked.
    ///
    /// All false, which is the cautious reading in every direction: it offers no
    /// transaction control it might not have, promises no cancel it might not be
    /// able to deliver, claims no switch it might have to take back, draws no
    /// control that writes, calls the level it has not read yet by the neutral
    /// word, lists no routines it has not been told are there, and does not
    /// offer to show a server's activity before the server has said it keeps
    /// any, nor its settings before it has said it will hand them over, and
    /// offers to change no relation before it knows it can write the statement.
    static let unknown = Capabilities(
        transactional: false, cancelStopsTheStatement: false, switchesDatabase: false,
        writesStatements: false, editsRows: false, schemaIsTheDatabase: false,
        reportsRoutines: false,
        reportsSequences: false, serverProcesses: .unreported, reportsVariables: false,
        changesRelations: false, changesColumns: false, altersColumns: false,
        changesIndexes: false, indexMethods: [], changesConstraints: false,
        changesDatabases: false)

    private enum CodingKeys: String, CodingKey {
        case transactional
        case cancelStopsTheStatement = "cancel_stops_the_statement"
        case switchesDatabase = "switches_database"
        case writesStatements = "writes_statements"
        case editsRows = "edits_rows"
        case schemaIsTheDatabase = "schema_is_the_database"
        case reportsRoutines = "reports_routines"
        case reportsSequences = "reports_sequences"
        case serverProcesses = "server_processes"
        case reportsVariables = "reports_variables"
        case changesRelations = "changes_relations"
        case changesColumns = "changes_columns"
        case altersColumns = "alters_columns"
        case changesIndexes = "changes_indexes"
        case indexMethods = "index_methods"
        case changesConstraints = "changes_constraints"
        case changesDatabases = "changes_databases"
    }
}

/// What is being done to a whole database.
///
/// Two and not three, as the core's own enum is: a rename is refused outright by
/// upstream's MySQL manager, and PostgreSQL's needs a connection to some *other*
/// database — which is exactly the connection a window pointed at this one does
/// not have. The raw values are the words the C boundary takes.
enum DatabaseChange: String, Codable, Hashable, CaseIterable {
    /// Make one, empty.
    case create
    /// Remove one and everything in it.
    case drop

    /// What the menu item says.
    ///
    /// Always "database", and deliberately not `AppModel.containerNoun`: that
    /// property names the *schema* level, which PostgreSQL calls a schema and
    /// MySQL calls a database. What these two act on is the database level on
    /// both — the row above the schemas on PostgreSQL and the schema rows
    /// themselves on MySQL, which the tree already labels databases. Borrowing
    /// the other noun put "A schema needs a name." on a `CREATE DATABASE`.
    var menuTitle: String {
        switch self {
        case .create: return "New Database…"
        case .drop: return "Drop Database…"
        }
    }

    /// What the button that runs it says. No ellipsis: this one is the doing.
    var actionTitle: String {
        switch self {
        case .create: return "Create"
        case .drop: return "Drop"
        }
    }

    /// What the status line says while it is happening, and afterwards.
    var progressive: String { self == .create ? "Creating" : "Dropping" }
    var pastTense: String { self == .create ? "Created" : "Dropped" }

    /// Whether pressing the button loses something that cannot be got back.
    var isDestructive: Bool { self == .drop }
}

/// What is being done to a relation that already exists.
///
/// The raw values are the words the C boundary takes, spelled rather than
/// numbered for the reason `EndProcess` spells its two: these are one argument
/// apart and two of them are irreversible. See `db_table_change_sql`.
enum TableChange: String, Codable, Hashable, CaseIterable {
    /// Remove the relation and everything in it.
    case drop
    /// Remove every row and leave the relation standing.
    case truncate
    /// Give it another name, in the schema it is already in.
    case rename

    /// What the menu item says.
    ///
    /// "Empty" rather than "Truncate" because the menu is read before the
    /// statement is: `TRUNCATE` is the word the server takes and "empty" is what
    /// it does, and the sheet shows the statement anyway.
    var menuTitle: String {
        switch self {
        case .drop: return "Drop…"
        case .truncate: return "Empty…"
        case .rename: return "Rename…"
        }
    }

    /// What the button that runs it says. No ellipsis: this one is the doing.
    var actionTitle: String {
        switch self {
        case .drop: return "Drop"
        case .truncate: return "Empty"
        case .rename: return "Rename"
        }
    }

    /// What the status line says while it is happening.
    var progressive: String {
        switch self {
        case .drop: return "Dropping"
        case .truncate: return "Emptying"
        case .rename: return "Renaming"
        }
    }

    /// What the status line says once it has happened. Both tenses are spelled
    /// out rather than derived from `actionTitle`, English being what it is.
    var pastTense: String {
        switch self {
        case .drop: return "Dropped"
        case .truncate: return "Emptied"
        case .rename: return "Renamed"
        }
    }

    /// Whether pressing the button loses something that cannot be got back.
    ///
    /// Both destructive ones say so the same way and the rename does not, which
    /// is the distinction worth drawing on a sheet whose three shapes are
    /// otherwise identical.
    var isDestructive: Bool { self != .rename }
}

/// What a column of a new table can be asked to hold.
///
/// Seven kinds and not a free-text type field, which is what most tools offer.
/// A type typed by hand is SQL, and spelling SQL for a database this side does
/// not know is the mistake the whole boundary exists to avoid: `nvarchar(max)`
/// on SQL Server is `text` on PostgreSQL is `String` on ClickHouse. What crosses
/// is the kind, and the core writes the word its own server reads.
///
/// The cost is the ceiling — no `varchar(64)`, no `uuid`, no `jsonb` — which is
/// what a form for making a table quickly should cost. Anything past it is
/// written in the SQL editor, which is a tab away and takes the whole language.
///
/// The raw values are what `db_new_table_sql` reads. `decimal` is the one kind
/// that carries a size, because a decimal held at another scale is a different
/// number and no server will mention it.
enum ColumnKind: Hashable, Encodable {
    case text
    case int
    case float
    case decimal(precision: Int, scale: Int)
    case bool
    case date
    case timestamp

    /// The word the core reads, which is the case name plus a decimal's size.
    ///
    /// Encoded as this single string rather than as a struct, so that the wire
    /// form has one shape for seven kinds and the core's `ColumnKind::parse` is
    /// the only thing that reads it.
    var word: String {
        switch self {
        case .text: return "text"
        case .int: return "int"
        case .float: return "float"
        case .decimal(let precision, let scale): return "decimal(\(precision),\(scale))"
        case .bool: return "bool"
        case .date: return "date"
        case .timestamp: return "timestamp"
        }
    }

    /// What the picker says, which is the noun rather than the word.
    ///
    /// "Whole number" and not `int`: the picker is read by somebody deciding
    /// what a column is for, and the type name they would have typed is the one
    /// this build is not asking them to know. The same four words the file
    /// import already infers in, so that the two features name one thing once.
    ///
    /// A decimal's size is not in the label. The two fields beside the picker
    /// carry it, and saying it twice made the row too narrow to read either.
    var label: String {
        switch self {
        case .text: return "Text"
        case .int: return "Whole number"
        case .float: return "Number"
        case .decimal: return "Exact decimal"
        case .bool: return "True or false"
        case .date: return "Date"
        case .timestamp: return "Date and time"
        }
    }

    /// The kinds the picker offers, in the order it offers them.
    ///
    /// Text first because it takes anything, which is what somebody unsure of a
    /// column wants. The decimal's size is the one number here that had to be
    /// picked rather than read: 18 digits with 4 after the point holds money in
    /// any currency and every quantity a form like this is used for, and the two
    /// steppers beside the picker are how it is changed.
    static let offered: [ColumnKind] = [
        .text, .int, .float, .decimal(precision: 18, scale: 4), .bool, .date, .timestamp
    ]

    /// Whether this is the same kind as `other`, a decimal's size aside.
    ///
    /// What the picker's selection is compared with: `decimal(18, 4)` and
    /// `decimal(12, 2)` are one row of the menu and two values, so `==` would
    /// leave the row unselected the moment a stepper moved.
    func isSameKind(as other: ColumnKind) -> Bool {
        switch (self, other) {
        case (.decimal, .decimal): return true
        default: return self == other
        }
    }

    /// The size, for the kind that has one.
    var decimalSize: (precision: Int, scale: Int)? {
        guard case .decimal(let precision, let scale) = self else { return nil }
        return (precision, scale)
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(word)
    }

    /// The kind `word` names, which is `word`'s inverse.
    ///
    /// Nothing on this side decodes a kind — they only ever go out — so this
    /// exists for the checks: a spelling that changed on one side of the
    /// boundary is a column silently refused, and the pair is what makes that
    /// testable without a connection.
    init?(word: String) {
        switch word {
        case "text": self = .text
        case "int": self = .int
        case "float": self = .float
        case "bool": self = .bool
        case "date": self = .date
        case "timestamp": self = .timestamp
        default:
            guard word.hasPrefix("decimal("), word.hasSuffix(")") else { return nil }
            let size = word.dropFirst("decimal(".count).dropLast().split(separator: ",")
            guard size.count == 2, let precision = Int(size[0]), let scale = Int(size[1]) else {
                return nil
            }
            self = .decimal(precision: precision, scale: scale)
        }
    }
}

/// One column of a table that does not exist yet.
///
/// Encoded straight to what `db_new_table_sql` reads, so the keys are the core's
/// spelling rather than Swift's. `id` is not encoded: it exists so that the form
/// can reorder and delete rows without SwiftUI losing which field has focus, and
/// the core identifies a column by its name.
struct NewTableColumn: Identifiable, Hashable, Encodable {
    let id = UUID()
    var name: String = ""
    var kind: ColumnKind = .text
    var nullable: Bool = true
    /// Written after `DEFAULT` exactly as typed, or absent when the field is
    /// empty — `DEFAULT` with nothing after it is a syntax error, and no default
    /// is the ordinary case.
    var defaultValue: String = ""
    var isPrimaryKey: Bool = false

    private enum CodingKeys: String, CodingKey {
        case name
        case kind
        case nullable
        case defaultValue = "default"
        case isPrimaryKey = "primary_key"
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(name, forKey: .name)
        try container.encode(kind, forKey: .kind)
        try container.encode(nullable, forKey: .nullable)
        let trimmed = defaultValue.trimmingCharacters(in: .whitespaces)
        try container.encode(trimmed.isEmpty ? nil : trimmed, forKey: .defaultValue)
        try container.encode(isPrimaryKey, forKey: .isPrimaryKey)
    }
}

/// What is being done to a column of a relation that already exists.
///
/// Three and not six. A column's type, its nullability and its default are one
/// family of statement on paper and a different one on every server — PostgreSQL
/// writes an `ALTER COLUMN` per property changed, MySQL a single `MODIFY COLUMN`
/// carrying the whole declaration back — and the core leaves them out rather
/// than folding them in. What is here is what the servers agree about.
///
/// The payload rides along because two of the three need one and each needs a
/// different one: a drop names a column, a rename names two, and an add carries
/// the same five answers the Create Table form fills in. Encoded as the tagged
/// JSON `db_column_change_sql` reads.
enum ColumnChange: Hashable, Encodable {
    /// Put a new column into the table. `isPrimaryKey` is refused by the core: a
    /// key is a rule about the whole table, and a table with rows in it has no
    /// room for another.
    case add(NewTableColumn)
    /// Remove the column and everything in it.
    case drop(name: String)
    /// Give it another name, leaving everything else about it alone.
    case rename(name: String, to: String)
    /// Change what the column is: its type, whether it takes a null, its
    /// default, or any two of the three at once.
    case alter(ColumnAlteration)

    /// The word the core reads, and what the status line calls this.
    var verb: String {
        switch self {
        case .add: return "add"
        case .drop: return "drop"
        case .rename: return "rename"
        case .alter: return "alter"
        }
    }

    /// What the menu item says.
    var menuTitle: String {
        switch self {
        case .add: return "Add Column…"
        case .drop: return "Drop Column…"
        case .rename: return "Rename Column…"
        case .alter: return "Edit Column…"
        }
    }

    /// What the button that runs it says. No ellipsis: this one is the doing.
    var actionTitle: String {
        switch self {
        case .add: return "Add"
        case .drop: return "Drop"
        case .rename: return "Rename"
        case .alter: return "Apply"
        }
    }

    /// What the status line says while it is happening, and afterwards. Both
    /// spelled out rather than derived from `actionTitle`, English being what it
    /// is — "Adding" doubles a letter and "Renamed" drops one.
    var progressive: String {
        switch self {
        case .add: return "Adding"
        case .drop: return "Dropping"
        case .rename: return "Renaming"
        case .alter: return "Altering"
        }
    }

    var pastTense: String {
        switch self {
        case .add: return "Added"
        case .drop: return "Dropped"
        case .rename: return "Renamed"
        case .alter: return "Altered"
        }
    }

    /// Whether pressing the button loses something that cannot be got back.
    ///
    /// Only the drop. A rename breaks whatever names the column and does not
    /// take the values with it, and an add takes nothing at all.
    var isDestructive: Bool {
        if case .drop = self { return true }
        return false
    }

    /// The column this acts on, which the sheet puts at the top.
    var columnName: String {
        switch self {
        case .add(let column): return column.name
        case .drop(let name): return name
        case .rename(let name, _): return name
        case .alter(let alteration): return alteration.name
        }
    }

    private enum CodingKeys: String, CodingKey {
        case change
        case column
        case name
        case to
        case kind
        case nullable
        case defaultValue = "default"
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(verb, forKey: .change)
        switch self {
        case .add(let column): try container.encode(column, forKey: .column)
        case .drop(let name): try container.encode(name, forKey: .name)
        case .rename(let name, let to):
            try container.encode(name, forKey: .name)
            try container.encode(to, forKey: .to)
        case .alter(let alteration):
            try container.encode(alteration.name, forKey: .name)
            // Each property crosses only where it moved. An absent key is the
            // core's "leave this one alone", and sending the type on every
            // alteration would retype a `character varying(64)` column to `text`
            // on the way past — the picker only ever holds one of seven kinds.
            try container.encodeIfPresent(alteration.kind?.word, forKey: .kind)
            try container.encodeIfPresent(alteration.nullable, forKey: .nullable)
            try container.encode(alteration.defaultChange, forKey: .defaultValue)
        }
    }
}

/// What is being changed about one column that already exists.
///
/// Each property carries its own "leave it alone", and the column as the server
/// last described it rides along unsent: the sheet says what is there now, and
/// the core is told only what moved.
struct ColumnAlteration: Hashable {
    let name: String
    /// The server's own words for the column as it stands. Shown and never sent:
    /// `dataType` is a string this build cannot always spell — `character
    /// varying(64)` is not one of the seven kinds — which is the whole reason
    /// the three properties below are optional.
    let currentType: String
    let currentNullable: Bool
    let currentDefault: String?

    var kind: ColumnKind?
    var nullable: Bool?
    var defaultChange: DefaultChange = .keep

    /// Whether anything is being asked for at all. The core refuses an
    /// alteration with no clauses — `ALTER TABLE t` alone is a syntax error
    /// rather than a statement that does nothing — and this asks the same
    /// question early enough to keep the button off.
    var isEmpty: Bool { kind == nil && nullable == nil && defaultChange == .keep }

    /// Whether the default is being set to nothing, which is not a shorter
    /// statement but a syntax error — and removing a default is `.drop`, which
    /// says so.
    var isSettingAnEmptyDefault: Bool {
        guard case .set(let value) = defaultChange else { return false }
        return value.trimmingCharacters(in: .whitespaces).isEmpty
    }

    init(_ column: ColumnInfo) {
        name = column.name
        currentType = column.dataType
        currentNullable = column.nullable
        currentDefault = column.defaultValue
    }
}

/// What an alteration does to a column's default.
///
/// Three answers rather than an optional string, because "leave it" and "take it
/// away" are different statements and only one of them runs.
enum DefaultChange: Hashable, Encodable {
    case keep
    case drop
    case set(String)

    func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .keep: try container.encode("keep")
        case .drop: try container.encode("drop")
        // Externally tagged, which is what serde reads at the other end: the two
        // answers carrying nothing are bare words and the one carrying a value
        // is an object.
        // Trimmed on the way out, the rule `NewTableColumn` follows: a default
        // is written into the statement exactly as it arrives, and trailing
        // space in `DEFAULT 0 ` is space nobody typed on purpose.
        case .set(let value):
            try container.encode(["set": value.trimmingCharacters(in: .whitespaces)])
        }
    }
}

/// What is being done to an index of a relation.
///
/// Two verbs. No server here alters an index in place — MySQL's own manager
/// drops it and creates it again, which is two statements and a window in which
/// the table has no index — so what is offered is the two that are one statement
/// each.
enum IndexChange: Hashable, Encodable {
    case create(NewIndex)
    case drop(name: String)

    /// The word the core reads.
    var verb: String {
        switch self {
        case .create: return "create"
        case .drop: return "drop"
        }
    }

    /// What the menu item says.
    var menuTitle: String {
        switch self {
        case .create: return "New Index…"
        case .drop: return "Drop Index…"
        }
    }

    /// What the button that runs it says.
    var actionTitle: String {
        switch self {
        case .create: return "Create"
        case .drop: return "Drop"
        }
    }

    /// What the status line says while it is happening, and afterwards.
    var progressive: String {
        switch self {
        case .create: return "Creating"
        case .drop: return "Dropping"
        }
    }

    var pastTense: String {
        switch self {
        case .create: return "Created"
        case .drop: return "Dropped"
        }
    }

    /// Whether pressing the button loses something.
    ///
    /// The drop. Its data is all in the table it indexes, so nothing is *lost* —
    /// but the index is rebuilt by reading the whole table, which on a large one
    /// is a wait, and that is enough to take Return off the button.
    var isDestructive: Bool {
        if case .drop = self { return true }
        return false
    }

    /// The index this acts on, which the sheet puts at the top.
    var indexName: String {
        switch self {
        case .create(let index): return index.name
        case .drop(let name): return name
        }
    }

    private enum CodingKeys: String, CodingKey {
        case change
        case index
        case name
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(verb, forKey: .change)
        switch self {
        case .create(let index): try container.encode(index, forKey: .index)
        case .drop(let name): try container.encode(name, forKey: .name)
        }
    }
}

/// An index that does not exist yet.
///
/// Four answers: a name, the columns in key order, whether it is unique, and
/// which access method — the last only where the server names one. What is not
/// here is what upstream's index editor also offers and this build cannot show:
/// an expression key, a descending column, a partial index's `WHERE`, an
/// operator class, a MySQL prefix length. Each of those is SQL typed into a
/// form, and the Create Table form draws that boundary in the same place.
struct NewIndex: Hashable, Encodable {
    var name: String = ""
    /// In key order, which is the order they are listed in: an index on
    /// `(a, b)` is not an index on `(b, a)`.
    var columns: [IndexColumn] = [IndexColumn()]
    var unique: Bool = false
    /// Nil takes the server's default, which is what a picker's first row says.
    var method: String?

    private enum CodingKeys: String, CodingKey {
        case name, columns, unique, method
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(name.trimmingCharacters(in: .whitespaces), forKey: .name)
        // The rows, not the identities of the rows: what the core reads is a
        // list of column names in order, and the row a name is on is this side's
        // business.
        try container.encode(columns.map(\.name), forKey: .columns)
        try container.encode(unique, forKey: .unique)
        try container.encodeIfPresent(method, forKey: .method)
    }
}

/// One row of the new index's column list.
///
/// A struct with an identity rather than a bare string, for the reason
/// `NewTableColumn` has one: two rows can hold the same name while somebody is
/// picking, and a list keyed by value would collapse them into one.
struct IndexColumn: Identifiable, Hashable {
    let id = UUID()
    var name: String = ""
}

/// How much of the server's own activity a connection can see and interrupt.
///
/// A ladder, and each rung includes the one below it. The illegal combinations
/// are the reason this is not three booleans: a driver cannot offer to kill a
/// connection it cannot list, and a menu built from separate flags would have to
/// be told so in a comment instead of by the type.
enum ServerProcesses: String, Codable, Hashable {
    /// This driver was never taught to ask, or there is nothing to ask —
    /// SQLite is a file, and a file has no sessions.
    case unreported
    /// The list can be drawn and nothing on it can be stopped from here.
    case readOnly = "read_only"
    /// A whole session can be closed, taking any transaction it holds.
    case closable
    /// A running statement can be cancelled and the session left alone, which
    /// is the gentler of the two and the one to offer first.
    case interruptible

    /// Whether there is a list worth drawing at all, and so whether the menu
    /// item is offered.
    var areReported: Bool { self != .unreported }

    /// Whether `EndProcess.statement` is something this server will do.
    var cancelsStatements: Bool { self == .interruptible }

    /// Whether a session can be closed. True for both rungs above `readOnly`:
    /// a server that cancels statements can always also close the session.
    var closesSessions: Bool { self == .closable || self == .interruptible }
}

/// Which of the two ways to stop a process is being asked for.
///
/// The raw values are the words the C boundary takes. They are spelled rather
/// than numbered there because the two are one bit apart and the wrong one is
/// the destructive one — see `db_end_process`.
enum EndProcess: String, Codable, Hashable {
    /// Cancel what it is running and leave the connection open. Nothing is
    /// rolled back; the session keeps whatever transaction it had.
    case statement
    /// Close the connection. Anything uncommitted on it goes back.
    case session
}

/// One thing the server is doing, as of the moment it was asked.
///
/// Every field is a string, including the id and the duration, because the
/// server formatted them and this side has no arithmetic to do on either — the
/// id goes back to `endProcess` unread, and the duration was rendered by the
/// engine's own interval formatting.
///
/// Named for the server rather than mirroring the core's `ProcessInfo`, which is
/// the one place a Swift name here departs from its Rust one: Foundation already
/// has a `ProcessInfo`, this module reads `ProcessInfo.processInfo` in five
/// places, and a type of ours by that name would quietly win the lookup.
struct ServerProcess: Codable, Hashable, Identifiable {
    let id: String
    let user: String
    let database: String
    let state: String
    let duration: String
    let statement: String

    /// What a filter matches on: everything the row shows, lowercased once.
    ///
    /// Built here rather than at each keystroke because the list is redrawn on a
    /// timer and a busy server has thousands of rows.
    var searchable: String {
        "\(id) \(user) \(database) \(state) \(statement)".lowercased()
    }
}

/// One setting the server is running with.
///
/// Read-only, and the read is the whole feature: there is no `setVariable` at
/// the boundary and no control here that would call one. Changing a setting is
/// `SET GLOBAL` in the Query tab, where it is a statement somebody typed and can
/// see, rather than a text field two rows below `max_connections`.
///
/// Identified by name, which is what makes the list a list. The core promises
/// one row per name — `contract.rs` refuses a driver that reports two — because
/// a duplicate here would be handed to SwiftUI as one identity and drawn over
/// itself.
///
/// Named for the server for the reason `ServerProcess` is: `Variable` alone
/// would be a word this module uses in a dozen other senses.
struct ServerVariable: Codable, Hashable, Identifiable {
    var id: String { name }
    let name: String
    let value: String
    let scope: VariableScope

    /// What a filter matches on: the name and the value, lowercased once.
    ///
    /// The value is included because half of what anybody asks this list is
    /// which settings mention a path, a size, or `off`. The scope is not — it is
    /// two words that would match a third of the rows apiece, and the sheet
    /// draws it in a column that can be read directly.
    var searchable: String { "\(name) \(value)".lowercased() }
}

/// Whose value a setting's is.
///
/// Two cases and not the server's own vocabulary, which is where this differs
/// from `ServerProcess.state`: every engine draws this same line, and a column
/// that said `sighup` on PostgreSQL and `GLOBAL` on MySQL could not be sorted,
/// filtered or read across connections.
enum VariableScope: String, Codable, Hashable {
    /// The server's, and so everybody's.
    case server
    /// This connection's alone — either set on it, or a setting that has no
    /// server-wide value to have.
    case session

    /// The word the sheet draws. Capitalised here rather than at the call site
    /// so the two spellings cannot drift apart.
    var label: String { self == .server ? "Server" : "Session" }
}

/// One database on the server this connection reached.
///
/// Only three engines have this level: SQL Server puts databases above schemas,
/// PostgreSQL can list them but cannot switch within a session, and DuckDB's are
/// catalogs attached to the open connection. The absence is `nil` rather than an
/// empty array, so the navigator can tell "no such level here" from "this login
/// can see none of them".
///
/// A `nil` here does not mean there are no databases — on MySQL, Mongo, SQLite
/// and the rest, the databases *are* the level below, which is what
/// `Capabilities.schemaIsTheDatabase` says.
struct DatabaseInfo: Codable, Hashable, Identifiable {
    let name: String
    /// Whether this is the one the open connection is on. Not derivable from the
    /// connection string: an engine may have sent the session somewhere else at
    /// login, and a default database that is missing leaves the server to pick.
    let isCurrent: Bool

    var id: String { name }

    enum CodingKeys: String, CodingKey {
        case name
        case isCurrent = "is_current"
    }
}

struct SchemaInfo: Codable, Hashable, Identifiable {
    let name: String

    /// Whether this container is the engine's own rather than anybody's data.
    ///
    /// Decided by the driver, which is the only side that knows its engine's
    /// rule — `pg_toast_16384` is the server's and `pg_dumps` is not. The tree
    /// hides these unless `Preferences.showsSystemSchemas`; nothing here
    /// filters, and the driver reports them all either way.
    let isSystem: Bool

    var id: String { name }

    private enum CodingKeys: String, CodingKey {
        case name
        case isSystem = "is_system"
    }
}

enum RelationKind: String, Codable, Hashable {
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
        case .view: return "eye"
        // A view that keeps its rows, so the glyph is the view's inside the
        // box a table's rows live in. It shared the plain eye until sequences
        // arrived and the tree had four kinds of thing drawn with three
        // glyphs — and the one difference the Structure tab can actually show
        // between the two, that a materialized view can be indexed, was
        // invisible in the sidebar.
        case .materializedView: return "eye.square"
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

struct RelationInfo: Codable, Hashable, Identifiable {
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

enum RoutineKind: String, Codable, Hashable {
    case function
    case procedure

    /// SF Symbol used in the navigator. Both are code; the distinction worth
    /// drawing is that one is called for its answer and the other for what it
    /// does.
    var symbol: String {
        switch self {
        case .function: return "function"
        case .procedure: return "gearshape.2"
        }
    }

    var label: String {
        switch self {
        case .function: return "Function"
        case .procedure: return "Procedure"
        }
    }
}

/// One function or procedure in a schema.
///
/// Only where `Capabilities.reportsRoutines` — every other connection refuses
/// the call rather than answering an empty list, so nothing here has to stand
/// for "not asked".
struct RoutineInfo: Codable, Hashable, Identifiable {
    let schema: String
    let name: String
    let kind: RoutineKind

    /// What to hand back to ask for the source. Opaque and driver-defined: a
    /// PostgreSQL oid, a `FUNCTION name` pair on MySQL. Overloading is the
    /// reason it is not the name — `age(date)` and `age(timestamp)` are two
    /// routines and one word.
    let id: String

    /// The parameter list as the database renders it, parentheses excluded.
    /// Empty for a routine that takes none.
    let arguments: String

    /// Absent for a procedure, which returns nothing to describe.
    let returns: String?

    /// `plpgsql`, `sql`, `c` — absent where the engine does not say.
    let language: String?

    /// Name and parameters as one line, which is what distinguishes two
    /// overloads in a list.
    var signature: String { "\(name)(\(arguments))" }
}

/// One sequence in a schema.
///
/// The numbers are strings, as they are in the core: `bigint` here and wider
/// elsewhere, and nothing in this window does arithmetic on them. See
/// `crates/conn/src/metadata.rs`, where the reason is written down once.
struct SequenceInfo: Codable, Hashable, Identifiable {
    let schema: String
    let name: String

    /// Absent for either of two reasons the server does not distinguish:
    /// nothing has been taken from the sequence yet, or this login may see it
    /// without being allowed to read it. Anything drawing this has to say both
    /// or neither. See `crates/conn/src/metadata.rs`.
    let lastValue: String?

    /// May be negative: a descending sequence is ordinary.
    let increment: String
    let minValue: String
    let maxValue: String

    /// Whether it wraps at the end instead of failing.
    let cycles: Bool

    /// How many values are handed out per trip to the catalog. Worth showing
    /// because it explains the gaps: a cache of 50 means the numbers in a table
    /// jump by 50 whenever a session ends.
    let cache: String?

    var id: String { "\(schema).\(name)" }

    /// The range as one line, which is how it is read. Both ends or neither —
    /// half a range says nothing.
    var range: String { "\(minValue) … \(maxValue)" }

    private enum CodingKeys: String, CodingKey {
        case schema, name, increment, cycles, cache
        case lastValue = "last_value"
        case minValue = "min_value"
        case maxValue = "max_value"
    }
}

/// One selectable row of the navigator.
///
/// A `List` selection is one `Hashable` type and the tree now holds two kinds of
/// object, so this is what the rows tag themselves with. It goes no further than
/// the tree: `AppModel.navigatorSelection` unpacks it into the two properties the
/// rest of the window reads, because a relation and a routine are not
/// interchangeable anywhere below the sidebar and a pane that had to unwrap this
/// on every access would be asking the same question a hundred times.
enum NavigatorNode: Hashable {
    case relation(RelationInfo)
    case routine(RoutineInfo)
    case sequence(SequenceInfo)
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

/// One line of what the engine has to say about a relation beyond its columns.
///
/// Two strings, both already formatted by the driver: PostgreSQL's "142 MB"
/// comes out of `pg_size_pretty` and MySQL's out of `information_schema`, and
/// the labels are the driver's words for its own concepts — "Storage engine"
/// means nothing on PostgreSQL and "Owner" is not a thing MySQL records here.
/// Modelling this as a struct with a field per fact would be this side deciding
/// which facts exist, and would leave every engine's own answer with nowhere to
/// go.
///
/// The order is the driver's too, so a pane reads in the order somebody who
/// knows the product would say it.
struct InfoField: Decodable, Hashable, Identifiable {
    let label: String
    let value: String

    var id: String { label }
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

    /// What the popup calls it.
    ///
    /// Not the raw value, deliberately. The wire spells `not_equals`, and a
    /// popup that said so would be asking somebody to read JSON. Where SQL has a
    /// symbol this uses it, because that is what the Custom field beside it will
    /// show; where SQL has a keyword this uses the keyword in capitals, for the
    /// same reason. The last three are lower-case words on purpose: `contains`
    /// is this window's name for a `LIKE` with a wildcard at each end, and
    /// dressing it as SQL would suggest it is something that can be typed.
    var label: String {
        switch self {
        case .equals: return "="
        case .notEquals: return "≠"
        case .isNull: return "IS NULL"
        case .isNotNull: return "IS NOT NULL"
        case .lessThan: return "<"
        case .lessOrEqual: return "≤"
        case .greaterThan: return ">"
        case .greaterOrEqual: return "≥"
        case .between: return "BETWEEN"
        case .contains: return "contains"
        case .startsWith: return "starts with"
        case .endsWith: return "ends with"
        }
    }
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

    /// This row made consistent with the column it names.
    ///
    /// A row is three controls over one value and any of them can be moved to a
    /// place that makes another's answer impossible. Two corrections follow from
    /// that.
    ///
    /// An operator the column cannot answer falls back to the column's first,
    /// which is `equals` everywhere — the case is `contains` on a row dragged
    /// from a text column onto an integer. Refusing the move was the
    /// alternative, and a popup that will not change is worse to use than one
    /// that changes to something obvious.
    ///
    /// Operands the operator does not have are dropped. `IS NULL` compares
    /// against nothing and everything but `BETWEEN` has one end, so text stays
    /// in the row only while a field is showing it. Left behind, it would go to
    /// the core at the next Apply as part of a filter nothing on screen
    /// describes.
    ///
    /// A column not in `columns` is left holding whatever it holds. That is a
    /// row naming something this relation does not have — a table changed under
    /// a restored filter — and the honest answer is the error the core gives it,
    /// naming the column, rather than a silent move to a column somebody did not
    /// choose.
    func settled(in columns: [FilterColumn]) -> FilterRule {
        var settled = self
        if let column = columns.first(where: { $0.name == column }),
            !column.operators.contains(settled.op)
        {
            settled.op = column.operators.first ?? .equals
        }
        switch settled.op {
        case .isNull, .isNotNull:
            settled.value = nil
            settled.second = nil
        case .between:
            break
        default:
            settled.second = nil
        }
        return settled
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
