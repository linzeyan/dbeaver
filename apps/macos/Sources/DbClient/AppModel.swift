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

/// One pane's result: the rows, plus everything the chrome says about them.
///
/// The browse and the SQL editor own one each. They used to share a single
/// grid, so opening the Query tab showed the browse's rows underneath a
/// statement that had not produced them, and the status bar attributed those
/// rows to the table being browsed. Two panes showing two things need two
/// results; one shared result can only ever describe one of them correctly.
@Observable
@MainActor
final class ResultSet {
    let table = ArrowTable()
    /// Bumped whenever `table` is replaced, so the Metal view knows to redraw.
    /// Zero means nothing has ever run here, which is a different state from a
    /// query that returned no rows.
    private(set) var generation = 0
    private(set) var rowCount = 0
    private(set) var milliseconds: Double = 0
    /// Whether the result hit the LIMIT it was given, so what is on screen is a
    /// window onto the table rather than all of it.
    private(set) var capped = false
    /// What the status bar says about this result.
    private(set) var summary = ""
    private(set) var isLoading = false
    var selection: GridSelection?

    var hasRun: Bool { generation > 0 }

    func beginLoading() { isLoading = true }

    func abandonLoading() { isLoading = false }

    /// Publishes rows already appended to `table` by the caller.
    func finish(capped: Bool, milliseconds: Double, summary: String) {
        generation += 1
        // Land the cursor on the first cell. It gives the arrow keys a starting
        // point and puts a real value in the inspector, instead of asking the
        // user to click once before the pane says anything.
        selection = table.rowCount > 0 ? GridSelection(row: 0, column: 0) : nil
        publish(capped: capped, milliseconds: milliseconds, summary: summary)
    }

    /// Publishes a page appended to the rows already here.
    ///
    /// Deliberately does not touch `generation` or `selection`: the grid resets
    /// its scroll position on a new generation, and someone who just asked for
    /// more rows is at the bottom of the ones they have. The row count is what
    /// tells the view to redraw.
    func extend(capped: Bool, milliseconds: Double, summary: String) {
        publish(capped: capped, milliseconds: milliseconds, summary: summary)
    }

    private func publish(capped: Bool, milliseconds: Double, summary: String) {
        rowCount = table.rowCount
        self.capped = capped
        self.milliseconds = milliseconds
        self.summary = summary
        isLoading = false
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
    private(set) var indexes: [IndexInfo] = []
    private(set) var foreignKeys: [RelationshipInfo] = []
    private(set) var referencedBy: [RelationshipInfo] = []
    private(set) var constraints: [ConstraintInfo] = []
    private(set) var triggers: [TriggerInfo] = []
    /// The statement a view is defined by. Nil for a table, which is what keeps
    /// the Definition section off relations that cannot have one.
    private(set) var definition: String?

    // Content pane
    let browseResult = ResultSet()

    // Query pane
    let queryResult = ResultSet()

    /// The result the chrome is currently describing. Structure has no result of
    /// its own, so it borrows the browse's — the status bar overrides what it
    /// says there anyway.
    var current: ResultSet { activeTab == .query ? queryResult : browseResult }

    // Content pane filters
    var whereClause = ""
    var orderClause = ""

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

    /// Rows fetched per browse page. A grid shows a window onto the data;
    /// pulling a million rows to display forty is what makes other clients feel
    /// slow. `loadMore()` fetches the next page on request.
    private let browsePage = 100_000

    /// Rows a page fetches, for the chrome to name in a control.
    var pageSize: Int { browsePage }
    private let batchRows = 8192

    private var db: Database?
    private let queue = DispatchQueue(label: "dev.dbclient.core", qos: .userInitiated)
    private let connString: String

    /// A statement to open with, from `--sql`. Runs once the connection is up,
    /// in place of browsing the first table.
    private let initialSQL: String?

    /// Structure section to open on, from `--section`. Exists for the same
    /// reason `--tab` does: rendering defects here are only caught by looking
    /// at a screenshot, and a screenshot cannot click a strip.
    let initialStructureDetail: StructureDetail?

    /// Relation to open on, from `--relation`. Without it a capture can only
    /// ever show whichever relation sorts first.
    private let initialRelation: String?

    /// Browse filters to open with, from `--where` / `--order`. Applied by the
    /// first browse rather than as a second query.
    private let initialFilters: (where: String?, order: String?)
    private var appliedInitialFilters = false

    init(
        connString: String, initialTab: DetailTab = .content, initialSQL: String? = nil,
        initialWhere: String? = nil, initialOrder: String? = nil,
        initialStructureDetail: StructureDetail? = nil, initialRelation: String? = nil
    ) {
        self.initialStructureDetail = initialStructureDetail
        self.initialRelation = initialRelation
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
            // start with the same two clicks. `--relation` overrides both, and
            // may name a schema of its own.
            let requested = initialRelation.flatMap(findRelation)
            let opening = requested.map(\.schema)
                ?? (schemas.first(where: { $0.name == "public" }) ?? schemas.first)?.name
            if let opening {
                expanded.insert(opening)
                selected = requested ?? relations[opening]?.first
            }
            status = Self.pluralized(schemas.count, "schema")
            isBusy = false
            // Runs after the selection above, so an explicit `--sql` replaces
            // the browse rather than racing it.
            if let initialSQL {
                runQuery(initialSQL, describedAs: "query", into: queryResult)
            }
        }
    }

