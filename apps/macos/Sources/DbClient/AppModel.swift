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

    // Detail
    var activeTab: DetailTab = .content
    private(set) var columns: [ColumnInfo] = []

    // Content pane
    let grid = ArrowTable()
    /// Bumped whenever `grid` is replaced, so the Metal view knows to redraw.
    private(set) var gridGeneration = 0
    private(set) var loadedRows = 0
    private(set) var loadMilliseconds: Double = 0

    // Content pane filters
    var whereClause = ""
    var orderClause = ""

    // Query pane
    var queryText = "SELECT * FROM bench_wide LIMIT 1000"

    // Chrome
    private(set) var connectionLabel = "Not connected"
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

    init(connString: String, initialTab: DetailTab = .content) {
        self.connString = connString
        self.activeTab = initialTab
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
            // Open the schema a user most likely wants, and land on a table
            // rather than an empty pane. Opening to nothing makes every session
            // start with the same two clicks.
            if let first = schemas.first(where: { $0.name == "public" }) ?? schemas.first {
                expanded.insert(first.name)
                selected = relations[first.name]?.first
            }
            status = "\(schemas.count) schemas"
            isBusy = false
        }
    }

    // MARK: - Selection

    private func selectionChanged(from previous: RelationInfo?) {
        guard let selected, selected != previous else { return }
        // Filters describe the previous table's columns and cannot be assumed
        // to apply here; carrying them over would produce confusing errors.
        whereClause = ""
        orderClause = ""
        loadColumns(for: selected)
        runQuery(browseSQL(for: selected), describedAs: selected.name)
        queryText = "SELECT * FROM \(selected.qualifiedName) LIMIT 1000"
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
            grid.setSchema(result.schema)
            if let release = result.schema.pointee.release { release(result.schema) }
            result.schema.deallocate()
            for batch in result.batches {
                grid.append(batch: batch)
            }
            gridGeneration += 1
            loadedRows = grid.rowCount
            loadMilliseconds = result.milliseconds
            let ms = result.milliseconds
            status = "\(Self.formatted(grid.rowCount)) rows · \(String(format: "%.2f", ms / 1000)) s"
            isBusy = false
            if activeTab == .structure { activeTab = .structure } // keep tab
            _ = label
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
                    MainActor.assumeIsolated {
                        self?.errorMessage = String(describing: error)
                        self?.status = "Failed"
                        self?.isBusy = false
                    }
                }
            }
        }
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
