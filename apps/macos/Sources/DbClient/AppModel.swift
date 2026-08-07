import CDbFfi
import Foundation
import Observation

/// Which detail pane is showing. Mirrors the tab strip a Sequel Ace user
/// expects: describe the object, browse its rows, or write SQL against it.
enum DetailTab: String, CaseIterable, Identifiable {
    case structure = "Structure"
    case content = "Content"
    case query = "Query"

    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .structure: return "list.bullet.rectangle"
        case .content: return "tablecells"
        case .query: return "terminal"
        }
    }
}

/// UI state and the bridge to the core.
///
/// Every core call blocks, so all of them run on a serial background queue and
/// publish results back to the main actor. The queue is serial rather than
/// concurrent because a single connection cannot service overlapping queries —
/// this is a property of the connection, not a convenience.
@Observable
@MainActor
final class AppModel {
    // Navigator
    private(set) var schemas: [SchemaInfo] = []
    private(set) var relations: [String: [RelationInfo]] = [:]
    var expanded: Set<String> = []
    var selected: RelationInfo? { didSet { selectionChanged(from: oldValue) } }
    /// Name filter for the navigator. A schema with hundreds of objects is the
    /// normal case, and scrolling to find one is the slowest thing a user does.
    var navigatorFilter = ""

    // Detail
    var activeTab: DetailTab = .content
    private(set) var columns: [ColumnInfo] = []

    // Content pane
    let grid = ArrowTable()
    /// Bumped whenever `grid` is replaced, so the Metal view knows to redraw.
    private(set) var gridGeneration = 0
    private(set) var loadedRows = 0
    private(set) var loadMilliseconds: Double = 0
    /// The cell the grid's cursor is on, mirrored here so the inspector and the
    /// status bar can read it.
    var gridSelection: GridSelection?

    // Content pane filters
    var whereClause = ""
    var orderClause = ""

    // Query pane
    var queryText = ""
    /// The last statement `selectionChanged` put in the editor, so a later
    /// selection can tell "untouched suggestion" from "the user's work".
    private var suggestedQueryText = ""

    // Chrome
    private(set) var connectionLabel = "Not connected"
    private(set) var connectionState: StatusDot.State = .connecting
    private(set) var status = "Connecting…"
    private(set) var isBusy = false
    var errorMessage: String?

    /// Rows fetched per browse. A grid shows a window onto the data; pulling a
    /// million rows to display forty is what makes other clients feel slow.
    /// The Content pane fetches more as the user scrolls (phase 1).
    private let browseLimit = 100_000
    private let batchRows = 8192

    private var db: Database?
    private let queue = DispatchQueue(label: "dev.dbclient.core", qos: .userInitiated)
    private let connString: String

    /// A statement to open with, from `--sql`. Runs once the connection is up,
    /// in place of browsing the first table.
    private let initialSQL: String?

    /// Browse filters to open with, from `--where` / `--order`. Applied by the
    /// first browse rather than as a second query.
    private let initialFilters: (where: String?, order: String?)
    private var appliedInitialFilters = false

    init(
        connString: String, initialTab: DetailTab = .content, initialSQL: String? = nil,
        initialWhere: String? = nil, initialOrder: String? = nil
    ) {
        self.connString = connString
        self.activeTab = initialSQL == nil ? initialTab : .query
        self.initialSQL = initialSQL
        self.initialFilters = (initialWhere, initialOrder)
        if let initialSQL { queryText = initialSQL }
    }

    // MARK: - Lifecycle

    func connect() {
        isBusy = true
        status = "Connecting…"
        run { [connString] in
            let db = try Database(connString: connString)
            let schemas = try db.schemas()
            var relations: [String: [RelationInfo]] = [:]
            for s in schemas {
                relations[s.name] = try db.relations(schema: s.name)
            }
            return (db, schemas, relations)
        } then: { [self] result in
            db = result.0
            schemas = result.1
            relations = result.2
            connectionLabel = Self.label(for: connString)
            connectionState = .connected
            // Open the schema a user most likely wants, and land on a table
            // rather than an empty pane. Opening to nothing makes every session
            // start with the same two clicks.
            if let first = schemas.first(where: { $0.name == "public" }) ?? schemas.first {
                expanded.insert(first.name)
                selected = relations[first.name]?.first
            }
            status = "\(schemas.count) schemas"
            isBusy = false
            // Runs after the selection above, so an explicit `--sql` replaces
            // the browse rather than racing it.
            if let initialSQL { runQuery(initialSQL, describedAs: "query") }
        }
    }

    // MARK: - Navigator

    /// Relations in `schema` matching the filter. Matching on a substring
    /// rather than a prefix, because table names are usually reached by the
    /// distinctive word in the middle rather than by what they start with.
    func visibleRelations(in schema: String) -> [RelationInfo] {
        let all = relations[schema] ?? []
        let needle = navigatorFilter.trimmingCharacters(in: .whitespaces).lowercased()
        guard !needle.isEmpty else { return all }
        return all.filter { $0.name.lowercased().contains(needle) }
    }