    /// Resolves `--relation`, which is either a bare name or `schema.name`.
    /// Unqualified searches every schema so a capture does not have to know
    /// where a table lives, but prefers the one that opens by default.
    private func findRelation(named requested: String) -> RelationInfo? {
        if let dot = requested.firstIndex(of: ".") {
            let schema = String(requested[..<dot])
            let name = String(requested[requested.index(after: dot)...])
            return relations[schema]?.first { $0.name == name }
        }
        let preferred = relations["public"]?.first { $0.name == requested }
        return preferred ?? relations.values.lazy.compactMap { list in
            list.first { $0.name == requested }
        }.first
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

    func inspectedCell(in result: ResultSet) -> InspectedCell? {
        let grid = result.table
        guard let s = result.selection,
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
            // A multi-row selection extends far past the viewport, so the count
            // is the only place it is legible before ⌘C makes it obvious.
            address: s.rows.count > 1
                ? "\(Self.formatted(s.rows.count)) rows selected"
                : "row \(Self.formatted(s.row + 1))")
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
        // Cleared rather than left showing the previous relation's structure
        // while the new one loads.
        indexes = []
        foreignKeys = []
        referencedBy = []
        constraints = []
        triggers = []
        definition = nil
        // The browse orders by the primary key, so the columns have to be known
        // before its statement can be written. Issuing both at once left the
        // first page in heap order and every later page in key order — two
        // different orders across one result, which is how a page repeats rows
        // the previous one already showed.
        loadColumns(for: selected) { [self] in
            runBrowse()
            loadRelationDetail(for: selected)
        }

        // Reseed the editor only when it still holds text this method wrote.
        // Selecting a table used to overwrite the editor unconditionally, which
        // silently discarded whatever statement the user was in the middle of.
        let suggestion = "SELECT * FROM \(selected.qualifiedName) LIMIT 1000"
        if queryText.isEmpty || queryText == suggestedQueryText {
            queryText = suggestion
            suggestedQueryText = suggestion
        }
    }

    private func loadColumns(for relation: RelationInfo, then next: @escaping @MainActor () -> Void) {
        run { db in
            try db.columns(schema: relation.schema, relation: relation.name)
        } then: { [self] cols in
            columns = cols
            next()
        }
    }

    /// Everything the Structure tab shows below the columns.
    ///
    /// Queued after the browse rather than before it. The core queue is serial,
    /// so putting five more round trips in front of the rows would delay the
    /// pane the user is actually looking at in order to fill one they may never
    /// open. Fetched as one unit so the sections cannot appear one at a time.
    private func loadRelationDetail(for relation: RelationInfo) {
        let (schema, name) = (relation.schema, relation.name)
        run { db in
            RelationDetail(
                indexes: try db.indexes(schema: schema, relation: name),
                foreignKeys: try db.foreignKeys(schema: schema, relation: name),
                referencedBy: try db.referencedBy(schema: schema, relation: name),
                constraints: try db.constraints(schema: schema, relation: name),
                triggers: try db.triggers(schema: schema, relation: name),
                definition: try db.definition(schema: schema, relation: name))
        } then: { [self] detail in
            indexes = detail.indexes
            foreignKeys = detail.foreignKeys
            referencedBy = detail.referencedBy
            constraints = detail.constraints
            triggers = detail.triggers
            definition = detail.definition
        }
    }

