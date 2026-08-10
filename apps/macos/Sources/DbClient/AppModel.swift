import CDbFfi
import Foundation
import Observation
import SwiftUI

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

    /// Drops the rows and everything the chrome says about them.
    ///
    /// For a result whose subject has stopped existing. The rows themselves are
    /// still what the server sent, but the summary names a relation — "victim ·
    /// 3 rows" beside a table that has been dropped reads as a claim that it is
    /// still there, and the status bar has no other way to stop making it.
    func discard() {
        table.reset()
        generation += 1
        rowCount = 0
        capped = false
        milliseconds = 0
        summary = ""
        selection = nil
        isLoading = false
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
    /// Set while `refresh` swaps `selected` for the freshly read value naming
    /// the same relation. The two are the same object to a user but not to
    /// `==` — `estimatedRows` moves on its own — and that assignment must not
    /// look like the user picking a table: `selectionChanged` clears the WHERE
    /// and ORDER BY fields, and a refresh that threw the filters away would be
    /// a worse answer than the stale pane it was pressed to fix.
    private var isReselecting = false
    /// Name filter for the navigator. A schema with hundreds of objects is the
    /// normal case, and scrolling to find one is the slowest thing a user does.
    var navigatorFilter = ""
    /// Bumped by the View menu's Filter Objects item.
    ///
    /// Focus lives in a `@FocusState` inside the window's view tree, which an
    /// `NSMenuItem` action cannot reach; this is what carries the request across
    /// that boundary. A counter rather than a flag, because pressing the
    /// shortcut again after clicking away has to move focus back, and assigning
    /// a flag the value it already holds gives `onChange` nothing to see.
    private(set) var filterFocusRequests = 0

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

    /// The statements the pane last ran, in order, each with what it did.
    ///
    /// A run of one is what ⌘R makes and a run of five is what ⌥⌘R makes. Five
    /// statements produce five outcomes and this pane has one grid, so the list
    /// is where the other four go: showing one and saying nothing about the rest
    /// is the class of lie the status bar's "first 100,000 of ~1,000,000 rows"
    /// exists to prevent.
    private(set) var scriptSteps: [ScriptStep] = []

    /// Which step the pane is showing, as an index into `scriptSteps`.
    var selectedStep = 0

    var selectedScriptStep: ScriptStep? {
        scriptSteps.indices.contains(selectedStep) ? scriptSteps[selectedStep] : nil
    }

    /// The Query pane's rows: whichever step is selected.
    ///
    /// Computed rather than one grid the run writes into, because choosing
    /// another statement out of the list must not re-run anything — each step
    /// holds the batches it was handed until the next run replaces the lot.
    var queryResult: ResultSet { selectedScriptStep?.result ?? pristine }

    /// Stands in before anything has run here. A result that has never run is a
    /// different state from a statement that returned nothing, and the pane
    /// draws them differently.
    private let pristine = ResultSet()

    /// The result the chrome is currently describing. Structure has no result of
    /// its own, so it borrows the browse's — the status bar overrides what it
    /// says there anyway.
    var current: ResultSet { activeTab == .query ? queryResult : browseResult }

    // Content pane filters
    var whereClause = ""
    var orderClause = ""

    /// Every statement this window has sent, newest first, kept across launches.
    ///
    /// Held rather than created here so a capture can hand in a scratch store;
    /// see `--history-store`.
    let history: QueryHistory

    /// Whether the history panel under the editor is open.
    ///
    /// Pane state rather than window state, unlike the value viewer: the list
    /// only ever feeds the editor, and there is nothing to read from it on the
    /// other tabs.
    var isHistoryOpen = false

    var queryText = ""
    /// Where the caret or selection is in the editor.
    ///
    /// Owned here rather than by the pane because the Run button lives in the
    /// window's toolbar, which has no view of the editor. ⌘R has to know which
    /// statement the user is standing in, and this is the only place both ends
    /// can see.
    var querySelection: TextSelection?
    /// The last statement `selectionChanged` put in the editor, so a later
    /// selection can tell "untouched suggestion" from "the user's work".
    private var suggestedQueryText = ""

    /// A statement the Query pane sent, carried alongside its result so a server
    /// error position can be turned back into a place in the editor.
    private struct SentStatement: Sendable {
        /// The buffer the statement was cut from. An error arrives after a round
        /// trip, by which time the buffer may have been edited; an offset into
        /// text that no longer exists points at a character nobody asked about,
        /// so the caret only moves while this still matches.
        let script: String
        /// Scalar offsets of the statement within `script`.
        let range: Range<Int>
    }

    /// A Query-pane failure with the statement that produced it.
    ///
    /// Wrapped around the error rather than parked in a property: the core queue
    /// is serial, but a browse issued between ⌘R and its answer would overwrite
    /// a shared slot, and the banner would then point into whichever statement
    /// happened to be there. Travelling with the error, the two cannot come
    /// apart.
    private struct StatementFailure: Error {
        let error: Error
        let sent: SentStatement
    }

    // Chrome
    private(set) var connectionLabel = "Not connected"
    private(set) var connectionState: StatusDot.State = .connecting
    private(set) var status = "Connecting…"
    private(set) var isBusy = false
    /// Set while a result is being written to a file. The write happens off the
    /// main thread, so without this the window would sit looking idle for
    /// however long a million rows take to reach the disk.
    private(set) var isExporting = false
    /// What the status bar reads while that write is in progress. Kept apart
    /// from `status` because a query started during an export overwrites that
    /// one with "Running…" and never puts it back — the export would end up
    /// described by a sentence about something else.
    private(set) var exportStatus = ""
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
    /// Exports get a queue of their own. The core queue is serial because one
    /// connection cannot service two statements; an export holds no connection,
    /// and parking a million-row write in front of the next query would make
    /// clicking a table in the navigator wait on a file.
    private let exportQueue = DispatchQueue(label: "dev.dbclient.export", qos: .userInitiated)
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

    /// Set by `--run-script`: the opening `--sql` is a whole script, and nothing
    /// here runs it. main.swift sends the Query menu's own item once the window
    /// has settled, which is what makes the capture a check of that item's
    /// wiring rather than only of the model behind it.
    private let initialSQLIsScript: Bool

    init(
        connString: String, history: QueryHistory, initialTab: DetailTab = .content,
        initialSQL: String? = nil,
        initialCaret: Int? = nil, initialSQLIsScript: Bool = false,
        initialWhere: String? = nil, initialOrder: String? = nil,
        initialStructureDetail: StructureDetail? = nil, initialRelation: String? = nil,
        initialFilter: String? = nil
    ) {
        self.navigatorFilter = initialFilter ?? ""
        self.history = history
        self.initialSQLIsScript = initialSQLIsScript
        self.initialStructureDetail = initialStructureDetail
        self.initialRelation = initialRelation
        self.connString = connString
        self.activeTab = initialSQL == nil ? initialTab : .query
        self.initialSQL = initialSQL
        self.initialFilters = (initialWhere, initialOrder)
        if let initialSQL { queryText = initialSQL }
        // `--caret` is the only way to put the caret anywhere but the start
        // without a click, and clicking is what a capture cannot do. It defaults
        // to the start whenever there is a statement to open with: left unset,
        // the editor drops the caret at the end of the text the first time it
        // takes it, and a multi-statement `--sql` would then run whichever
        // statement that happened to land in.
        let caret = initialCaret ?? (initialSQL == nil ? nil : 0)
        if let caret, let index = SQLScript.range(caret..<caret, in: queryText) {
            querySelection = TextSelection(insertionPoint: index.lowerBound)
        }
    }

    // MARK: - Lifecycle

    func connect() {
        isBusy = true
        status = "Connecting…"
        run { [connString] in
            let db = try Database(connString: connString)
            return (db, try Self.inventory(of: db))
        } then: { [self] result in
            db = result.0
            schemas = result.1.schemas
            relations = result.1.relations
            connectionLabel = Self.label(for: connString)
            connectionState = .connected
            // Open the schema a user most likely wants, and land on a table
            // rather than an empty pane. Opening to nothing makes every session
            // start with the same two clicks. `--relation` overrides both, and
            // may name a schema of its own.
            let requested = initialRelation.flatMap(findRelation)
            let opening =
                requested.map(\.schema)
                ?? (schemas.first(where: { $0.name == "public" }) ?? schemas.first)?.name
            if let opening {
                expanded.insert(opening)
                selected = requested ?? relations[opening]?.first
            }
            status = Self.pluralized(schemas.count, "schema")
            isBusy = false
            // Runs after the selection above, so an explicit `--sql` replaces
            // the browse rather than racing it. Through the same path ⌘R takes,
            // so a multi-statement `--sql` runs the one `--caret` names rather
            // than the whole buffer — unless `--run-script` says otherwise, and
            // then the menu item is what runs it.
            if initialSQL != nil, !initialSQLIsScript { runCurrentQuery() }
        }
    }

    /// The navigator's whole contents, read in one pass.
    ///
    /// Shared by `connect` and `refresh` so the two cannot drift into loading
    /// different things: a refresh that read less than the connection did would
    /// quietly delete objects from a tree that is supposed to have become more
    /// accurate, not less.
    private nonisolated static func inventory(of db: Database) throws -> Inventory {
        let schemas = try db.schemas()
        var relations: [String: [RelationInfo]] = [:]
        for schema in schemas {
            relations[schema.name] = try db.relations(schema: schema.name)
        }
        return Inventory(schemas: schemas, relations: relations)
    }

    private struct Inventory: Sendable {
        let schemas: [SchemaInfo]
        let relations: [String: [RelationInfo]]
    }

    // MARK: - Refresh

    /// Whether the object tree can be reloaded. False before the connection is
    /// up, and while something is already running: the core queue is serial, so
    /// a second refresh would only queue behind the first and land looking like
    /// a button that did nothing.
    var canRefresh: Bool { db != nil && !isBusy }

    /// Rereads the object tree, and the selected relation with it.
    ///
    /// Deliberately not `connect()`. Only the metadata has gone stale;
    /// reconnecting would throw away the connection and, with it, whatever the
    /// Query tab is holding — a client that loses your result in order to show
    /// you a new table is not one anybody presses twice. Nothing here caches on
    /// the core side either, so every call is a fresh read of pg_catalog.
    func refresh() {
        guard canRefresh else { return }
        isBusy = true
        status = "Refreshing…"
        // A refresh supersedes the previous failure, as a new query does. The
        // "no longer in this database" note below is written from the answer
        // that comes back, not carried over from the last one.
        errorMessage = nil
        // The rows on screen describe the table as it was. Dimming them for the
        // whole reload rather than only for the browse at the end of it is what
        // keeps several metadata round trips from looking like an idle window.
        if selected != nil { browseResult.beginLoading() }
        run { db in
            try Self.inventory(of: db)
        } then: { [self] inventory in
            schemas = inventory.schemas
            relations = inventory.relations
            // A dropped schema should not come back already open if one of the
            // same name is created later; the user never expanded that one.
            expanded.formIntersection(inventory.schemas.map(\.name))
            reselect()
        }
    }

    /// Re-attaches the selection to the object list just read, and reloads what
    /// the detail panes are showing from it.
    private func reselect() {
        guard let previous = selected else {
            settleRefresh()
            return
        }
        guard let current = relations[previous.schema]?.first(where: { $0.id == previous.id })
        else {
            // Keeping the panes as they are would leave them describing a table
            // that is gone, and clearing them without a word would read as the
            // refresh having failed. Nothing else will mention it either: the
            // next thing to notice would be an error from a query the user did
            // not know was already doomed.
            errorMessage =
                "\(previous.schema).\(previous.name) is no longer in this database — "
                + "it was dropped or renamed while this window was open."
            selected = nil
            columns = []
            clearRelationDetail()
            browseResult.discard()
            settleRefresh()
            return
        }
        // Take the new value even though it names the same relation. Only
        // `estimatedRows` can have moved and it is invisible at this size — but
        // the navigator tags its rows with the whole `RelationInfo`, so holding
        // the old one silently unhighlights the row the user is sitting on.
        isReselecting = true
        selected = current
        isReselecting = false
        // Same order as a fresh selection, and for the same reason: the browse
        // orders by the primary key, so the columns have to land first. The
        // detail sections are not cleared on the way — this is the same
        // relation, so its old structure is context worth keeping under the
        // veil rather than a different table's left on screen.
        loadColumns(for: current) { [self] in
            runBrowse()
            loadRelationDetail(for: current)
        }
    }

    /// Ends a refresh that has no relation left to reload. The path through
    /// `runBrowse` clears `isBusy` itself when the rows land.
    private func settleRefresh() {
        status = Self.pluralized(schemas.count, "schema")
        isBusy = false
        browseResult.abandonLoading()
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
        return preferred
            ?? relations.values.lazy.compactMap { list in
                list.first { $0.name == requested }
            }.first
    }

    // MARK: - Navigator

    /// What the filter field is asking for, folded for comparison, or nil when
    /// it is asking for nothing. Trimmed because a trailing space is the normal
    /// residue of typing and never part of an object's name.
    private var filterNeedle: String? {
        let needle = navigatorFilter.trimmingCharacters(in: .whitespaces).lowercased()
        return needle.isEmpty ? nil : needle
    }

    /// Whether the navigator is showing a subset. Read by the tree in several
    /// places, each of which has to behave differently while it is true.
    var isFiltering: Bool { filterNeedle != nil }

    /// Relations in `schema` matching the filter. Matching on a substring
    /// rather than a prefix, because table names are usually reached by the
    /// distinctive word in the middle rather than by what they start with.
    func visibleRelations(in schema: String) -> [RelationInfo] {
        let all = relations[schema] ?? []
        guard let needle = filterNeedle else { return all }
        // A schema whose own name matches keeps everything under it. Typing
        // "reporting" is asking to see that schema, and answering with the
        // subset of its tables that happen to repeat the schema's name in their
        // own would be a different question entirely.
        if schema.lowercased().contains(needle) { return all }
        return all.filter { $0.name.lowercased().contains(needle) }
    }

    /// Whether a schema's disclosure is open.
    ///
    /// While a filter is active every schema with a match opens, so results are
    /// never hidden inside a collapsed group the user cannot see to expand.
    /// `expanded` is left untouched throughout, which is what lets clearing the
    /// field put the tree back exactly as the user last arranged it.
    func isExpanded(_ schema: String) -> Bool {
        if isFiltering { return !visibleRelations(in: schema).isEmpty }
        return expanded.contains(schema)
    }

    var matchedRelationCount: Int {
        schemas.reduce(0) { $0 + visibleRelations(in: $1.name).count }
    }

    var totalRelationCount: Int {
        relations.values.reduce(0) { $0 + $1.count }
    }

    /// Whether the filter is currently hiding the relation the detail panes are
    /// describing.
    var filterHidesSelection: Bool {
        guard isFiltering, let selected else { return false }
        return !visibleRelations(in: selected.schema).contains(selected)
    }

    /// What the navigator's `List` binds to, which is not quite `selected`.
    ///
    /// A `List` reports the selection among the rows it is showing, and under a
    /// filter those are a subset — so the row the user is sitting on can leave
    /// it, and SwiftUI writes nil back. Taking that nil would clear the
    /// Structure and Content panes and throw away the browsed rows: typing in
    /// the filter field would then change what the window says about a table,
    /// not merely which tables are listed. So a nil arriving while the filter
    /// is hiding the selection is read as the list disowning a row rather than
    /// as the user deselecting one.
    ///
    /// The cost, accepted deliberately: while the selection is filtered out the
    /// sidebar shows no highlighted row, and the title bar names a relation the
    /// tree below does not list. The alternative — keeping the selected
    /// relation in the list so its highlight survives — was rejected because it
    /// puts a row that does not match into a list whose entire claim is that
    /// every row in it matches, and a filter that quietly keeps one exception
    /// is a worse lie than a highlight that is briefly off-screen. Clearing the
    /// field brings the row and its highlight straight back.
    var navigatorSelection: RelationInfo? {
        get { selected }
        set {
            if newValue == nil, filterHidesSelection { return }
            selected = newValue
        }
    }

    /// Whether there is anything for the filter field to filter. Drives the
    /// menu item's enabled state, so the command is not offered before the
    /// connection has read a tree to narrow.
    var canFilterObjects: Bool { !schemas.isEmpty }

    func focusNavigatorFilter() { filterFocusRequests += 1 }

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
        /// How the viewer should draw this value when it is open.
        let rendering: ValueRendering
        /// Whether the viewer under the strip is open.
        ///
        /// Carried in the cell rather than passed to the strip, because the
        /// strip is drawn by two panes and neither of them owns this: it is a
        /// window-wide View command, and threading a binding through every pane
        /// that happens to show a grid would make each of them responsible for
        /// state it has no part in.
        let isExpanded: Bool
        /// Opens or closes that viewer, for the chevron in the strip. The menu
        /// item flips the same flag, so the two cannot disagree about which way
        /// the chevron points.
        let toggleExpanded: @MainActor () -> Void
    }

    /// Whether the value viewer under the inspector strip is open.
    ///
    /// Window state rather than pane state: someone comparing a long value
    /// against a query result should not have to reopen it on the way across.
    var isValueViewerOpen = false

    /// Whether there is a value for the viewer to show. Drives the menu item's
    /// enabled state, so the command is never offered when pressing it would
    /// open a pane with nothing in it.
    ///
    /// Deliberately not `inspectedCell(in:) != nil`: answering a Bool would then
    /// copy a whole binary cell out of its Arrow buffer, and menu validation
    /// runs on every ⌥⌘V whether the item is wanted or not.
    var canInspectValue: Bool {
        activeTab != .structure && selectedCell(in: current) != nil
    }

    /// The browsed relation's declared types, keyed by column name, for the grid
    /// header. A browse is `SELECT *`, so every column on screen is one of
    /// these; the Query pane is given none, because its columns need not be.
    var declaredColumnTypes: [String: String] {
        columns.reduce(into: [:]) { $0[$1.name] = $1.dataType }
    }

    /// The selected cell's coordinates, once bounds-checked against the result.
    ///
    /// A result can be replaced while a selection points into the last one, so
    /// every reader of a selection has to do this; sharing it keeps the two
    /// readers from disagreeing about what counts as selected.
    private func selectedCell(in result: ResultSet) -> GridSelection? {
        guard let s = result.selection,
            s.column < result.table.columns.count,
            s.row < result.table.rowCount
        else { return nil }
        return s
    }

    func inspectedCell(in result: ResultSet) -> InspectedCell? {
        let grid = result.table
        guard let s = selectedCell(in: result) else { return nil }
        let name = grid.columns[s.column].name
        let isNull = grid.isNull(row: s.row, column: s.column)
        // The relation's declared type where we have it; the Query tab may
        // return computed columns that no relation describes.
        let declared = columns.first { $0.name == name }?.dataType ?? ""
        // Nothing to render for a NULL, and asking would copy a binary cell out
        // of Arrow to describe a value that is not there.
        let rendering: ValueRendering =
            isNull
            ? .text
            : Self.rendering(
                kind: grid.columns[s.column].kind, declared: declared,
                bytes: { grid.bytes(row: s.row, column: s.column) ?? [] })
        return InspectedCell(
            column: name,
            type: declared,
            value: isNull ? "NULL" : Self.text(of: rendering, in: grid, at: s),
            isNull: isNull,
            // A multi-row selection extends far past the viewport, so the count
            // is the only place it is legible before ⌘C makes it obvious.
            address: s.rows.count > 1
                ? "\(Self.formatted(s.rows.count)) rows selected"
                : "row \(Self.formatted(s.row + 1))",
            rendering: rendering,
            isExpanded: isValueViewerOpen,
            toggleExpanded: { [weak self] in self?.isValueViewerOpen.toggle() })
    }

    /// Which rendering a cell gets, from the two type sources this has.
    ///
    /// The Arrow kind is always true of what arrived and is what identifies a
    /// binary column; the declared type is the only thing that can say `jsonb`,
    /// because the driver maps it to Utf8 like any other string. Neither is the
    /// string itself — a `text` column holding `{}` is text.
    private static func rendering(
        kind: ArrowTable.Kind, declared: String, bytes: () -> [UInt8]
    ) -> ValueRendering {
        switch kind {
        case .binary: return .binary(bytes())
        case .utf8 where ValueRendering.isJSONType(declared): return .json
        default: return .text
        }
    }

    /// The strip's one-line form of a cell, which for a binary column is not
    /// what the grid draws: "12 B" is the most a column-width cell can say, and
    /// the strip has room for the bytes themselves.
    private static func text(
        of rendering: ValueRendering, in grid: ArrowTable, at s: GridSelection
    ) -> String {
        if case .binary(let bytes) = rendering { return ValueRendering.preview(bytes: bytes) }
        return grid.text(row: s.row, column: s.column)
    }

    // MARK: - Selection

    private func selectionChanged(from previous: RelationInfo?) {
        guard !isReselecting, let selected, selected != previous else { return }
        // Filters describe the previous table's columns and cannot be assumed
        // to apply here; carrying them over would produce confusing errors.
        // The first selection is the exception: it is where --where/--order land.
        whereClause = appliedInitialFilters ? "" : (initialFilters.where ?? "")
        orderClause = appliedInitialFilters ? "" : (initialFilters.order ?? "")
        appliedInitialFilters = true
        // Cleared rather than left showing the previous relation's structure
        // while the new one loads.
        clearRelationDetail()
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

    private func loadColumns(for relation: RelationInfo, then next: @escaping @MainActor () -> Void)
    {
        run { db in
            try db.columns(schema: relation.schema, relation: relation.name)
        } then: { [self] cols in
            columns = cols
            next()
        }
    }

    /// Empties every Structure section at once, so a section added later cannot
    /// be forgotten at one of the two places that has to drop the old one.
    private func clearRelationDetail() {
        indexes = []
        foreignKeys = []
        referencedBy = []
        constraints = []
        triggers = []
        definition = nil
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
        let keys =
            columns
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
        // An export outranks whatever the tab would otherwise say, and stops
        // doing so the moment it finishes: nothing has to remember to clear it,
        // and no stale "Exported…" can outlive the thing it described.
        if isExporting { return exportStatus }
        switch activeTab {
        case .structure:
            guard !columns.isEmpty else { return status }
            let keys = columns.filter(\.isPrimaryKey).count
            let keyPart = keys == 0 ? "no primary key" : "\(keys) in primary key"
            return "\(Self.pluralized(columns.count, "column")) · \(keyPart)"
        case .content, .query:
            // A Query step is asked rather than the grid behind it: a statement
            // that returned no rows has no row count to report and a sentence of
            // its own instead, and one that never ran has neither.
            if activeTab == .query, let step = selectedScriptStep { return step.summary }
            // Each pane reports its own result. Falling back to `status` covers
            // the connection messages and the window before anything has run.
            return current.summary.isEmpty ? status : current.summary
        }
    }

    /// Whether ⌘R has anything to run. Drives the Run button's disabled state,
    /// so the button is never offered when pressing it would do nothing.
    ///
    /// A buffer holding only comments has text in it and nothing to run, which
    /// is why this asks the splitter rather than measuring the string.
    var canRun: Bool {
        activeTab == .query ? !SQLScript.statements(in: queryText).isEmpty : selected != nil
    }

    /// The caret or selection as scalar offsets into `queryText`.
    ///
    /// `TextSelection` carries `String.Index` values, which mean nothing away
    /// from the string they were made against. Converting on read rather than
    /// storing offsets is what keeps them from outliving an edit.
    private var editorSelection: Range<Int> {
        func offset(_ index: String.Index) -> Int {
            let bounded = min(index, queryText.endIndex)
            return queryText.unicodeScalars.distance(from: queryText.startIndex, to: bounded)
        }
        guard let indices = querySelection?.indices else { return 0..<0 }
        switch indices {
        case .selection(let range):
            return offset(range.lowerBound)..<offset(range.upperBound)
        case .multiSelection(let set):
            // A discontiguous selection names no single statement; its first run
            // is what ⌘R acts on.
            guard let first = set.ranges.first else { return 0..<0 }
            return offset(first.lowerBound)..<offset(first.upperBound)
        @unknown default:
            // A shape this build does not know how to read is no worse than no
            // selection: the caret rule takes over from the start of the buffer.
            return 0..<0
        }
    }

    /// What ⌘R would send right now. The editor's corner reads it out, because
    /// "which of these five is about to run" is the question a script raises and
    /// a blinking caret does not answer.
    var runTarget: SQLScript.Target? {
        SQLScript.target(in: queryText, selection: editorSelection)
    }

    func runCurrentQuery() {
        // ⌘R means "run what I am looking at". In the Query tab that is one
        // statement — the selection if there is one, otherwise the statement the
        // caret sits in — not the whole buffer, which stopped being a sensible
        // reading the moment a buffer could hold a script.
        guard activeTab == .query else {
            runBrowse()
            return
        }
        guard let target = runTarget else { return }
        runStatements([target.range], labelled: [target.label])
    }

    /// Whether ⌥⌘R has anything to run. Includes `isBusy`, unlike `canRun`,
    /// because this is what greys the menu item out: the core queue is serial,
    /// so a second run would only queue behind the first and land looking like a
    /// command that did nothing.
    var canRunScript: Bool {
        activeTab == .query && !isBusy && !SQLScript.statements(in: queryText).isEmpty
    }

    /// Runs every statement in the buffer, in order, stopping at the first that
    /// fails.
    ///
    /// Deliberately not wrapped in a transaction. Each statement goes out on its
    /// own, exactly as ⌘R sends it, so a script that half-succeeds has half
    /// happened — and the outcome list is what says which half. An implicit
    /// BEGIN…COMMIT around the buffer would change the atomicity of what the
    /// user typed without being asked, and it cannot even be done honestly:
    /// CREATE INDEX CONCURRENTLY and VACUUM refuse to run inside a transaction
    /// block, so statements that work in psql would start failing here; and a
    /// script containing its own COMMIT would end the client's wrapper halfway
    /// through, leaving the rest unwrapped while the window went on claiming
    /// otherwise. The statements share one connection, which is what makes a
    /// BEGIN the user wrote cover the statements after it — atomicity stays
    /// something a script asks for rather than something this imposes.
    func runScript() {
        guard canRunScript else { return }
        let all = SQLScript.statements(in: queryText)
        guard !all.isEmpty else { return }
        // A buffer holding one statement is "query" here as it is under ⌘R, so
        // running a one-liner whole is described exactly as it always was.
        let labels =
            all.count == 1
            ? ["query"] : all.indices.map { "statement \($0 + 1) of \(all.count)" }
        runStatements(all, labelled: labels)
    }

    /// Runs `ranges` of the editor buffer in order on the one connection, and
    /// installs an outcome for each.
    ///
    /// The whole run is a single trip to the core queue rather than one trip per
    /// statement. The queue is serial anyway — one connection cannot service two
    /// statements — so hopping back to the main actor between them would buy
    /// nothing but a chance for a browse to interleave into the middle of
    /// somebody's script.
    private func runStatements(_ ranges: [Range<Int>], labelled labels: [String]) {
        isBusy = true
        // The step on screen dims for the duration. Blanking the pane would lose
        // the result the user is comparing against, and the veil is the
        // vocabulary the browse already uses for exactly this.
        queryResult.beginLoading()
        status = "Running…"
        // A new run supersedes the previous failure; leaving the banner up would
        // attribute an old error to the outcomes now on screen.
        errorMessage = nil
        let batchRows = self.batchRows
        // The buffer as it is now. An error arrives after a round trip, and the
        // caret may only be moved while the text it indexes still exists.
        let script = queryText
        let sql = ranges.map { SQLScript.text($0, in: script) }

        run { db -> ScriptOutput in
            var completed: [StatementOutput] = []
            for text in sql {
                let started = CFAbsoluteTimeGetCurrent()
                do {
                    let query = try db.query(text, batchRows: batchRows)
                    let schema = try query.schema()
                    var batches: [UnsafeMutablePointer<ArrowArray>] = []
                    // Pulled to exhaustion even when there is nothing to pull.
                    // `query` returns once the server has acknowledged the bind,
                    // which is before it executes anything — so a duplicate
                    // relation or a violated constraint is still ahead of us,
                    // and so is the count a statement without rows reports.
                    while let batch = try query.nextBatch() {
                        batches.append(batch)
                    }
                    completed.append(
                        StatementOutput(
                            schema: schema, batches: batches,
                            rowsAffected: query.rowsAffected ?? 0,
                            milliseconds: (CFAbsoluteTimeGetCurrent() - started) * 1000))
                } catch {
                    // Returned rather than thrown: the statements that already
                    // ran are results the user needs, and `dispatch` discards
                    // the value of a throwing stage. Stopping here is the point
                    // — the statements after this one are not sent.
                    return ScriptOutput(
                        completed: completed,
                        failure: error as? DbError
                            ?? DbError(description: String(describing: error)))
                }
            }
            return ScriptOutput(completed: completed, failure: nil)
        } then: { [self] output in
            install(output, ranges: ranges, statements: sql, labels: labels, script: script)
        }
    }

    /// Turns a finished run into the steps the pane shows.
    private func install(
        _ output: ScriptOutput, ranges: [Range<Int>], statements: [String], labels: [String],
        script: String
    ) {
        var steps: [ScriptStep] = []
        for (i, out) in output.completed.enumerated() {
            let result = ResultSet()
            let grid = result.table
            grid.setSchema(out.schema)
            if let release = out.schema.pointee.release { release(out.schema) }
            out.schema.deallocate()
            for batch in out.batches {
                grid.append(batch: batch)
            }
            // No columns at all is the server's own answer to "did this return
            // rows", and it is not the same answer as a result set that happened
            // to be empty. An UPDATE and a SELECT that matched nothing both show
            // no rows, and only one of them changed the database.
            let outcome: StatementOutcome =
                grid.columns.isEmpty
                ? .completed(affected: out.rowsAffected) : .rows(grid.rowCount)
            let summary = Self.stepSummary(
                label: labels[i], outcome: outcome, milliseconds: out.milliseconds)
            // Nothing in the Query pane imposes a LIMIT, so nothing it returns
            // is capped: what is on screen is the whole of what the statement
            // produced.
            result.finish(capped: false, milliseconds: out.milliseconds, summary: summary)
            steps.append(
                ScriptStep(
                    id: i + 1, sql: statements[i], range: ranges[i], summary: summary,
                    outcome: outcome, result: result))
        }

        // A run stops at the first failure, so the failed statement is the one
        // after the last that completed, and everything past it never ran. Those
        // rows have to say so: a list that goes on looking the same below the
        // failure claims work that did not happen.
        let stopped = output.failure.map { _ in output.completed.count }
        if let failure = output.failure, let stopped {
            for i in stopped..<ranges.count {
                let outcome: StatementOutcome =
                    i == stopped ? .failed(failure.description) : .notRun
                steps.append(
                    ScriptStep(
                        id: i + 1, sql: statements[i], range: ranges[i],
                        summary: Self.stepSummary(
                            label: labels[i], outcome: outcome, milliseconds: 0),
                        outcome: outcome, result: ResultSet()))
            }
        }

        scriptSteps = steps
        // Recorded in the order the statements went out, and only those that
        // did: this is the one place every statement the Query pane sends passes
        // through, whether ⌘R sent one or ⌥⌘R sent forty, and the steps past a
        // failure never reached the server.
        for step in steps {
            guard let outcome = QueryHistoryOutcome(step.outcome) else { continue }
            history.record(step.sql, outcome: outcome)
        }
        // Where the eye should go. A run that stopped has exactly one place
        // worth looking and it is the statement that stopped it; a run that
        // finished lands on the last statement that returned anything, which is
        // where a script that ends by checking its own work keeps the answer.
        selectedStep =
            stopped
            ?? steps.lastIndex { $0.outcome.hasGrid }
            ?? max(steps.count - 1, 0)
        isBusy = false

        if let failure = output.failure, let stopped {
            // Through `fail`, so the banner, the status word and the caret are
            // the same ones every other failure gets.
            self.fail(
                with: StatementFailure(
                    error: failure, sent: SentStatement(script: script, range: ranges[stopped])))
        }
    }

    /// What the status bar reads for one step of a run.
    private static func stepSummary(
        label: String, outcome: StatementOutcome, milliseconds: Double
    ) -> String {
        switch outcome {
        case .rows, .completed:
            let elapsed = String(format: "%.2f", milliseconds / 1000)
            return "\(label) · \(outcome.label) · \(elapsed) s"
        case .failed, .notRun:
            // No elapsed time: for one of these there is nothing to time, and
            // for the other the number would sit beside "failed" looking like a
            // measurement of the answer rather than of the wait.
            return "\(label) · \(outcome.label)"
        }
    }

    // MARK: - History

    /// Whether the history can be shown. The panel lives in the Query pane, so
    /// off that tab the menu item would open something nobody can see.
    var canShowHistory: Bool { activeTab == .query }

    /// Puts a statement from the history back in the editor, selected so that
    /// ⌘R sends exactly it.
    ///
    /// Appended rather than swapped in. Selecting a table in the navigator used
    /// to overwrite this buffer and silently discard whatever the user was in
    /// the middle of; a history that did the same would be a second way to lose
    /// the same work, reached from a list they opened to avoid retyping. The
    /// editor already holds scripts and ⌘R already means "the statement I am
    /// standing in", so a recalled statement arriving as one more of them needs
    /// no new idea — and the selection is what makes it the one that runs.
    ///
    /// The panel closes on the way out: the pick is the whole transaction, and
    /// the rows it was occupying are worth more to the result below it.
    func recall(_ entry: QueryHistoryEntry) {
        activeTab = .query
        isHistoryOpen = false
        let existing = queryText.trimmingCharacters(in: .whitespacesAndNewlines)
        // The splitter strips the terminator from a statement, so a buffer that
        // ends without one would run straight into what is being appended and
        // the two would go to the server as a single statement.
        let prefix = existing.isEmpty ? "" : existing + (existing.hasSuffix(";") ? "\n\n" : ";\n\n")
        queryText = prefix + entry.sql
        let start = prefix.unicodeScalars.count
        let end = start + entry.sql.unicodeScalars.count
        if let selection = SQLScript.range(start..<end, in: queryText) {
            querySelection = TextSelection(range: selection)
        }
    }

    // MARK: - Export

    /// Whether there is a result to write out. The menu item is disabled when
    /// there is not, rather than opening a save panel that can only produce a
    /// file holding a header line.
    var canExport: Bool { current.rowCount > 0 && !current.isLoading && !isExporting }

    /// The name the save panel proposes.
    ///
    /// A capped result is a page of a table, not the table, and the name is
    /// where that has to be said. The status bar's "first 100,000 of
    /// ~1,000,000 rows" and the marker beside it stop existing the moment the
    /// panel closes; the file goes on being opened, mailed and loaded by people
    /// who were never in the room. `bench_wide.csv` holding a tenth of
    /// bench_wide is the same lie `pagingObstacle` and `truncationHelp` exist
    /// to prevent, except that it outlives the window.
    ///
    /// Refusing to export a capped result would be the wrong fix — the first
    /// page is very often exactly what someone wants — and a comment row inside
    /// the file would be worse still, because it is a row that is not data and
    /// every parser downstream would read it as one.
    func exportFilename(_ format: DelimitedFormat) -> String {
        let base = activeTab == .query ? "query" : (selected?.name ?? "result")
        // Raw digits, not `formatted`: a comma in the name of a CSV file is a
        // joke that stops being funny at the first script that splits on one.
        let suffix = current.capped ? "-first-\(current.rowCount)-rows" : ""
        return "\(base)\(suffix).\(format.fileExtension)"
    }

    /// What the save panel says above the name field.
    ///
    /// Says the same thing as `truncationHelp`, in the same order and for the
    /// same reason, because this is the last moment at which it can be said.
    var exportMessage: String {
        let count = Self.formatted(current.rowCount)
        guard current.capped else {
            return "Writes this result in full — \(Self.pluralized(current.rowCount, "row"))."
        }
        let shown =
            "This result is the first \(count) rows, not the whole table. "
            + "Only those rows will be written."
        guard let obstacle = pagingObstacle else { return shown }
        return "\(shown) \(obstacle.detail)"
    }

    /// Writes the result the window is showing to `url`.
    ///
    /// The rows are snapshotted on the way out and formatted on the export
    /// queue. The snapshot is what makes that safe: it owns the Arrow batches,
    /// so re-running the query while the file is still being written replaces
    /// the grid without pulling the buffers out from under the writer.
    func exportCurrentResult(to url: URL, format: DelimitedFormat) {
        guard canExport else { return }
        let rows = current.table.snapshot()
        isExporting = true
        exportStatus =
            "Exporting \(Self.pluralized(rows.rowCount, "row")) to \(url.lastPathComponent)…"
        // A new export supersedes the previous failure, as a new query does.
        errorMessage = nil
        dispatch(on: exportQueue) {
            try DelimitedWriter.write(rows, format: format, to: url)
        } then: { [self] _ in
            isExporting = false
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
    ///
    /// The browse's path. The Query pane runs through `runStatements`, which has
    /// to keep N results rather than install one.
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
        dispatch(on: queue, { try work(db) }, then: apply)
    }

    private func run<T>(
        _ work: @escaping @Sendable () throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        dispatch(on: queue, work, then: apply)
    }

    private func dispatch<T>(
        on queue: DispatchQueue,
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
        // A Query-pane failure arrives wrapped in the statement that caused it;
        // everything else is its own error.
        let statement = error as? StatementFailure
        errorMessage = String(describing: statement?.error ?? error)
        status = "Failed"
        isBusy = false
        isExporting = false
        // The core queue is serial, so at most one of these was running; clearing
        // both saves threading the target through the generic dispatch helper.
        browseResult.abandonLoading()
        queryResult.abandonLoading()
        // A failed statement says nothing about the connection; only a failure
        // before one exists does.
        if db == nil { connectionState = .failed }
        if let statement { pointAtSyntaxError(statement) }
    }

    /// Says where the server found the trouble, and puts the editor's selection
    /// on it.
    ///
    /// The banner has always said what is wrong and never where, which for a
    /// syntax error in a hundred lines of SQL is most of the answer missing.
    ///
    /// The arithmetic is the whole difficulty. PostgreSQL counts from 1, in
    /// characters, from the start of the string it was handed — and what it was
    /// handed is one statement, not the buffer. Applying the number to the
    /// buffer points confidently at a character in the wrong statement, and
    /// looks right every time the statement that failed happened to be the
    /// first. `SQLScript.errorOffset` is where that translation lives, and the
    /// buffer is checked against the one the statement was cut from before any
    /// of it is trusted: pointing at the wrong character is worse than not
    /// pointing, so an edited buffer gets the bare position and no caret move.
    private func pointAtSyntaxError(_ statement: StatementFailure) {
        guard let failure = statement.error as? DbError, let position = failure.position
        else { return }
        let sent = statement.sent
        guard sent.script == queryText,
            let offset = SQLScript.errorOffset(ofPosition: position, in: sent.range),
            let selection = SQLScript.range(
                SQLScript.tokenRange(at: offset, in: queryText), in: queryText)
        else {
            errorMessage = "\(failure.description) · at position \(position) of the statement"
            return
        }
        let place = SQLScript.lineColumn(of: offset, in: queryText)
        errorMessage = "\(failure.description) · line \(place.line), column \(place.column)"
        querySelection = TextSelection(range: selection)
    }

    private static func label(for connString: String) -> String {
        // "host=… dbname=…" → "dbname@host", which is how these tools name a
        // session and how users refer to one.
        var host = "localhost"
        var dbname = "database"
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

/// One statement of a script run, on the same journey and for the same reason.
private struct StatementOutput: @unchecked Sendable {
    let schema: UnsafeMutablePointer<ArrowSchema>
    let batches: [UnsafeMutablePointer<ArrowArray>]
    /// What the server said the statement affected. Meaningful only where the
    /// schema has no columns; a statement that returned rows is described by the
    /// rows it returned.
    let rowsAffected: Int
    let milliseconds: Double
}

/// A whole run: what completed, and what stopped it.
///
/// The failure rides in the value rather than being thrown, because the
/// statements that already ran are results the user needs and a thrown error
/// takes the value with it. Its index is `completed.count` — a run stops at the
/// first failure, so there is nowhere else it can be.
private struct ScriptOutput: @unchecked Sendable {
    let completed: [StatementOutput]
    let failure: DbError?
}