    /// Whether a schema's disclosure is open.
    ///
    /// While a filter is active every schema with a match opens, so results are
    /// never hidden inside a collapsed group the user cannot see to expand.
    func isExpanded(_ schema: String) -> Bool {
        if !navigatorFilter.trimmingCharacters(in: .whitespaces).isEmpty {
            return !visibleRelations(in: schema).isEmpty
        }
        return expanded.contains(schema)
    }

    var matchedRelationCount: Int {
        schemas.reduce(0) { $0 + visibleRelations(in: $1.name).count }
    }

    var totalRelationCount: Int {
        relations.values.reduce(0) { $0 + $1.count }
    }

    // MARK: - Cell inspection

    /// What the selected cell holds, spelled out for the inspector strip.
    ///
    /// The grid clips a cell to the column width, so without this a value wider
    /// than its column simply cannot be read.
    struct InspectedCell {
        let column: String
        let type: String
        let value: String
        let isNull: Bool
        let address: String
    }

    var inspectedCell: InspectedCell? {
        guard let s = gridSelection,
              s.column < grid.columns.count,
              s.row < grid.rowCount
        else { return nil }
        let name = grid.columns[s.column].name
        let isNull = grid.isNull(row: s.row, column: s.column)
        return InspectedCell(
            column: name,
            // The relation's declared type where we have it; the Query tab may
            // return computed columns that no relation describes.
            type: columns.first { $0.name == name }?.dataType ?? "",
            value: isNull ? "NULL" : grid.text(row: s.row, column: s.column),
            isNull: isNull,
            address: "row \(Self.formatted(s.row + 1))")
    }

    // MARK: - Selection

    private func selectionChanged(from previous: RelationInfo?) {
        guard let selected, selected != previous else { return }
        // Filters describe the previous table's columns and cannot be assumed
        // to apply here; carrying them over would produce confusing errors.
        // The first selection is the exception: it is where --where/--order land.
        whereClause = appliedInitialFilters ? "" : (initialFilters.where ?? "")
        orderClause = appliedInitialFilters ? "" : (initialFilters.order ?? "")
        appliedInitialFilters = true
        loadColumns(for: selected)
        runQuery(browseSQL(for: selected), describedAs: selected.name)

        // Reseed the editor only when it still holds text this method wrote.
        // Selecting a table used to overwrite the editor unconditionally, which
        // silently discarded whatever statement the user was in the middle of.
        let suggestion = "SELECT * FROM \(selected.qualifiedName) LIMIT 1000"
        if queryText.isEmpty || queryText == suggestedQueryText {
            queryText = suggestion
            suggestedQueryText = suggestion
        }
    }

    private func loadColumns(for relation: RelationInfo) {
        run { db in
            try db.columns(schema: relation.schema, relation: relation.name)
        } then: { [self] cols in
            columns = cols
        }
    }

    /// Builds the browse query from the filter bar.
    ///
    /// Filters become SQL rather than filtering fetched rows, so they apply to
    /// the whole table instead of only the window already in memory.
    private func browseSQL(for relation: RelationInfo) -> String {
        var sql = "SELECT * FROM \(relation.qualifiedName)"
        let predicate = whereClause.trimmingCharacters(in: .whitespacesAndNewlines)
        if !predicate.isEmpty { sql += " WHERE \(predicate)" }
        let order = orderClause.trimmingCharacters(in: .whitespacesAndNewlines)
        if !order.isEmpty { sql += " ORDER BY \(order)" }
        sql += " LIMIT \(browseLimit)"
        return sql
    }

    func applyFilters() {
        guard let selected else { return }
        activeTab = .content
        runQuery(browseSQL(for: selected), describedAs: selected.name)
    }

    // MARK: - Sorting

    /// The single-column ordering the ORDER BY field currently expresses, or
    /// nil if it is empty or says something this cannot summarise.
    ///
    /// Derived rather than stored. The field is the state: a header click edits
    /// it and the marker reads it back, so the two cannot drift, and an order
    /// typed by hand gets the same marker as one that was clicked.
    private var parsedOrder: (column: String, descending: Bool)? {
        var text = orderClause.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty, !text.contains(",") else { return nil }

        var descending = false
        for suffix in [" desc", " asc"] where text.lowercased().hasSuffix(suffix) {
            descending = suffix == " desc"
            text = String(text.dropLast(suffix.count)).trimmingCharacters(in: .whitespaces)
            break
        }
        text = text.trimmingCharacters(in: CharacterSet(charactersIn: "\""))
        return text.isEmpty ? nil : (text, descending)
    }

    /// Where to draw the ordering marker, as an index into the current result.
    var gridSort: GridSort? {
        guard let order = parsedOrder,
              let index = grid.columns.firstIndex(where: { $0.name == order.column })
        else { return nil }
        return GridSort(column: index, descending: order.descending)
    }

    /// Cycles a column through ascending, descending, and unsorted.
    func toggleSort(column index: Int) {
        guard selected != nil, index < grid.columns.count else { return }
        let name = grid.columns[index].name
        let current = parsedOrder

        if current?.column == name {
            orderClause = current?.descending == true ? "" : "\"\(name)\" DESC"
        } else {
            orderClause = "\"\(name)\""
        }
        applyFilters()
    }