    /// The sections the strip offers for the selected relation.
    ///
    /// Definition is the only conditional one. The other five are empty on a
    /// relation that has none of them, and an empty section still answers a
    /// question; a Definition section on a table would offer to show something
    /// a table cannot have.
    var structureSections: [StructureDetail] {
        StructureDetail.allCases.filter { $0 != .definition || definition != nil }
    }

    /// How many rows a section holds, or nil for one that is not a list.
    ///
    /// A definition is a single value, and "1" beside it would answer a question
    /// nobody asked — the section being offered at all is what says there is one.
    func structureDetailCount(_ section: StructureDetail) -> Int? {
        switch section {
        case .indexes: return indexes.count
        case .foreignKeys: return foreignKeys.count
        case .referencedBy: return referencedBy.count
        case .constraints: return constraints.count
        case .triggers: return triggers.count
        case .definition: return nil
        }
    }

    private struct RelationDetail: Sendable {
        let indexes: [IndexInfo]
        let foreignKeys: [RelationshipInfo]
        let referencedBy: [RelationshipInfo]
        let constraints: [ConstraintInfo]
        let triggers: [TriggerInfo]
        let definition: String?
    }

    /// Builds the browse query from the filter bar.
    ///
    /// Filters become SQL rather than filtering fetched rows, so they apply to
    /// the whole table instead of only the window already in memory.
    private func browseSQL(for relation: RelationInfo, offset: Int) -> String {
        var sql = "SELECT * FROM \(relation.qualifiedName)"
        let predicate = whereClause.trimmingCharacters(in: .whitespacesAndNewlines)
        if !predicate.isEmpty { sql += " WHERE \(predicate)" }
        if let order = totalOrder { sql += " ORDER BY \(order)" }
        sql += " LIMIT \(browsePage)"
        if offset > 0 { sql += " OFFSET \(offset)" }
        return sql
    }

    /// The browse's ORDER BY, made total by the primary key.
    ///
    /// LIMIT without a total order does not describe a stable window. Postgres
    /// may return rows in a different order for the same query — a different
    /// plan, a different set of pages in cache — so the second page can repeat
    /// rows the first one showed and skip others entirely, with nothing on
    /// screen to say it happened. Appending the primary key makes the order
    /// unique, which is what gives OFFSET a meaning. A relation without one has
    /// no such order, and `canLoadMore` refuses to page it rather than paging
    /// it wrongly.
    private var totalOrder: String? {
        let user = orderClause.trimmingCharacters(in: .whitespacesAndNewlines)
        // The user's own order may already name the key; repeating it is
        // harmless to Postgres but noise in the statement.
        let keys = columns
            .filter { $0.isPrimaryKey && $0.name != parsedOrder?.column }
            .map { "\"\($0.name)\"" }
        let terms = user.isEmpty ? keys : [user] + keys
        return terms.isEmpty ? nil : terms.joined(separator: ", ")
    }

    /// Whether the browse can fetch a further page. Needs both a page boundary
    /// to fetch past and an order stable enough for "past" to mean anything.
    var canLoadMore: Bool {
        activeTab == .content && browseResult.capped && !browseResult.isLoading
            && columns.contains(where: \.isPrimaryKey)
    }

    /// Why the truncation marker is not offering a next page, when it is not.
    ///
    /// Two lengths because they go to two places that can't take the same text:
    /// the status bar has one line already spent on counts, and the tooltip is
    /// where the reason can be spelled out. Kept together so they cannot drift
    /// into saying different things.
    struct PagingObstacle {
        let label: String
        let detail: String
    }

    var pagingObstacle: PagingObstacle? {
        guard browseResult.capped, !columns.contains(where: \.isPrimaryKey) else { return nil }
        return PagingObstacle(
            label: "no primary key",
            detail: "\(selected?.name ?? "This relation") has no primary key, "
                + "so there is no stable order to page in.")
    }

    /// Example filters, written against the selected relation. A fixed `id > 100`
    /// hint names a column most tables do not have, which reads as the field
    /// having been prefilled with something that will not run.
    var filterHint: (where: String, order: String) {
        guard let first = columns.first?.name else { return ("", "") }
        return ("\(first) > 100", "\(first) desc")
    }

    private func runBrowse() {
        guard let selected else { return }
        runQuery(
            browseSQL(for: selected, offset: 0), describedAs: selected.name,
            into: browseResult, cappedAt: browsePage)
    }

    /// Fetches the next page and appends it to the rows already on screen.
    func loadMore() {
        guard canLoadMore, let selected else { return }
        runQuery(
            browseSQL(for: selected, offset: browseResult.rowCount),
            describedAs: selected.name, into: browseResult,
            cappedAt: browsePage, appending: true)
    }

    func applyFilters() {
        activeTab = .content
        runBrowse()
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

    /// Where to draw the ordering marker, as an index into the browse result.
    /// Only the browse sorts: the Query pane shows what a statement returned,
    /// and this cannot append an ORDER BY to arbitrary SQL correctly.
    var gridSort: GridSort? {
        guard let order = parsedOrder,
              let index = browseResult.table.columns
                  .firstIndex(where: { $0.name == order.column })
        else { return nil }
        return GridSort(column: index, descending: order.descending)
    }

    /// Cycles a column through ascending, descending, and unsorted.
    func toggleSort(column index: Int) {
        let grid = browseResult.table
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
            return "\(Self.pluralized(columns.count, "column")) · \(keyPart)"
        case .content, .query:
            // Each pane reports its own result. Falling back to `status` covers
            // the connection messages and the window before anything has run.
            return current.summary.isEmpty ? status : current.summary
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
            runQuery(queryText, describedAs: "query", into: queryResult)
        } else {
            runBrowse()
        }
    }

    // MARK: - Query execution

    /// Runs `sql` and installs the result.
    ///
    /// `cappedAt` is the LIMIT this query carries, when it was imposed by the
    /// browse rather than written by the user. Landing exactly on it means the
    /// grid is showing a window, not an answer, and the status bar has to say
    /// so — a row count that silently means "the first hundred thousand" is the
    /// same class of lie as a cell that truncates without a marker.
    private func runQuery(
        _ sql: String, describedAs label: String, into result: ResultSet,
        cappedAt: Int? = nil, appending: Bool = false
    ) {
        isBusy = true
        result.beginLoading()
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
        } then: { [self] fetched in
            let grid = result.table
            if appending {
                // The page carries the same schema as the rows already here;
                // re-installing it would drop the columns the existing batches
                // were built against.
                if let release = fetched.schema.pointee.release { release(fetched.schema) }
                fetched.schema.deallocate()
            } else {
                grid.reset()
                result.selection = nil
                grid.setSchema(fetched.schema)
                if let release = fetched.schema.pointee.release { release(fetched.schema) }
                fetched.schema.deallocate()
            }

            let before = grid.rowCount
            for batch in fetched.batches {
                grid.append(batch: batch)
            }
            // The cap applies to the page, not the running total: a fourth page
            // that comes back short is the end of the table, however many rows
            // are on screen by then.
            let page = grid.rowCount - before
            let capped = cappedAt.map { page >= $0 } ?? false
            let summary = browseSummary(
                label: label, rows: grid.rowCount, capped: capped,
                seconds: fetched.milliseconds / 1000)
            if appending {
                result.extend(
                    capped: capped, milliseconds: fetched.milliseconds, summary: summary)
            } else {
                result.finish(
                    capped: capped, milliseconds: fetched.milliseconds, summary: summary)
            }
            isBusy = false
        }
    }

    private func browseSummary(
        label: String, rows: Int, capped: Bool, seconds: Double
    ) -> String {
        let elapsed = String(format: "%.2f", seconds)
        guard capped else {
            return "\(label) · \(Self.pluralized(rows, "row")) · \(elapsed) s"
        }
        let count = Self.formatted(rows)
        // Only a browse is ever capped, so the selected relation is the one
        // these rows came from and its estimate is the right denominator.
        if let estimate = selected?.estimatedRows, estimate > Int64(rows) {
            return "\(label) · first \(count) of ~\(Self.formatted(estimate)) rows · \(elapsed) s"
        }
        return "\(label) · first \(count) rows · \(elapsed) s"
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
        // The core queue is serial, so at most one of these was running; clearing
        // both saves threading the target through the generic dispatch helper.
        browseResult.abandonLoading()
        queryResult.abandonLoading()
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

    /// "1 schema" / "2 schemas". A label that reads "1 objects" is small, but it
    /// is the kind of small that makes a program feel unfinished.
    static func pluralized(_ n: Int, _ singular: String, _ plural: String? = nil) -> String {
        "\(formatted(n)) \(n == 1 ? singular : plural ?? singular + "s")"
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