    /// What the status bar reads. Tab-specific, because the panes are showing
    /// different things: on the Structure tab, row count and elapsed time
    /// describe a result the user is not currently looking at.
    var statusLine: String {
        switch activeTab {
        case .structure:
            guard !columns.isEmpty else { return status }
            let keys = columns.filter(\.isPrimaryKey).count
            let keyPart = keys == 0 ? "no primary key" : "\(keys) in primary key"
            return "\(columns.count) columns · \(keyPart)"
        case .content, .query:
            return status
        }
    }

    /// Whether ⌘R has anything to run. Drives the Run button's disabled state,
    /// so the button is never offered when pressing it would do nothing.
    var canRun: Bool {
        activeTab == .query
            ? !queryText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            : selected != nil
    }

    func runCurrentQuery() {
        // ⌘R means "run what I am looking at": the query text in the Query tab,
        // the filtered browse elsewhere.
        if activeTab == .query {
            runQuery(queryText, describedAs: "query")
        } else if let selected {
            runQuery(browseSQL(for: selected), describedAs: selected.name)
        }
    }

    // MARK: - Query execution

    private func runQuery(_ sql: String, describedAs label: String) {
        isBusy = true
        status = "Running…"
        // A new run supersedes the previous failure; leaving the banner up
        // would attribute an old error to the result now on screen.
        errorMessage = nil
        let batchRows = self.batchRows

        // The grid is mutated on the main actor only; the background stage
        // returns batches and the main stage installs them.
        run { db -> QueryResult in
            let started = CFAbsoluteTimeGetCurrent()
            let query = try db.query(sql, batchRows: batchRows)
            let schema = try query.schema()
            var batches: [UnsafeMutablePointer<ArrowArray>] = []
            while let batch = try query.nextBatch() {
                batches.append(batch)
            }
            return QueryResult(
                schema: schema, batches: batches,
                milliseconds: (CFAbsoluteTimeGetCurrent() - started) * 1000)
        } then: { [self] result in
            grid.reset()
            gridSelection = nil
            grid.setSchema(result.schema)
            if let release = result.schema.pointee.release { release(result.schema) }
            result.schema.deallocate()
            for batch in result.batches {
                grid.append(batch: batch)
            }
            gridGeneration += 1
            loadedRows = grid.rowCount
            // Land the cursor on the first cell. It gives the arrow keys a
            // starting point and puts a real value in the inspector, instead of
            // asking the user to click once before the pane says anything.
            gridSelection = grid.rowCount > 0 ? GridSelection(row: 0, column: 0) : nil
            loadMilliseconds = result.milliseconds
            let ms = result.milliseconds
            status = "\(label) · \(Self.formatted(grid.rowCount)) rows · "
                + "\(String(format: "%.2f", ms / 1000)) s"
            isBusy = false
        }
    }

    // MARK: - Plumbing

    /// Runs `work` on the core queue and applies its result on the main actor.
    ///
    /// Failures surface as `errorMessage` rather than being swallowed: a
    /// navigator that silently shows nothing is worse than one that says why.
    private func run<T>(
        _ work: @escaping @Sendable (Database) throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        guard let db else { return }
        dispatch({ try work(db) }, then: apply)
    }

    private func run<T>(
        _ work: @escaping @Sendable () throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        dispatch(work, then: apply)
    }

    private func dispatch<T>(
        _ work: @escaping @Sendable () throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        queue.async { [weak self] in
            do {
                let value = try work()
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { apply(value) }
                }
            } catch {
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { self?.fail(with: error) }
                }
            }
        }
    }

    private func fail(with error: Error) {
        errorMessage = String(describing: error)
        status = "Failed"
        isBusy = false
        // A failed statement says nothing about the connection; only a failure
        // before one exists does.
        if db == nil { connectionState = .failed }
    }

    private static func label(for connString: String) -> String {
        // "host=… dbname=…" → "dbname@host", which is how these tools name a
        // session and how users refer to one.
        var host = "localhost", dbname = "database"
        for pair in connString.split(separator: " ") {
            let kv = pair.split(separator: "=", maxSplits: 1)
            guard kv.count == 2 else { continue }
            if kv[0] == "host" { host = String(kv[1]) }
            if kv[0] == "dbname" { dbname = String(kv[1]) }
        }
        return "\(dbname)@\(host)"
    }

    static func formatted(_ n: Int) -> String {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f.string(from: NSNumber(value: n)) ?? String(n)
    }

    static func formatted(_ n: Int64) -> String {
        formatted(Int(n))
    }
}

/// One query's Arrow output on its way from the core queue to the main actor.
///
/// The pointers are not `Sendable` by inference, but ownership transfers with
/// the value and only the receiver ever reads them — so the conformance sits on
/// this handoff type rather than on `UnsafeMutablePointer` itself, which would
/// make the claim for every pointer in the program.
private struct QueryResult: @unchecked Sendable {
    let schema: UnsafeMutablePointer<ArrowSchema>
    let batches: [UnsafeMutablePointer<ArrowArray>]
    let milliseconds: Double
}
