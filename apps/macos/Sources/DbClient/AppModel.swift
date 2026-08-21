import AppKit
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
    /// Whether the fetch in flight is adding to rows already on screen.
    ///
    /// Beside `isLoading` rather than in place of it. Everything that asks "is a
    /// fetch in flight" — `canLoadMore`, `canExport`, and the capture helpers
    /// that wait for one to finish — keeps meaning what it meant, and this
    /// answers the one further question the veil has: whether there is anything
    /// behind it worth reading.
    private(set) var isExtending = false
    var selection: GridSelection?

    var hasRun: Bool { generation > 0 }

    /// Whether to dim what is on screen while this fetch runs.
    ///
    /// A first page has nothing behind it worth keeping: the grid is empty, or
    /// it is holding the rows of the table the user has just navigated away
    /// from, and leaving either undimmed presents it as the answer to the
    /// question just asked. A page appended to rows the reader is already
    /// reading is the opposite case — those rows are theirs, they asked for more
    /// of them, and covering them stops the reading the fetch exists to extend.
    /// Sequel Ace's `tableLoadTimer` is this lesson: watching data arrive is
    /// materially different from waiting for it.
    var isVeiled: Bool { isLoading && !isExtending }

    /// Assigns rather than raises, both times. A browse begun after a *Load
    /// more* is not an extension of anything, and a flag only ever turned on
    /// would leave every browse for the rest of the session undimmed.
    func beginLoading(appending: Bool = false) {
        isLoading = true
        isExtending = appending
    }

    func abandonLoading() {
        isLoading = false
        isExtending = false
    }

    /// The statement these rows came from, for anything that has to ask the
    /// server the same question again.
    ///
    /// Export is that: it re-reads through a cursor of its own rather than
    /// writing out the rows the grid is holding, because those are as many as
    /// the grid stopped at. Kept here rather than recomposed from `selected` or
    /// `queryText`, both of which the user can change while a result is still
    /// on screen — and a file written from a statement the result did not come
    /// from is wrong in the way nobody checks for.
    private(set) var statement = ""

    /// Publishes rows already appended to `table` by the caller.
    func finish(statement: String, capped: Bool, milliseconds: Double, summary: String) {
        self.statement = statement
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
        statement = ""
        selection = nil
        isLoading = false
        isExtending = false
    }

    private func publish(capped: Bool, milliseconds: Double, summary: String) {
        rowCount = table.rowCount
        self.capped = capped
        self.milliseconds = milliseconds
        self.summary = summary
        isLoading = false
        isExtending = false
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
    // Navigator. The active session's, like the chrome below it: these describe
    // one connection's database, and the same schema name means a different
    // thing on the next server.
    private(set) var schemas: [SchemaInfo] {
        get { session.schemas }
        set { session.schemas = newValue }
    }
    private(set) var relations: [String: [RelationInfo]] {
        get { session.relations }
        set { session.relations = newValue }
    }
    var expanded: Set<String> {
        get { session.expanded }
        set { session.expanded = newValue }
    }
    /// The `didSet` this used to carry, written out. A computed property cannot
    /// have one, and the observer is not incidental: it is what clears the WHERE
    /// and ORDER BY fields when a user picks a different table.
    var selected: RelationInfo? {
        get { session.selected }
        set {
            let previous = session.selected
            session.selected = newValue
            selectionChanged(from: previous)
        }
    }
    /// Set while `refresh` swaps `selected` for the freshly read value naming
    /// the same relation. The two are the same object to a user but not to
    /// `==` — `estimatedRows` moves on its own — and that assignment must not
    /// look like the user picking a table: `selectionChanged` clears the WHERE
    /// and ORDER BY fields, and a refresh that threw the filters away would be
    /// a worse answer than the stale pane it was pressed to fix.
    private var isReselecting: Bool {
        get { session.isReselecting }
        set { session.isReselecting = newValue }
    }
    /// Name filter for the navigator. A schema with hundreds of objects is the
    /// normal case, and scrolling to find one is the slowest thing a user does.
    var navigatorFilter: String {
        get { session.navigatorFilter }
        set { session.navigatorFilter = newValue }
    }
    /// Bumped by the View menu's Filter Objects item.
    ///
    /// Focus lives in a `@FocusState` inside the window's view tree, which an
    /// `NSMenuItem` action cannot reach; this is what carries the request across
    /// that boundary. A counter rather than a flag, because pressing the
    /// shortcut again after clicking away has to move focus back, and assigning
    /// a flag the value it already holds gives `onChange` nothing to see.
    private(set) var filterFocusRequests = 0

    // Detail
    /// Which pane is showing.
    ///
    /// Recorded in the history on its way through, because moving between a
    /// table's structure and its rows is moving: Back from the rows should mean
    /// the description of the same table, not the table before it.
    var activeTab: DetailTab {
        get { session.activeTab }
        set {
            session.activeTab = newValue
            recordVisit()
        }
    }
    private(set) var columns: [ColumnInfo] {
        get { session.columns }
        set { session.columns = newValue }
    }
    /// Which of those columns name one row, as the core decides it. Read
    /// alongside the columns, because every question about editing is a question
    /// about this one.
    private(set) var rowIdentity: RowIdentity? {
        get { session.rowIdentity }
        set { session.rowIdentity = newValue }
    }
    private(set) var indexes: [IndexInfo] {
        get { session.indexes }
        set { session.indexes = newValue }
    }
    private(set) var foreignKeys: [RelationshipInfo] {
        get { session.foreignKeys }
        set { session.foreignKeys = newValue }
    }
    private(set) var referencedBy: [RelationshipInfo] {
        get { session.referencedBy }
        set { session.referencedBy = newValue }
    }
    private(set) var constraints: [ConstraintInfo] {
        get { session.constraints }
        set { session.constraints = newValue }
    }
    private(set) var triggers: [TriggerInfo] {
        get { session.triggers }
        set { session.triggers = newValue }
    }
    /// The statements that would recreate the selected relation. Nil where the
    /// core cannot write them, which is what keeps the DDL section off a
    /// relation it would have nothing to show for.
    private(set) var ddl: String? {
        get { session.ddl }
        set { session.ddl = newValue }
    }

    // Content pane
    var browseResult: ResultSet { session.browseResult }

    private var browseStore: BrowseStore {
        get { session.browseStore }
        set { session.browseStore = newValue }
    }

    private var stateToRestore: BrowseState? {
        get { session.stateToRestore }
        set { session.stateToRestore = newValue }
    }

    // Query pane

    private(set) var scriptSteps: [ScriptStep] {
        get { session.scriptSteps }
        set { session.scriptSteps = newValue }
    }

    var selectedStep: Int {
        get { session.selectedStep }
        set { session.selectedStep = newValue }
    }

    var selectedScriptStep: ScriptStep? {
        scriptSteps.indices.contains(selectedStep) ? scriptSteps[selectedStep] : nil
    }

    /// The Query pane's rows: whichever step is selected.
    ///
    /// Computed rather than one grid the run writes into, because choosing
    /// another statement out of the list must not re-run anything — each step
    /// holds the batches it was handed until the next run replaces the lot.
    var queryResult: ResultSet { selectedScriptStep?.result ?? pristine }

    private var pristine: ResultSet { session.pristine }

    /// The result the chrome is currently describing. Structure has no result of
    /// its own, so it borrows the browse's — the status bar overrides what it
    /// says there anyway.
    var current: ResultSet { activeTab == .query ? queryResult : browseResult }

    // Content pane filters
    var whereClause: String {
        get { session.whereClause }
        set { session.whereClause = newValue }
    }
    var orderClause: String {
        get { session.orderClause }
        set { session.orderClause = newValue }
    }

    private(set) var filterRules: [FilterRule] {
        get { session.filterRules }
        set { session.filterRules = newValue }
    }

    private(set) var compiledClause: String {
        get { session.compiledClause }
        set { session.compiledClause = newValue }
    }

    private(set) var filterColumns: [FilterColumn] {
        get { session.filterColumns }
        set { session.filterColumns = newValue }
    }

    /// Whether the Custom field may be typed into.
    ///
    /// False while there are rows. The two are mutually exclusive by the plan's
    /// own rule — no merged half-SQL — and a field that is visibly disabled is
    /// how somebody finds out that removing the rows is what gives it back,
    /// rather than typing into it and watching the text be ignored.
    var isCustomFilterEditable: Bool { filterRules.isEmpty }

    /// The WHERE the browse will send: what the rows compiled to where there are
    /// rows, and the Custom field's text where there are not.
    ///
    /// A property rather than a choice made inside `browseAsk`, because this is
    /// the mutual exclusion itself and a rule nothing can read is a rule nothing
    /// can check.
    var browsePredicate: String {
        (filterRules.isEmpty ? whereClause : compiledClause)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Every statement this window has sent, newest first, kept across launches.
    ///
    /// Held rather than created here so a capture can hand in a scratch store;
    /// see `--history-store`.
    let history: QueryHistory

    /// The statements kept by name, across launches.
    ///
    /// A store of its own beside the history rather than a flag on its entries.
    /// The two answer different questions — what did I run, what do I always
    /// run — they are read in different orders, and a history that drops its
    /// oldest entry to stay under its limit must not be able to drop something
    /// somebody took the trouble to name.
    ///
    /// Held rather than created here for the reason the history is: a check
    /// hands in a scratch store.
    let favorites: QueryFavorites

    /// The settings the window reads. Held rather than reached for through a
    /// global so that a check can hand in a scratch store, the way the history
    /// is — and so the Settings window and this model are demonstrably looking
    /// at one object rather than two copies that agree at launch.
    let preferences: Preferences

    /// Whether the history panel under the editor is open.
    ///
    /// Pane state rather than window state, unlike the value viewer: the list
    /// only ever feeds the editor, and there is nothing to read from it on the
    /// other tabs.
    var isHistoryOpen = false

    /// Whether the filter rows are showing under the browse's filter bar.
    ///
    /// One flag for the window rather than one per table. Closing it is a
    /// statement about how much room the bar should take, not about the table
    /// that happened to be open — and a disclosure that reopened itself on every
    /// third table would be a control fighting whoever shut it.
    ///
    /// Shutting it does not hide what is running. The toggle carries the number
    /// of rows, and the Custom field beside it goes on showing the WHERE they
    /// compiled to.
    var isFilterRowsOpen = false

    /// Whether the history list shows the statements nobody typed.
    ///
    /// Off to begin with, which is the list this panel has always been. Someone
    /// opens it to find a statement they wrote, and browses outnumber those
    /// within a minute of clicking about in the sidebar. Turning it on is what
    /// makes this the console: every statement the window sent, in the order it
    /// sent them.
    var showsAllStatements = false

    var historyFilter: String {
        get { session.historyFilter }
        set { session.historyFilter = newValue }
    }

    /// The entries the panel draws, in the store's own order.
    ///
    /// Both narrowings in one place because the count in the header, the rows
    /// themselves and the sentence shown when there are none all have to agree
    /// about what is being shown. Three readers deciding separately is three
    /// chances for a header to promise a row the list does not draw.
    var shownHistory: [QueryHistoryEntry] {
        let needle = historyFilter.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return history.entries.filter { entry in
            guard showsAllStatements || entry.origin == .query else { return false }
            return needle.isEmpty || entry.sql.lowercased().contains(needle)
        }
    }

    /// The two lists that panel can show.
    enum QueryPanelTab: String, CaseIterable, Identifiable {
        case history
        case favorites

        var id: String { rawValue }

        /// What the control reads. Written here rather than at the control so
        /// that the two cases cannot be labelled in one place and switched on in
        /// another.
        var title: String { self == .history ? "History" : "Favorites" }
    }

    /// Which of the two the panel is showing.
    ///
    /// Kept apart from `isHistoryOpen` rather than folded into it: picking a
    /// statement closes the panel, and somebody working out of their favorites
    /// would otherwise be put back on the history every single time.
    var queryPanelTab = QueryPanelTab.history

    /// One SQL editor buffer. A session tab holds one of these; the editor
    /// always edits the active one. The buffer owns only its text and its
    /// name — results, history and favorites stay on the model, because they
    /// describe the connection's work rather than one buffer's.
    struct QueryBuffer: Identifiable {
        let id = UUID()
        var name: String
        var text = ""
    }

    var queryBuffers: [QueryBuffer] {
        get { session.queryBuffers }
        set { session.queryBuffers = newValue }
    }
    var activeQueryBufferIndex: Int {
        get { session.activeQueryBufferIndex }
        set { session.activeQueryBufferIndex = newValue }
    }

    /// The active buffer's text, under the name the rest of the model has
    /// always used, so that splitting one buffer into a list changed no caller.
    var queryText: String {
        get { queryBuffers[activeQueryBufferIndex].text }
        set { queryBuffers[activeQueryBufferIndex].text = newValue }
    }

    /// Moves the editor onto another buffer.
    ///
    /// The caret goes to the end of the text arrived at, which is the same
    /// answer `formatQuery` gives after it replaces the buffer wholesale — and
    /// it is the only answer available: a selection is offsets into the text it
    /// was made in, and carrying one across would point into a string that does
    /// not have those characters.
    func selectQueryBuffer(_ index: Int) {
        guard queryBuffers.indices.contains(index), index != activeQueryBufferIndex else {
            return
        }
        activeQueryBufferIndex = index
        querySelection = TextSelection(insertionPoint: queryText.endIndex)
    }

    /// A new buffer, named for its place in the list, and opened.
    ///
    /// Opened rather than merely appended: a tab that appears without the
    /// editor moving into it is a tab that did nothing.
    func addQueryBuffer() {
        queryBuffers.append(QueryBuffer(name: "query \(queryBuffers.count + 1)"))
        selectQueryBuffer(queryBuffers.count - 1)
    }

    /// Closes a buffer, keeping the editor on the text somebody was typing in.
    ///
    /// The last buffer cannot be closed: an editor with nowhere to type is not
    /// a state this window has. Closing one below the active index moves that
    /// index down with the list rather than leaving it pointing at a neighbour,
    /// which would swap the text under the caret.
    func closeQueryBuffer(_ index: Int) {
        guard queryBuffers.count > 1, queryBuffers.indices.contains(index) else { return }
        queryBuffers.remove(at: index)
        if activeQueryBufferIndex >= queryBuffers.count {
            activeQueryBufferIndex = queryBuffers.count - 1
        } else if index < activeQueryBufferIndex {
            activeQueryBufferIndex -= 1
        }
    }

    var querySelection: TextSelection? {
        get { session.querySelection }
        set { session.querySelection = newValue }
    }
    private var suggestedQueryText: String {
        get { session.suggestedQueryText }
        set { session.suggestedQueryText = newValue }
    }

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
        /// What was written in front of the statement before it was sent, empty
        /// where nothing was. It travels with the script and the range because
        /// the arithmetic that turns the server's number into a place in the
        /// buffer needs all three of them or none.
        let prefix: String
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

    /// The connections this window holds, and which one it is showing.
    ///
    /// A window is a list of connections and a pointer into it. Everything a
    /// pane draws is read from the one the pointer names, which is why switching
    /// tabs is one assignment and needs no saving or putting back: the state
    /// never left the connection it describes.
    private(set) var sessions: [Session] = [Session()]
    private(set) var activeSession = 0

    /// The session an arriving result belongs to, while it is being applied.
    ///
    /// Set only by `dispatch`, and only across the synchronous body of one apply
    /// block. Those blocks write through the forwarding properties below, which
    /// otherwise resolve to whatever tab is in front — and the tab in front is
    /// not necessarily the one that asked the question. A page fetched for a
    /// connection somebody has since switched away from has to land in that
    /// connection's grid, not in the one they are looking at now.
    ///
    /// Dynamic scope rather than a session parameter threaded through two hundred
    /// apply blocks. It is sound because those blocks are synchronous and on the
    /// main actor: nothing can run between the two assignments that bracket one.
    ///
    /// `@ObservationIgnored` deliberately. It is put back before the run loop
    /// gets its turn, so a view that tracked it would be invalidated twice per
    /// arriving result over a value it can never see.
    @ObservationIgnored private var applyingTo: Session?

    /// The session a connection attempt is filling, while it is in flight.
    ///
    /// Nil at rest. Held rather than searched for again: which tab an attempt
    /// made has one answer, and finding it a second time by identity is a second
    /// answer that can disagree with the first.
    private var sessionBeingOpened: Session?

    /// The session everything below reaches: the one a result is being applied
    /// into, or else the one in front.
    private var session: Session { applyingTo ?? sessions[activeSession] }

    // Chrome. Each of these is the active session's, kept under the name it has
    // always had so that no pane and no check has to learn a new one. The
    // forwarding is what lets a second session exist without a second copy of
    // the state: there is one place a connection's label is written, and it is
    // the session that connected.
    private(set) var connectionLabel: String {
        get { session.connectionLabel }
        set { session.connectionLabel = newValue }
    }
    private(set) var connectionState: StatusDot.State {
        get { session.connectionState }
        set { session.connectionState = newValue }
    }
    private(set) var connectionColor: ConnectionColor {
        get { session.connectionColor }
        set { session.connectionColor = newValue }
    }
    private(set) var capabilities: Capabilities {
        get { session.capabilities }
        set { session.capabilities = newValue }
    }
    private(set) var safety: ConnectionSafety {
        get { session.safety }
        set { session.safety = newValue }
    }
    private(set) var status: String {
        get { session.status }
        set { session.status = newValue }
    }

    /// What `status` says when the connection is not doing anything.
    ///
    /// Named because it is now written from four places and every one of them
    /// has to agree: two that arrive at rest (a connection landing, a refresh
    /// settling) and two that are putting "Running…" back after the work it
    /// described has finished. It used not to be put back at all, and the
    /// omission was visible — the Query tab before anything has run has no
    /// result to describe, falls through to `status`, and so sat reading
    /// "Running…" for the rest of the session over a window where nothing was.
    ///
    /// `exportStatus` and `importStatus` below are not that bug worked around,
    /// which is what this note used to claim: they outrank the tab's own summary
    /// and survive a query run while a file is being written, and `status` can do
    /// neither. See the note on `exportStatus`.
    private var settledStatus: String { Self.pluralized(schemas.count, "schema") }

    private(set) var isBusy: Bool {
        get { session.isBusy }
        set { session.isBusy = newValue }
    }
    private(set) var isExporting: Bool {
        get { session.isExporting }
        set { session.isExporting = newValue }
    }
    private(set) var exportStatus: String {
        get { session.exportStatus }
        set { session.exportStatus = newValue }
    }
    private(set) var isImporting: Bool {
        get { session.isImporting }
        set { session.isImporting = newValue }
    }
    private(set) var importStatus: String {
        get { session.importStatus }
        set { session.importStatus = newValue }
    }
    var errorMessage: String? {
        get { session.errorMessage }
        set { session.errorMessage = newValue }
    }

    private(set) var transaction: TransactionState {
        get { session.transaction }
        set { session.transaction = newValue }
    }

    /// Rows fetched per browse page. A grid shows a window onto the data;
    /// pulling a million rows to display forty is what makes other clients feel
    /// slow. `loadMore()` fetches the next page on request.
    private let browsePage = 100_000

    /// Maximum rows one browse result will retain.
    ///
    /// A bound on this window's memory rather than on what the cursor could
    /// still deliver: the grid keeps every row it was given so the user can
    /// scroll back to it, and a million of those is already past what anyone
    /// reads. It stops rather than evicting for the same reason — a row that
    /// scrolled off the top has to still be there when they scroll back up.
    private let browseResultBound = 1_000_000

    private var browseCursor: Cursor? {
        get { session.browseCursor }
        set { session.browseCursor = newValue }
    }
    private var browseStatementText: String {
        get { session.browseStatementText }
        set { session.browseStatementText = newValue }
    }
    private var exportCursor: Cursor? {
        get { session.exportCursor }
        set { session.exportCursor = newValue }
    }
    private var browseFetchInFlight: Bool {
        get { session.browseFetchInFlight }
        set { session.browseFetchInFlight = newValue }
    }

    private var emptyColumns: EmptyColumns {
        get { session.emptyColumns }
        set { session.emptyColumns = newValue }
    }

    /// Columns the browse grid should not draw.
    ///
    /// The decision, as against the evidence above. The column stays in the
    /// result either way — Copy and Export write what was fetched — which is the
    /// whole reason hiding one loses nothing. What it saves is a column of
    /// screen spent on a value that is never there.
    var hiddenBrowseColumns: Set<Int> {
        preferences.hidesEmptyColumns ? emptyColumns.columns : []
    }

    /// Rows a page fetches, for the chrome to name in a control.
    var pageSize: Int { browsePage }
    private let batchRows = 8192

    private var db: Database? {
        get { session.db }
        set { session.db = newValue }
    }
    private var queue: DispatchQueue { session.queue }
    /// Exports get a queue of their own. The core queue is serial because one
    /// connection cannot service two statements; an export holds no connection,
    /// and parking a million-row write in front of the next query would make
    /// clicking a table in the navigator wait on a file.
    ///
    /// One for the window rather than one per session, which is the opposite of
    /// the core queue above and for the same reason: what this protects is the
    /// next query from a file write parked in front of it, and that is as true
    /// across two connections as within one.
    private let exportQueue = DispatchQueue(label: "dev.dbclient.export", qos: .userInitiated)
    private var connString: String {
        get { session.connString }
        set { session.connString = newValue }
    }

    /// Which database the editor is writing SQL for, as the scheme the core
    /// picks a dialect by.
    ///
    /// Read off the live connection rather than the form's draft, because the
    /// draft is what somebody may be part way through typing into File ▸
    /// Connect… while the buffer behind it still belongs to the database that is
    /// open. Empty before there is one, which the core reads as PostgreSQL.
    var scheme: String { ConnectionURL.scheme(in: connString) }

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
    private var appliedInitialFilters: Bool {
        get { session.appliedInitialFilters }
        set { session.appliedInitialFilters = newValue }
    }

    /// Set by `--run-script`: the opening `--sql` is a whole script, and nothing
    /// here runs it. main.swift sends the Query menu's own item once the window
    /// has settled, which is what makes the capture a check of that item's
    /// wiring rather than only of the model behind it.
    private let initialSQLIsScript: Bool

    init(
        history: QueryHistory, favorites: QueryFavorites, preferences: Preferences,
        initialTab: DetailTab = .content, initialSQL: String? = nil,
        initialCaret: Int? = nil, initialSQLIsScript: Bool = false,
        initialWhere: String? = nil, initialOrder: String? = nil,
        initialStructureDetail: StructureDetail? = nil, initialRelation: String? = nil,
        initialFilter: String? = nil
    ) {
        self.history = history
        self.favorites = favorites
        self.preferences = preferences
        self.initialSQLIsScript = initialSQLIsScript
        self.initialStructureDetail = initialStructureDetail
        self.initialRelation = initialRelation
        self.initialSQL = initialSQL
        self.initialFilters = (initialWhere, initialOrder)
        // Onto the session directly, not through the forwarding properties, and
        // down here rather than at the top. Two reasons, and the second is the
        // one that would have been a defect rather than a compile error:
        // reaching a computed property needs every stored one to have a value
        // first, and `activeTab`'s setter records a visit — which the `didSet`
        // it replaced would not have done, because Swift does not run those
        // during initialisation. A window would have opened with a history
        // entry nobody navigated to.
        sessions[0].navigatorFilter = initialFilter ?? ""
        sessions[0].activeTab = initialSQL == nil ? initialTab : .query
        // After `preferences`, which is what says where to look for the file.
        //
        // The first saved connection is selected rather than left for the user to
        // pick, so that the commonest launch — one database, opened again — is the
        // return key and nothing else. It is a selection and not a connection: what
        // this application must never do is open one nobody named this time.
        connections = ConnectionList(ConnectionStore.load(from: preferences.connectionStorage))
        if let first = connections.connections.first {
            connectionDraft = first
            deferPassword(of: first.id)
        }
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

    // MARK: - Connection

    /// The connections the sidebar lists, as the file has them.
    ///
    /// Held rather than read on demand, because the window edits it: a list read
    /// back from disk on every draw would lose an entry the moment a save failed
    /// and say nothing about it.
    var connections = ConnectionList()

    /// What the sidebar's field is filtering the list by.
    var connectionFilter = ""

    /// What the form is showing.
    ///
    /// A whole `SavedConnection` rather than the fields alone: the form edits one
    /// connection, and that connection now has a name, a colour and — the part that
    /// matters — an identity. The identity is what says whether this draft is a row
    /// in the list (its id is in there) or Quick connect (it is not), which is one
    /// question with one answer rather than a selection kept beside the draft and
    /// able to disagree with it.
    ///
    /// An empty form until `init`, which is where the file is read. A property
    /// default cannot reach `preferences`, and where to look is a preference.
    var connectionDraft = AppModel.suggestedConnection()

    /// The form's password field. Nothing persists it from here: Save hands it to
    /// the Keychain, and nothing else keeps a copy.
    var connectionPassword = ""

    /// What the Keychain had for the connection being edited.
    ///
    /// Kept because the password is the one field `unsavedEdits` cannot compare for
    /// itself — it is not in the value, and it never leaves the Keychain — so this
    /// is the window's own answer to "is the one on screen the one that was saved".
    ///
    /// Empty while `deferredPassword` is set, because nothing has been read yet.
    private var savedConnectionPassword = ""

    /// The connection whose stored password has deliberately not been read.
    ///
    /// Reading a secret is what raises the system panel asking the user to
    /// authorise it, and an ad-hoc-signed build is asked again after every
    /// rebuild. Opening a window is not a reason to ask: the read waits until
    /// something needs the secret, which is connecting.
    ///
    /// Nil once the read has happened, and nil where nothing was stored — both
    /// mean `connectionPassword` can be believed as it stands.
    private var deferredPassword: UUID?

    /// Whether the form is showing a password it has not read.
    ///
    /// The field is empty in that state, and an empty password field otherwise
    /// says "none saved". This is what the placeholder needs in order not to say
    /// something untrue.
    var hasUnreadPassword: Bool { deferredPassword != nil }

    /// Quick connect's draft, while a saved connection is the one on screen.
    ///
    /// Somebody typing a one-off connection, who clicks a saved row to check a port
    /// and clicks back, has not asked to lose what they typed — and Quick connect is
    /// the one row with nothing on disk to restore it from.
    private var quickConnectDraft = AppModel.suggestedConnection()
    private var quickConnectPassword = ""

    /// A form with nothing in it yet: the first driver the core reports, with its
    /// own suggestion of a host and a port.
    static func suggestedConnection() -> SavedConnection {
        SavedConnection(
            settings: DriverCatalog.first.map(ConnectionSettings.suggested(for:))
                ?? ConnectionSettings(scheme: ""))
    }

    /// Whether the window is showing the connection form rather than a session.
    /// True at launch, because there is no database until one is named.
    private(set) var isPresentingConnection = true

    /// A connect attempt in flight. Distinct from `isBusy`, which describes the
    /// session's own queue: this is what routes a failure to the form instead
    /// of to the pane's banner, and a reconnect fails with the previous
    /// connection still open behind it.
    private(set) var isConnecting = false

    /// What the last attempt failed with, shown in the form. The pane's banner
    /// cannot carry it — while the form is up the pane is not on screen, and on
    /// a reconnect the pane behind it still describes a connection that works.
    var connectionError: String?

    /// Whether the form can be dismissed without connecting. Only once there is
    /// a session behind it to go back to.
    var canCancelConnection: Bool { db != nil }

    /// Whether there is a connection to close.
    ///
    /// Its own property rather than a second reader of `canCancelConnection`,
    /// which happens to test the same thing for an unrelated question — whether
    /// the form has a session to be dismissed back to. Two questions that agree
    /// today and have no reason to go on agreeing.
    var canDisconnect: Bool { db != nil }

    /// What the session waiting behind the chooser is connected to, or nil before
    /// there is one.
    ///
    /// Parsed back out of the string the connection was opened with rather than
    /// kept as a second copy of it: the two would go out of step on the first
    /// reconnect, and the one the chooser marks as open would be the previous
    /// database. The password is not part of what comes back, which is what makes
    /// this comparable to a row in the file — those hold no password either.
    var openConnectionSettings: ConnectionSettings? {
        db == nil ? nil : ConnectionSettings(connectionString: connString)
    }

    /// Whether Connect has something to try.
    var canConnect: Bool { connectionDraft.settings.isComplete && !isConnecting }

    /// What the last Test found, or nil before anybody has pressed it.
    ///
    /// Three states rather than a flag and a message, because "not tried" is not
    /// "failed": a form that opened showing a red line about a connection nobody
    /// has tested would be reporting its own state as the database's.
    enum ConnectionTest: Equatable {
        case running
        case reached(ServerInfo)
        case failed(String)
    }

    /// The last Test's answer. Cleared when the form moves to another connection:
    /// an answer about the row somebody just left is worse than no answer.
    private(set) var connectionTest: ConnectionTest?

    /// Whether Test has something to try — Connect's question, and one more: a
    /// test already in flight is not worth starting twice.
    var canTestConnection: Bool {
        connectionDraft.settings.isComplete && connectionTest != .running
    }

    /// Opens what the form describes, asks what answered, and lets it go.
    ///
    /// A connection of its own, and off the core queue rather than on it: that
    /// queue belongs to whatever database is already open, and a test that waited
    /// behind a running query would report a timeout against a server that is
    /// perfectly well. Nothing here touches the session — finding out before
    /// replacing anything is the whole point of a test.
    ///
    /// The failure lands in `connectionTest` rather than in `connectionError`:
    /// the banner is where a failed *connect* goes, and a test that failed has
    /// not yet stopped anybody from doing anything.
    func testConnection() {
        guard canTestConnection else { return }
        readDeferredPassword()
        let connString = connectionDraft.settings.connectionString(password: connectionPassword)
        connectionTest = .running
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let outcome: ConnectionTest
            do {
                outcome = .reached(try Database(connString: connString).serverInfo())
            } catch {
                outcome = .failed("\(error)")
            }
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.connectionTest = outcome
                    if case .reached(let info) = outcome { self?.record(server: info.label) }
                }
            }
        }
    }

    /// Keeps what answered against the connection it answered for.
    ///
    /// Written straight to the file rather than waiting for Save, because it is
    /// not an edit: nobody typed it, `unsavedEdits` does not compare it, and a
    /// record that needed saving would leave the row wrong until somebody pressed
    /// a button about something they had never done. Quick connect has nowhere to
    /// keep it, which is what the lookup below decides.
    private func record(server: String) {
        guard !server.isEmpty else { return }
        connectionDraft.server = server
        guard var saved = connections.connection(connectionDraft.id), saved.server != server
        else { return }
        saved.server = server
        connections.save(saved)
        ConnectionStore.save(connections.connections, to: preferences.connectionStorage)
    }

    /// The saved connection the form is editing, or nil for Quick connect.
    var editedConnection: SavedConnection? { connections.connection(connectionDraft.id) }

    /// Which row the sidebar draws as selected. Nil is Quick connect, which is a
    /// row like the others and is where a draft that is not in the file belongs.
    var selectedConnectionID: UUID? { editedConnection?.id }

    /// What Save would write and Revert would throw away, or nil when the form
    /// agrees with the file.
    ///
    /// Nil for Quick connect too, and not because nothing was typed: there is
    /// nothing saved for it to differ from, so leaving it loses a draft rather than
    /// an edit, and a question about that would be a question nobody can answer.
    var unsavedConnectionEdits: UnsavedConnectionEdits? {
        guard let saved = editedConnection else { return nil }
        return saved.unsavedEdits(
            against: connectionDraft,
            // A password that has not been read cannot have been edited, so
            // anything in the field is something just typed — and an empty field
            // is the state it was left in, not a change to nothing.
            passwordChanged: hasUnreadPassword
                ? !connectionPassword.isEmpty
                : connectionPassword != savedConnectionPassword
        )
    }

    /// Whether Save has anything to do. A draft nobody has typed into is not a
    /// connection, and a saved one nobody has edited is already saved.
    var canSaveConnection: Bool {
        guard ConnectionList.isWorthSaving(connectionDraft) else { return false }
        return editedConnection == nil || unsavedConnectionEdits != nil
    }

    /// Whether there is a saved connection to delete. Quick connect is a control
    /// rather than an entry, so there is never one to remove.
    var canDeleteConnection: Bool { editedConnection != nil }

    /// Whether the sidebar is showing fewer connections than there are.
    var isFilteringConnections: Bool {
        !connectionFilter.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// The connections the sidebar draws.
    var visibleConnections: [SavedConnection] { connections.matching(connectionFilter) }

    /// The same connections, in their folders, which is how the sidebar draws
    /// them. Beside `visibleConnections` rather than in place of it, because the
    /// count in the footer and the "no matches" state both ask how many there are
    /// and neither cares where they sit.
    var visibleConnectionGroups: [ConnectionList.Group] {
        connections.grouped(connectionFilter)
    }

    /// Asks before a connection and its password are forgotten.
    ///
    /// Injectable for the reason `confirmDeletion` is: the alert is a modal run
    /// loop, and the checks have nobody to click it.
    @ObservationIgnored
    var confirmConnectionDeletion: @MainActor (SavedConnection) -> Bool = { connection in
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Delete “\(connection.title)”?"
        alert.informativeText =
            "This removes the connection and its password from this Mac. It cannot be undone."
        alert.addButton(withTitle: "Delete")
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// Asks what to do with edits that would be lost by moving to another
    /// connection.
    ///
    /// Three answers rather than two, in the order macOS puts them: the one that
    /// keeps the work leads, and Cancel takes the escape key so that dismissing the
    /// sheet without reading it changes nothing. Injectable for the same reason as
    /// above.
    @ObservationIgnored
    var resolveUnsavedConnection: @MainActor (UnsavedConnectionEdits) -> UnsavedConnectionChoice = {
        edits in
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = edits.question
        alert.informativeText = edits.detail
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Discard Changes")
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        switch alert.runModal() {
        case .alertFirstButtonReturn: return .save
        case .alertSecondButtonReturn: return .discard
        default: return .cancel
        }
    }

    /// Shows another connection in the form, or Quick connect for nil.
    ///
    /// Unsaved edits are settled first, and a cancelled question leaves the
    /// selection where it was — which is why this asks rather than the row that was
    /// clicked: the sidebar would otherwise move its highlight to a connection the
    /// form is not showing.
    func selectConnection(_ id: UUID?) {
        guard id != selectedConnectionID else { return }
        guard settleUnsavedConnectionEdits() else { return }
        // The last test was about the connection being left. Left on screen it
        // would read as a statement about the one arriving.
        connectionTest = nil
        if editedConnection == nil {
            quickConnectDraft = connectionDraft
            quickConnectPassword = connectionPassword
        }
        guard let id, let saved = connections.connection(id) else {
            connectionDraft = quickConnectDraft
            connectionPassword = quickConnectPassword
            savedConnectionPassword = ""
            deferredPassword = nil
            return
        }
        connectionDraft = saved
        // Clicking a row is looking, not connecting. Somebody checking which port
        // they wrote down should not be made to authorise a Keychain read.
        deferPassword(of: id)
    }

    /// Arranges for a connection's password to be read when it is wanted.
    ///
    /// Asks whether there is one, which costs a Keychain lookup but no panel, so
    /// that the form can tell "no password saved" from "one saved, not read".
    private func deferPassword(of id: UUID) {
        connectionPassword = ""
        savedConnectionPassword = ""
        // An entry that declined storage is answered from memory instead, and
        // filled in rather than deferred: there is no panel to raise and nothing
        // to authorise, so making somebody press Connect before the field showed
        // what this process already knows would only make the form look emptier
        // than it is.
        if connections.connection(id)?.savesPassword == false {
            connectionPassword = SessionPasswords.password(for: id) ?? ""
            savedConnectionPassword = connectionPassword
            deferredPassword = nil
            return
        }
        switch preferences.passwordStorage {
        case .never:
            // Nothing is read and nothing is asked — not even whether an item
            // exists — so that a window opened by somebody who declined this
            // touches no secret of theirs in any way.
            deferredPassword = nil
        case .thisMac:
            // Filled in, for the same reason the session store above is: opening
            // the file raises no panel and authorises nothing, so there is
            // nothing to wait until Connect for.
            connectionPassword = CredentialFile.shared.password(for: id) ?? ""
            savedConnectionPassword = connectionPassword
            deferredPassword = nil
        case .keychain:
            // Deferred, because this is the answer that asks. Clicking a row is
            // looking, not connecting, and looking must not raise a panel.
            deferredPassword = ConnectionKeychain.hasPassword(for: id) ? id : nil
        }
    }

    /// Reads the deferred password, if there is one.
    ///
    /// This is the call that may raise the system's permission panel, so it has
    /// exactly one caller: the moment somebody asks to connect. Anything else
    /// calling it would put the panel back in front of a person who had not
    /// asked for anything needing a secret.
    private func readDeferredPassword() {
        // A field with something in it has already answered the question. Reading
        // now would raise the panel for a secret nobody needs, and then replace
        // what was just typed with what happened to be stored.
        guard connectionPassword.isEmpty, let id = deferredPassword else { return }
        // An empty field rather than an error when the Keychain refuses — see
        // `ConnectionKeychain` for when that happens and why it is survivable.
        savedConnectionPassword = ConnectionKeychain.password(for: id) ?? ""
        connectionPassword = savedConnectionPassword
        deferredPassword = nil
    }

    /// Empties Quick connect and shows it, for the sidebar's `+`.
    func newConnection() {
        guard settleUnsavedConnectionEdits() else { return }
        quickConnectDraft = Self.suggestedConnection()
        quickConnectPassword = ""
        connectionDraft = quickConnectDraft
        connectionPassword = ""
        savedConnectionPassword = ""
        deferredPassword = nil
    }

    /// Writes the form to the file, and its password to the Keychain.
    ///
    /// The only path by which anything reaches the file. Connecting does not save:
    /// a connection somebody tried once is not one they asked to keep, and a client
    /// that wrote every attempt down would fill the sidebar with typing mistakes.
    func saveConnection() {
        guard canSaveConnection else { return }
        let wasQuickConnect = editedConnection == nil
        connections.save(connectionDraft)
        ConnectionStore.save(connections.connections, to: preferences.connectionStorage)
        // Nothing reaches the Keychain while the setting is off. Its owner said
        // not to keep passwords, and Save is not the place to argue.
        //
        // A password left unread is left alone for a different reason: writing
        // the empty field over it would delete somebody's stored password
        // because they saved a change to the port, and
        // `ConnectionKeychain.save` treats empty as "store nothing", so the
        // deletion would be silent and total.
        if !connectionDraft.savesPassword {
            // Off means off for what is already stored, not only for what is
            // being typed now. Leaving the old copy behind would have the file
            // saying this password is not kept while a store still held it, and
            // the entry somebody turned the flag off *for* is exactly the one
            // with a password already in there. Both stores, because the setting
            // may have been the other one when it was written.
            ConnectionKeychain.delete(for: connectionDraft.id)
            CredentialFile.shared.delete(for: connectionDraft.id)
            SessionPasswords.remember(connectionPassword, for: connectionDraft.id)
            deferredPassword = nil
        } else {
            switch preferences.passwordStorage {
            case .never:
                break
            case .thisMac:
                // The Keychain copy goes, for the reason above: one password, one
                // place. An empty field deletes rather than stores, which is what
                // `CredentialFile.save` does with it — and unlike the Keychain
                // branch there is no unread state to protect, because this answer
                // fills the field in as soon as the row is clicked.
                ConnectionKeychain.delete(for: connectionDraft.id)
                CredentialFile.shared.save(connectionPassword, for: connectionDraft.id)
                deferredPassword = nil
            case .keychain:
                CredentialFile.shared.delete(for: connectionDraft.id)
                // A password left unread is left alone: writing the empty field
                // over it would delete somebody's stored password because they
                // saved a change to the port, and `ConnectionKeychain.save`
                // treats empty as "store nothing", so the deletion would be
                // silent and total.
                if !hasUnreadPassword || !connectionPassword.isEmpty {
                    ConnectionKeychain.save(connectionPassword, for: connectionDraft.id)
                }
                deferredPassword = nil
            }
        }
        // Assigned either way, including when nothing was written. Save has done
        // everything it is going to do, and leaving this behind would have the
        // form calling the typed password an unsaved edit for the rest of the
        // session. Where the password was left unread this is a no-op: both
        // sides are already empty.
        savedConnectionPassword = connectionPassword
        // The draft became a row and stays selected, so Quick connect goes back to
        // being empty rather than a second copy of what was just saved.
        if wasQuickConnect {
            quickConnectDraft = Self.suggestedConnection()
            quickConnectPassword = ""
        }
    }

    /// Puts the saved values back in the form.
    func revertConnection() {
        guard let saved = editedConnection else { return }
        connectionDraft = saved
        // Back to unread, rather than reading in order to restore. Revert undoes
        // what was typed, and what was typed over was an empty field.
        connectionPassword = hasUnreadPassword ? "" : savedConnectionPassword
    }

    /// Puts a dragged connection above another and writes the new order down.
    ///
    /// Written through immediately rather than left as an unsaved edit. The form
    /// holds one connection and this changes the list, so there would be no row
    /// to show the pencil against — and a reorder that vanished on quit would be
    /// a gesture that did not take.
    func moveConnection(_ dragged: UUID, above target: UUID) {
        guard connections.move(dragged, above: target) else { return }
        ConnectionStore.save(connections.connections, to: preferences.connectionStorage)
    }

    /// Forgets the connection the form is showing, once its owner says so.
    ///
    /// The password goes with it. One left behind belongs to an entry nothing will
    /// ever show, offer to change, or delete.
    func deleteConnection() {
        guard let saved = editedConnection, confirmConnectionDeletion(saved) else { return }
        connections.remove(saved.id)
        ConnectionStore.save(connections.connections, to: preferences.connectionStorage)
        ConnectionKeychain.delete(for: saved.id)
        CredentialFile.shared.delete(for: saved.id)
        SessionPasswords.forget(saved.id)
        connectionDraft = quickConnectDraft
        connectionPassword = quickConnectPassword
        savedConnectionPassword = ""
        deferredPassword = nil
    }

    /// Settles edits before the form shows something else. False when the person
    /// asked to stay where they are.
    private func settleUnsavedConnectionEdits() -> Bool {
        guard let edits = unsavedConnectionEdits else { return true }
        switch resolveUnsavedConnection(edits) {
        case .save:
            saveConnection()
            return true
        case .discard:
            return true
        case .cancel:
            return false
        }
    }

    /// Whether the launch options have been spent.
    ///
    /// They describe the launch, not every session: re-running `--sql` against
    /// a database the user switched to by hand would execute a statement they
    /// did not ask for, written for a schema it has never seen.
    private var appliedLaunchOptions = false

    /// Connects to what the form is showing.
    ///
    /// Whatever is on screen, saved or not: the fields are there to be connected
    /// with, and refusing to open an edited connection until it had been written
    /// down would make every experiment a commitment. The row keeps its unsaved
    /// mark, which is where that fact belongs.
    func connectFromForm() {
        guard canConnect else { return }
        // The one moment the secret is actually needed, and so the one moment
        // worth asking the user to authorise reading it.
        readDeferredPassword()
        // Held for the rest of this launch where the entry declined the Keychain.
        // Without this the flag's promise — typed once, kept until you quit —
        // would hold only for connections somebody also pressed Save on, and
        // nowhere else in this form does connecting mean saving.
        if !connectionDraft.savesPassword {
            SessionPasswords.remember(connectionPassword, for: connectionDraft.id)
        }
        open(connectionDraft.settings.connectionString(password: connectionPassword))
    }

    /// Connects to a raw connection URL, from `--conn`.
    ///
    /// Deliberately not saved. This is the automation path — the benchmarks and the
    /// screenshot captures run through it — and a run of `make screenshot` must not
    /// leave bench credentials in somebody's sidebar. It lands in Quick connect
    /// rather than over a saved connection for the same reason, and the form is
    /// seeded from it so that a string which does not connect can be corrected
    /// rather than retyped.
    func connect(using connString: String) {
        connectionDraft = SavedConnection(
            settings: ConnectionSettings(connectionString: connString))
        connectionPassword = ConnectionURL.password(in: connString) ?? ""
        savedConnectionPassword = ""
        deferredPassword = nil
        open(connString)
    }

    /// Opens the connection form over the session, for File ▸ Connect….
    ///
    /// Leaves the live connection alone: it is still the one answering queries
    /// until another one opens, so a mistyped password costs nothing but the
    /// retyping. What opens arrives in a tab of its own, and `adopt` is what puts
    /// that tab in front.
    func presentConnection() {
        connectionError = nil
        isPresentingConnection = true
    }

    /// Puts the session back, for the form's Cancel.
    func cancelConnection() {
        guard canCancelConnection else { return }
        isPresentingConnection = false
        connectionError = nil
        // A failed attempt left the dot red over a connection that is still
        // working. Nothing else would ever put it right.
        connectionState = .connected
    }

    /// The session an attempt should fill: the tab in front when nothing is open
    /// on it, and a new tab otherwise.
    ///
    /// Three behaviours out of one rule. A window's first connection fills the
    /// tab that is already there; a retry after a refusal reuses the tab the
    /// refusal left; File ▸ Connect… over a live connection opens a second tab
    /// beside it. The new tab is appended and not selected — `adopt` is what puts
    /// it in front, once there is something in it to show.
    private func sessionToFill() -> Session {
        if session.db == nil { return session }
        let added = Session()
        sessions.append(added)
        return added
    }

    private func open(_ connString: String) {
        let filling = sessionToFill()
        sessionBeingOpened = filling
        // Onto that session directly rather than through the forwarding
        // properties, which reach the tab in front — and the tab in front is the
        // connection still answering queries while this one is in the air.
        filling.connString = connString
        filling.connectionLabel = Self.label(for: connString)
        filling.connectionState = .connecting
        filling.status = "Connecting…"
        filling.isBusy = true
        isConnecting = true
        connectionError = nil
        dispatch(
            on: filling.queue, applyingInto: filling,
            {
                let db = try Database(connString: connString)
                return (db, try Self.inventory(of: db))
            },
            then: { [self] result in
                adopt(result.0, inventory: result.1)
            })
    }

    /// Installs a connection that opened, into the session that was opening it.
    ///
    /// Nothing is cleared on the way in. There used to be a `reset` here that
    /// emptied every pane, because a new connection arrived into the window that
    /// was showing the previous one; now it arrives into a session of its own,
    /// which has never held anything. The list of what to clear was the kind of
    /// list that goes wrong by omission — one property left off it is a fragment
    /// of the previous database shown under the name of this one — and it is
    /// gone, replaced by `Session`'s own stored properties.
    private func adopt(_ connection: Database, inventory: Inventory) {
        db = connection
        isConnecting = false
        isPresentingConnection = false
        schemas = inventory.schemas
        relations = inventory.relations
        connectionLabel = Self.label(for: connString)
        connectionColor = connectionDraft.color
        safety = ConnectionSafety(of: connectionDraft)
        record(server: inventory.server)
        capabilities = inventory.capabilities
        connectionState = .connected
        // Open the schema a user most likely wants, and land on a table
        // rather than an empty pane. Opening to nothing makes every session
        // start with the same two clicks. `--relation` overrides both, and
        // may name a schema of its own.
        let asked = appliedLaunchOptions ? nil : initialRelation
        let requested = asked.flatMap(findRelation)
        // A `--relation` that named nothing selects nothing, rather than falling
        // through to the first table of the default schema. A capture switch
        // that quietly substitutes its subject makes every screenshot and every
        // row count taken with it a claim about a table nobody asked for — and
        // the probes read an empty selection as the failure it is.
        let opening =
            requested.map(\.schema)
            ?? (asked == nil
                ? (schemas.first(where: { $0.name == "public" }) ?? schemas.first)?.name : nil)
        if let opening {
            expanded.insert(opening)
            selected = requested ?? relations[opening]?.first
        }
        status = settledStatus
        isBusy = false
        // Whether this database has a transaction to control is the first thing
        // the toolbar needs, and it is a property of the connection just made
        // rather than of anything the user has done yet.
        refreshTransaction()
        // Runs after the selection above, so an explicit `--sql` replaces
        // the browse rather than racing it. Through the same path ⌘R takes,
        // so a multi-statement `--sql` runs the one `--caret` names rather
        // than the whole buffer — unless `--run-script` says otherwise, and
        // then the menu item is what runs it.
        if !appliedLaunchOptions, initialSQL != nil, !initialSQLIsScript { runCurrentQuery() }
        appliedLaunchOptions = true
        // In front now, and not a moment sooner. Until this line the window went
        // on showing the connection that was already answering, which is the
        // promise `presentConnection` makes: a mistyped password costs the
        // retyping and nothing else.
        if let index = sessions.firstIndex(where: { $0 === session }) { activeSession = index }
        sessionBeingOpened = nil
    }

    /// Puts another of this window's connections in front.
    ///
    /// One assignment, because there is nothing to save and nothing to put back.
    /// Everything a pane reads is the session's, so moving the pointer moves the
    /// navigator, the grid, the editor, the transaction state and the status bar
    /// together — and a statement still running on the tab being left goes on
    /// running, and lands in it.
    func selectSession(_ index: Int) {
        guard sessions.indices.contains(index) else { return }
        activeSession = index
    }

    /// Closes a connection and takes its tab with it.
    ///
    /// A window always has a tab. Closing the only one leaves an empty session
    /// rather than a window with nothing in it — which is also what Disconnect
    /// is: there is no separate command, because a connection nobody is looking
    /// at and a closed connection are the same thing.
    func closeSession(_ index: Int) {
        guard sessions.indices.contains(index) else { return }
        let closing = sessions[index]
        sessions.remove(at: index)
        if sessions.isEmpty {
            sessions = [Session()]
            activeSession = 0
        } else if activeSession >= sessions.count {
            activeSession = sessions.count - 1
        } else if index < activeSession {
            activeSession -= 1
        }
        // An attempt still in the air belongs to a tab that no longer exists.
        // Forgetting it is what stops its refusal from being reported against
        // whatever tab has taken its place.
        if sessionBeingOpened === closing { sessionBeingOpened = nil }
        // A window showing nothing shows the chooser, which is what a window
        // with no connection is for. Without this, closing the last one leaves
        // the panes of a database that has gone, under a toolbar naming it.
        if session.db == nil { isPresentingConnection = true }
        drain(closing)
    }

    /// Lets go of a connection in the order the server needs it let go of.
    ///
    /// The cursors first. Each is a connection of its own that the server is
    /// holding open on this session's behalf, and freeing the session's handle
    /// would not touch them — a cursor left behind is a connection nothing will
    /// ever close.
    ///
    /// Then an open transaction is rolled back by name rather than left for the
    /// socket to decide. A server that is told releases its locks now; a server
    /// that is not may hold them until it notices the client has gone, and the
    /// rows are locked against everybody else in the meantime.
    ///
    /// All of it on that session's own queue and not on the main actor, so it
    /// queues behind whatever is still running there: a statement in flight
    /// finishes against a connection that is still open, rather than one pulled
    /// out from under it. The handle is freed when the closure returns, which is
    /// after the rollback rather than racing it.
    private func drain(_ closing: Session) {
        closing.browseCursor = nil
        closing.exportCursor = nil
        closing.browseFetchInFlight = false
        guard let db = closing.db else { return }
        let hadTransaction = closing.transaction.open
        closing.db = nil
        closing.queue.async {
            // Nowhere to report a failure to — the tab it would be reported in
            // is gone — and nothing to do about one either: the handle going out
            // of scope closes the socket, which rolls back whatever this could
            // not. The closure holding the last reference is what makes that
            // happen here, after the rollback, rather than on the main actor
            // while the rollback was still in the air.
            if hadTransaction { try? db.rollback() }
        }
    }

    /// The navigator's whole contents, read in one pass.
    ///
    /// Shared by `open` and `refresh` so the two cannot drift into loading
    /// different things: a refresh that read less than the connection did would
    /// quietly delete objects from a tree that is supposed to have become more
    /// accurate, not less.
    private nonisolated static func inventory(of db: Database) throws -> Inventory {
        let schemas = try db.schemas()
        var relations: [String: [RelationInfo]] = [:]
        for schema in schemas {
            relations[schema.name] = try db.relations(schema: schema.name)
        }
        // Asked here, where a failure costs the label and not the connection. A
        // database that will not say what it is is still a database somebody has
        // just opened, and refusing to show it over a version string would be
        // this application deciding the answer mattered more than the data.
        return Inventory(
            schemas: schemas, relations: relations,
            server: (try? db.serverInfo())?.label ?? "",
            // Read here rather than on the main actor for the reason everything
            // else in this function is: it crosses the FFI boundary, and the
            // window is not the place to wait for anything that does. It costs no
            // round trip, so it rides along with the metadata instead of being a
            // second trip of its own.
            capabilities: (try? db.capabilities()) ?? .unknown)
    }

    private struct Inventory: Sendable {
        let schemas: [SchemaInfo]
        let relations: [String: [RelationInfo]]
        /// What answered, for the list row to keep. Empty where it would not say.
        let server: String
        /// What this connection can do, for the controls that would otherwise
        /// have to find out by being refused.
        let capabilities: Capabilities
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
            // Refresh means the database changed under us. The names the editor
            // completes from were learned before that, so they go with the tree
            // — one button, or the navigator and the editor disagree about which
            // tables exist.
            db.forgetNames()
            return try Self.inventory(of: db)
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
            discardBrowse()
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
        status = settledStatus
        isBusy = false
        browseResult.abandonLoading()
    }

    /// Resolves `--relation`, which is either a bare name or `schema.name`.
    /// Unqualified searches every schema so a capture does not have to know
    /// where a table lives, but prefers the one that opens by default.
    ///
    /// The qualified form cannot be split at a dot, in either direction. A
    /// schema name may contain one — DuckDB's is `database.schema` and Trino's
    /// is `catalog.schema` — so the first dot is wrong, and a relation name may
    /// contain one too, so the last dot is wrong as well. The schemas that
    /// actually exist are what settles it, which is a lookup this side has and
    /// a parser would only be guessing at.
    private func findRelation(named requested: String) -> RelationInfo? {
        for schema in relations.keys where requested.hasPrefix("\(schema).") {
            let name = String(requested.dropFirst(schema.count + 1))
            if let found = relations[schema]?.first(where: { $0.name == name }) { return found }
        }
        if requested.contains(".") { return nil }
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

    // MARK: - Back and forward

    private var browseHistory: BrowseHistory {
        get { session.browseHistory }
        set { session.browseHistory = newValue }
    }

    private var isNavigatingHistory: Bool {
        get { session.isNavigatingHistory }
        set { session.isNavigatingHistory = newValue }
    }

    var canGoBack: Bool { browseHistory.canGoBack }
    var canGoForward: Bool { browseHistory.canGoForward }

    func goBack() { travel(to: browseHistory.goBack()) }
    func goForward() { travel(to: browseHistory.goForward()) }

    /// Notes where the window now is, unless Back or Forward put it there.
    ///
    /// Called from two places, because there are two ways to move: picking
    /// another relation, and switching tab. `BrowseHistory.visit` ignores an
    /// arrival at the place already current, which is what lets both call it
    /// without either having to know whether the other just did.
    private func recordVisit() {
        guard !isNavigatingHistory, let selected else { return }
        browseHistory.visit(Visit(relationID: selected.id, tab: activeTab))
    }

    /// Goes where the history says, if that place is still on the tree.
    ///
    /// A visit naming a relation the sidebar no longer has is dropped rather
    /// than reported: the path is a record of where this window went, and a table
    /// dropped by somebody else is not an error this window made. The cursor has
    /// already moved by then, so pressing Back again steps past it.
    ///
    /// `findRelation(named:)` is the lookup the `--relation` flag uses, and a
    /// `Visit`'s id is spelled the way that flag spells one.
    private func travel(to visit: Visit?) {
        guard let visit, let relation = findRelation(named: visit.relationID) else { return }
        isNavigatingHistory = true
        defer { isNavigatingHistory = false }
        activeTab = visit.tab
        selected = relation
    }

    // MARK: - Go to

    /// Whether the go-to palette is on screen.
    var isGoToOpen = false

    /// Whether there is anything to go to. Drives the menu item's enabled state,
    /// so the command is not offered before the tree has been read.
    ///
    /// Asks whether any schema holds something rather than whether any schema
    /// was found: a database of empty schemas reads as answered here but opens a
    /// palette with nothing in it.
    var canGoTo: Bool {
        relations.values.contains { !$0.isEmpty } || !offeredFavorites.isEmpty
    }

    /// Every relation this window has read, as the palette's targets.
    ///
    /// Built from the inventory already here rather than asked of the server:
    /// the palette is typed into at speed, and anything else would be a round
    /// trip per keystroke.
    ///
    /// The saved queries join them, unqualified: a favorite belongs to the
    /// person rather than to a schema, so there is nothing to put in front of
    /// its name — and the ones this connection is not offered are left out
    /// here for the same reason the panel leaves them out.
    var goToTargets: [GoToTarget] {
        relations.values.flatMap { $0 }.map { GoToTarget(schema: $0.schema, name: $0.name) }
            + offeredFavorites.map {
                GoToTarget(schema: "", name: $0.name, kind: .favorite, sql: $0.sql)
            }
    }

    /// Opens the relation the palette chose and shows its rows.
    ///
    /// Looked up here rather than carried by the target, because `GoToTarget` is
    /// deliberately two strings: the matching rule is checked without a database,
    /// and a `RelationInfo` inside it would drag the metadata layer into a rule
    /// that has no business knowing about one.
    ///
    /// The palette closes even where the lookup fails. It was opened over a list
    /// this window had already read, so a name in it that no longer resolves
    /// means the tree moved underneath — and a palette that stayed open would be
    /// offering the same stale row again.
    func goTo(_ target: GoToTarget) {
        isGoToOpen = false
        // Through the one insertion path, which is what the history list and
        // the favorites tab already use: appended to the buffer and selected,
        // ready for the ⌘R after it. A second way in would be a second answer
        // to where the caret ends up.
        guard target.kind == .relation else {
            insertIntoEditor(target.sql)
            return
        }
        guard let relation = relations[target.schema]?.first(where: { $0.name == target.name })
        else { return }
        activeTab = .content
        selected = relation
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

    var isValueViewerOpen: Bool {
        get { session.isValueViewerOpen }
        set { session.isValueViewerOpen = newValue }
    }

    /// Opens or closes that pane, and ends any edit as it closes.
    ///
    /// One method rather than a `toggle()` at each of the three call sites,
    /// because the two flags have to move together. A pane closed while the box
    /// was open would come back as a box the next time it was opened — over
    /// whatever cell was selected by then, seeded from that one's value, with
    /// nothing on screen to say an edit had been resumed rather than started.
    func toggleValueViewer() {
        isValueViewerOpen.toggle()
        if !isValueViewerOpen { isEditingValue = false }
    }

    /// Opens the value pane over the selected cell, as a box where that cell has
    /// a value the box can hold.
    ///
    /// The grid's context menu calls this, and it cannot be what the pencil does:
    /// that button is only drawn once the pane is open, which is the right rule
    /// for a control living in the pane's own header and the wrong one for a
    /// right-click, which is how somebody with nothing open asks to change the
    /// cell under the pointer. So this opens both, in that order.
    ///
    /// A value the box is refused — a blob, or one too long to lay out — still
    /// opens the pane, and deliberately. The alternative was a menu item that did
    /// nothing for those cells, and the pane is where the reason already is: the
    /// descriptor beside the value says "hex dump · 200 bytes", and the pencil
    /// beside it is disabled carrying the sentence. Opening on a refusal answers
    /// the question that was asked; doing nothing would not.
    ///
    /// `isValueViewerOpen` is set first because `editedValue` is read through the
    /// inspected cell, and that cell is built from this flag.
    func editSelectedValue() {
        isValueViewerOpen = true
        isEditingValue = editedValue?.isEditable == true
    }

    // MARK: - The record view

    /// Whether the Content pane is listing one row instead of drawing the grid.
    ///
    /// Window state, like the value viewer's and for a stronger reason: somebody
    /// who reads a sixty-column table this way reads every table this way, and
    /// being put back into the grid on each selection would be the application
    /// arguing with them about it.
    var isRecordViewOpen = false

    /// Whether there is a row for the record view to lay out.
    ///
    /// The Content tab only: the Query pane's rows belong to no one relation, so
    /// a field listed there could not be written back through anything.
    var canShowRecord: Bool {
        activeTab == .content && selected != nil && browseRowCount > 0
    }

    /// Which row the record view is on, counted from one, and how many there are.
    /// Nil where there is no row — which is what keeps the header from reading
    /// "Row 1 of 0".
    var recordPosition: (row: Int, of: Int)? {
        guard canShowRecord,
            let row = Record.row(browseSelection?.row ?? 0, steppedBy: 0, rowCount: browseRowCount)
        else { return nil }
        return (row + 1, browseRowCount)
    }

    /// The selected row's columns, in the order the grid draws them.
    ///
    /// Read from the same place the grid reads: the row is the cursor's, the
    /// hidden columns are the grid's, and each value comes through `cell(at:in:)`
    /// — so this is the row on screen described differently, not a second
    /// reading of the result that could drift from it.
    var recordFields: [RecordField] {
        guard canShowRecord,
            let row = Record.row(browseSelection?.row ?? 0, steppedBy: 0, rowCount: browseRowCount)
        else { return [] }
        let grid = browseResult.table
        return Record.fields(count: grid.columns.count, hidden: hiddenBrowseColumns) { column in
            guard let cell = cell(at: GridSelection(row: row, column: column), in: browseResult)
            else { return nil }
            return RecordField(
                column: column, name: cell.column, type: cell.type, value: cell.value,
                isNull: cell.isNull)
        }
    }

    /// Moves to another row, and takes the grid's cursor with it.
    ///
    /// There is one cursor in this window. The record view is another way of
    /// drawing the selection rather than a second selection — which is what
    /// keeps Save, the inspector strip and the value field all talking about the
    /// row the list was showing once it is closed again.
    ///
    /// The anchor is dropped, because a range that was extended across rows in
    /// the grid has no meaning in a view that shows one row at a time.
    func stepRecord(by delta: Int) {
        guard canShowRecord,
            let row = Record.row(
                browseSelection?.row ?? 0, steppedBy: delta, rowCount: browseRowCount)
        else { return }
        var selection = browseSelection ?? GridSelection(row: row, column: 0)
        selection.row = row
        selection.anchor = nil
        browseSelection = selection
    }

    /// Puts the cursor on a field, which is what the value editor writes through.
    ///
    /// Column 0 rather than the field's own column where there is no selection
    /// yet is not a case worth writing: a list with a field in it has a row, and
    /// a row means a cursor.
    func focusRecordField(_ column: Int) {
        guard canShowRecord, var selection = browseSelection else { return }
        selection.column = column
        selection.anchor = nil
        browseSelection = selection
    }

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
        let rows = result === browseResult ? browseRowCount : result.table.rowCount
        // The browse's cursor is read through the same clamp the grid reads it
        // through, so the cell described here is the cell outlined there.
        guard let s = result === browseResult ? browseSelection : result.selection,
            s.column < result.table.columns.count,
            s.row < rows
        else { return nil }
        return s
    }

    func inspectedCell(in result: ResultSet) -> InspectedCell? {
        guard let s = selectedCell(in: result) else { return nil }
        return cell(at: s, in: result)
    }

    /// What a field opened over the selected browse cell should start with.
    ///
    /// The rule `CellEditorRow.seed` states, for the reason stated there: a NULL
    /// seeds an empty field rather than the word, because what the field holds
    /// is what would be written and "NULL" typed into a text column is four
    /// characters. Which also means the inline editor cannot produce a NULL —
    /// the button under the grid is where that lives, and it is the reason that
    /// row stays.
    var inlineEditSeed: String {
        guard let cell = inspectedCell(in: browseResult), !cell.isNull else { return "" }
        return cell.value
    }

    /// The same, for a cell named rather than selected.
    ///
    /// Split out for the record view, which describes every column of one row
    /// where the strip describes one cell of it. Both arrive here, so a value
    /// read down the page and the same value read across the grid cannot end up
    /// disagreeing about NULL, about a binary blob, or about a row that has not
    /// been sent to the server yet.
    private func cell(at s: GridSelection, in result: ResultSet) -> InspectedCell? {
        let grid = result.table
        if result === browseResult, isDraft(row: s.row) { return draftCell(at: s) }
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
            toggleExpanded: { [weak self] in self?.toggleValueViewer() })
    }

    /// A cell of a row that is not in the database yet.
    ///
    /// Everything here comes from what was typed and from the catalogue, because
    /// there is no Arrow buffer behind it: the type is the column's declared one
    /// rather than an Arrow kind, and the value is a plain string — a draft has
    /// no bytes for the viewer to render as JSON or hex, and will not until the
    /// row has been sent and read back.
    private func draftCell(at s: GridSelection) -> InspectedCell {
        let name = browseResult.table.columns[s.column].name
        let held = staged.drafts[s.row - firstDraftRow].values[s.column]
        return InspectedCell(
            column: name,
            type: columns.first { $0.name == name }?.dataType ?? "",
            // Three states and two of them are not values: nothing typed leaves
            // the column out of the INSERT altogether, which is not the same as
            // typing NULL into it, and the strip has to be able to say which.
            value: held.map { $0.text ?? "NULL" } ?? "DEFAULT",
            // Both of those seed an empty field rather than the word they are
            // drawn as, which is what `isNull` is read for.
            isNull: held?.text == nil,
            address: "new row \(Self.formatted(s.row - firstDraftRow + 1))",
            rendering: .text,
            isExpanded: isValueViewerOpen,
            toggleExpanded: { [weak self] in self?.toggleValueViewer() })
    }

    // MARK: - Editing the browse result

    private(set) var staged: StagedChanges {
        get { session.staged }
        set { session.staged = newValue }
    }

    /// The cells the grid should mark as changed.
    var pendingCells: Set<GridCell> { Set(staged.updates.keys) }

    /// The rows the grid should mark as going.
    var deletedRows: Set<Int> { staged.deletes }

    /// The rows the grid should draw after the last fetched one.
    var draftRows: [DraftRow] { staged.drafts }

    /// Where the drafts start, which is one past the last row the database sent.
    private var firstDraftRow: Int { browseResult.table.rowCount }

    /// Whether a row of the browse is a draft rather than something read.
    private func isDraft(row: Int) -> Bool { row >= firstDraftRow }

    /// Whether the selected cell can be changed at all.
    ///
    /// Editing is the Content tab's alone. A query result is not attributable to
    /// one relation — a join of five tables has no answer to which of them a cell
    /// belongs to that is right often enough to write into a database — and a
    /// view has no rows of its own to name. The key is the other half of the same
    /// question: without something that names one row there is no way to say
    /// which row a change is to, so the core refuses and this does not offer.
    ///
    /// The connection's own read-only mark leads the list, because it is the one
    /// reason here that is not a discovery about the data but a decision the user
    /// already made.
    var canEditCell: Bool {
        safety.writeRefusal == nil && activeTab == .content && selected?.kind == .table && !isBusy
            && hasRowIdentity && selectedCell(in: browseResult) != nil
    }

    /// Whether the core found something that names one row of the selection.
    ///
    /// Nil while the answer is still being read, which is not the same as "no":
    /// treating it as no would show the refusal sentence for a table that turns
    /// out to be perfectly editable, for as long as the catalog takes to answer.
    private var hasRowIdentity: Bool { !(rowIdentity?.columns.isEmpty ?? true) }

    /// Why the selected cell cannot be changed, for a pane that has to say so.
    ///
    /// A control that is simply absent reads as a feature this build does not
    /// have. The three reasons a user can act on — the connection's read-only
    /// mark, the wrong tab, and a table with nothing to identify a row by — are
    /// worth a sentence each, and the last of those is the core's own: it is the
    /// one place that knows whether a unique constraint was turned down and which.
    ///
    /// The mark leads because it is the answer for every tab and every relation.
    /// A window that said "editing is for a browsed table" about a connection
    /// somebody had marked read-only would send them off to browse one and find
    /// the grid exactly as locked.
    var editObstacle: String? {
        if let refusal = safety.writeRefusal { return refusal }
        guard activeTab == .content, let selected else { return "Editing is for a browsed table." }
        guard selected.kind == .table else {
            return "A \(selected.kind.label.lowercased()) has no rows of its own to change."
        }
        guard let rowIdentity, rowIdentity.columns.isEmpty else { return nil }
        return rowIdentity.obstacle.map { "\($0)." }
    }

    var hasPendingEdits: Bool { !staged.isEmpty }

    /// Whether a cell is holding a change that has not been sent.
    func isPending(row: Int, column: Int) -> Bool {
        staged.updates[GridCell(row: row, column: column)] != nil
    }

    /// What the button that removes rows should say, or nil where there is no
    /// row to act on.
    ///
    /// It says both what pressing it does and what state the selection is in: a
    /// marked row is undone by the same button, and a control that read "Delete"
    /// over a row already crossed out would be offering to do it twice. A draft
    /// discards rather than deletes, because there is nothing in the database to
    /// delete and nothing to undo afterwards.
    var deleteRowsTitle: String? {
        guard canEditCell, let rows = selectedRows else { return nil }
        let noun = rows.count > 1 ? "\(Self.formatted(rows.count)) Rows" : "Row"
        if isDraft(row: rows.lowerBound) { return "Discard \(noun)" }
        let marked = rows.allSatisfy { staged.deletes.contains($0) }
        return marked ? "Keep \(noun)" : "Delete \(noun)"
    }

    /// Marks the selected rows for deletion, unmarks them if they are already
    /// marked, or drops them if they were never in the database.
    ///
    /// Marked rather than deleted: nothing has been sent, and the row stays on
    /// screen crossed out until Save or Revert says which way it goes. That is
    /// the same bargain the cell editor makes, and for the same reason — a
    /// person deleting rows from a grid picks several and then decides.
    func toggleDeleteSelectedRows() {
        guard canEditCell, let rows = selectedRows else { return }
        if isDraft(row: rows.lowerBound) {
            // Removed outright, so the ones below shift up: a draft is
            // identified by its place in the list, and marking one would leave
            // the grid drawing a row that no statement is going to be made from.
            let first = rows.lowerBound - firstDraftRow
            staged.drafts.removeSubrange(first...(rows.upperBound - firstDraftRow))
            // The cursor lands on whatever took the discarded row's place, or on
            // the last row left. A selection pointing past the end draws nothing
            // and takes the editor strip with it.
            browseResult.selection =
                browseRowCount > 0
                ? GridSelection(row: min(rows.lowerBound, browseRowCount - 1), column: 0) : nil
            return
        }
        if rows.allSatisfy({ staged.deletes.contains($0) }) {
            staged.deletes.subtract(rows)
        } else {
            staged.deletes.formUnion(rows)
        }
    }

    /// Adds a row after the last one, with every column at whatever the table
    /// says it defaults to until something is typed into it.
    ///
    /// Selected as it is added, on the first column, because the next thing the
    /// user does is fill it in and a new row nobody is standing on is a row they
    /// have to go and find.
    func addDraftRow() {
        guard canAddRow else { return }
        staged.drafts.append(DraftRow())
        browseResult.selection = GridSelection(row: browseRowCount - 1, column: 0)
    }

    /// Whether there is a row to copy, which is `canEditCell`'s questions plus
    /// one: a draft is refused. There is nothing in the database behind it and
    /// no key of its own to leave out, so the copy would be a second row of the
    /// same nothing — and the row it was made from is one Add Row away.
    var canDuplicateRow: Bool {
        guard canEditCell, let s = selectedCell(in: browseResult) else { return false }
        return !isDraft(row: s.row)
    }

    /// Adds a row holding what the selected one holds, minus the columns that
    /// name it.
    ///
    /// The key is left out rather than copied, because copying it asks the
    /// database for a second row with a key it already has — which is the one
    /// way this insert is certain to fail. Left out, the table's default
    /// supplies a fresh one, which is what "another one like this" means.
    ///
    /// The row it copies is the row on screen and not the row that was read: a
    /// cell edited a moment ago is part of what the user is looking at and is
    /// about to send, and a copy that quietly reverted it would differ from its
    /// original in a way nothing on screen explains.
    func duplicateSelectedRow() {
        guard canDuplicateRow, let s = selectedCell(in: browseResult) else { return }
        staged.drafts.append(
            staged.draft(
                copying: s.row, from: browseResult.table,
                clearing: rowIdentity?.columns ?? []))
        browseResult.selection = GridSelection(row: browseRowCount - 1, column: 0)
    }

    /// Whether a row can be added at all.
    ///
    /// `canEditCell`'s questions minus the cell: a table with no rows in it is
    /// exactly where adding one is worth offering, and there is nothing selected
    /// there to ask about. The columns are the one extra thing needed — a row is
    /// added by typing into it, and a result with no columns has nowhere to
    /// type.
    var canAddRow: Bool {
        activeTab == .content && selected?.kind == .table && !isBusy && hasRowIdentity
            && !browseResult.table.columns.isEmpty
    }

    /// The rows the browse grid draws: what the database sent, plus the drafts
    /// waiting under them.
    var browseRowCount: Int { browseResult.table.rowCount + staged.drafts.count }

    /// The browse rows the selection covers, bounds-checked against what is
    /// drawn, and never straddling the join between the two kinds — a span of
    /// fetched rows and a span of drafts are acted on differently, and one
    /// command cannot be both.
    private var selectedRows: ClosedRange<Int>? {
        guard let s = browseResult.selection, s.rows.upperBound < browseRowCount,
            isDraft(row: s.rows.lowerBound) == isDraft(row: s.rows.upperBound)
        else { return nil }
        return s.rows
    }

    /// Records a change to the selected cell. `nil` is NULL.
    ///
    /// Recorded rather than sent: a person editing a grid makes several changes
    /// and then decides, and a client that sent each keystroke would be one they
    /// could not change their mind in.
    func stageEdit(_ value: String?) {
        guard canEditCell, let s = selectedCell(in: browseResult) else { return }
        if isDraft(row: s.row) {
            // No "back to what it held" here: a draft holds nothing until it is
            // typed into, and clearing a cell is done with the NULL button —
            // which is a value, not the absence of one.
            staged.drafts[s.row - firstDraftRow].values[s.column] = PendingValue(text: value)
            return
        }
        let cell = GridCell(row: s.row, column: s.column)
        // Typing a cell back to what it already held is not a change. Keeping it
        // would put an UPDATE on the wire that says nothing and a dirty mark on
        // screen that cannot be cleared by undoing the edit.
        let grid = browseResult.table
        let before: String? =
            grid.isNull(row: s.row, column: s.column)
            ? nil : grid.text(row: s.row, column: s.column)
        if before == value {
            staged.updates.removeValue(forKey: cell)
        } else {
            staged.updates[cell] = PendingValue(text: value)
        }
    }

    /// Whether the value pane is a box being typed into rather than a value
    /// being read.
    ///
    /// On the model, where a mode nothing outside one view can start does not
    /// obviously belong. It is here because otherwise it cannot be photographed:
    /// every rendering defect this window has had was caught by a screenshot,
    /// `--cell` exists so that the reading pane is capturable at all, and a
    /// `TextEditor` over a dark theme is exactly the control that comes out as a
    /// white slab. A mode reachable only by a click is a mode no capture can
    /// reach — synthetic events need accessibility permission this environment
    /// does not grant.
    var isEditingValue: Bool {
        get { session.isEditingValue }
        set { session.isEditingValue = newValue }
    }

    /// Why the selected cell cannot be edited in the value pane, or nil where it
    /// can.
    ///
    /// `editObstacle` answers for the relation — the wrong tab, a view, a table
    /// with nothing that names a row — and is the sentence the editor row under
    /// the grid already shows. What it does not cover is the run in progress:
    /// `canEditCell` refuses then, and `stageEdit` refuses with it, so a box
    /// offered during a statement would take a change and silently drop it.
    private var valueEditObstacle: String? {
        if let obstacle = editObstacle { return obstacle }
        return canEditCell ? nil : "Wait for the statement that is running to finish."
    }

    /// What the value pane should offer for the selected browse cell, or nil
    /// where there is no cell to offer anything for.
    ///
    /// The browse result only. The Query pane draws the same inspector and gives
    /// it no model at all — see `CellInspector.editing` for why that is the
    /// parameter which decides it.
    var editedValue: ValueEdit? {
        inspectedCell(in: browseResult).map {
            ValueEdit.offered(for: $0, obstacle: valueEditObstacle)
        }
    }

    /// Records what was typed into the value pane.
    ///
    /// Through `stageEdit`, so this is a bigger field and not a second way of
    /// writing to the database: the same three-state handling of a draft, the
    /// same "typed back to what it held is not a change", the same batch. What
    /// this adds is the guard — the pane can be showing a box for a cell that
    /// stopped being editable while it was open, and a refused value must not
    /// reach the staging path by a route the strip's own buttons do not have.
    func stageEditedValue(_ text: String) {
        guard case .editable = editedValue else { return }
        stageEdit(text)
    }

    /// Throws the pending changes away. The rows on screen are already the
    /// database's, so nothing has to be re-read to undo them.
    func revertEdits() {
        staged = StagedChanges()
    }

    /// Sends the pending changes and re-reads the rows they touched.
    ///
    /// The re-read is not a formality. A trigger, a default or a check
    /// constraint can make the stored row differ from what was typed, and a grid
    /// that went on showing the typed value would be showing something that is
    /// not in the database.
    func applyEdits() {
        guard hasPendingEdits, !isBusy, let selected else { return }
        if let refusal = staged.refusal(sendingRowOfDefaults: preferences.insertsRowOfDefaults) {
            errorMessage = refusal
            return
        }
        // Asked before the request is built, so a user who says no has not paid
        // for anything, and after the refusal above, so the two cannot both
        // interrupt one press.
        if let confirmation = staged.confirmation(
            askingBeforeDeleting: preferences.confirmsDeletions),
            !confirmDeletion(confirmation)
        {
            return
        }
        guard let request = editRequest(for: selected) else { return }
        isBusy = true
        status = "Saving…"
        errorMessage = nil
        let batchRows = self.batchRows
        run { db -> [WrittenStatement] in
            let statements = try db.editStatements(request)
            var sent: [WrittenStatement] = []
            for sql in statements {
                let query = try db.query(sql, batchRows: batchRows)
                // Drained, because a statement that violates a constraint fails
                // when the server executes it rather than when it accepts it.
                while try query.nextBatch() != nil {}
                sent.append(WrittenStatement(sql: sql, affected: query.rowsAffected ?? 0))
            }
            return sent
        } then: { [self] sent in
            // One entry per statement rather than one per Save. What brings
            // somebody to this list is the UPDATE, not the fact that a button
            // was pressed — and a Save of four changes is four statements the
            // server saw separately.
            //
            // Each carries a zero duration, which is what the entry's own note
            // says a zero means: nothing measured it. Timing the loop would
            // produce one number for the batch, and hanging that on all four
            // would be four wrong durations in place of four honest blanks.
            //
            // Only what was sent. A Save that fails part way never reaches here,
            // so the statements before the failure go unrecorded — the banner is
            // where that answer lives today, and a history that claimed a run it
            // could not describe would be worse than one that is silent.
            for statement in sent {
                history.record(
                    statement.sql, from: .edit, outcome: .affected(statement.affected),
                    milliseconds: 0)
            }
            staged = StagedChanges()
            isBusy = false
            status = Self.pluralized(sent.count, "statement") + " sent"
            // Whatever the transaction is doing now, a write is what moved it.
            refreshTransaction()
            runBrowse()
        }
    }

    /// One statement a Save sent, and what the server said it touched.
    ///
    /// Carried back out of the connection's thread rather than recorded where it
    /// was sent: `QueryHistory` is main-actor and the loop that sends these is
    /// not. `rowsAffected` being nil is read as none rather than as unknown — an
    /// UPDATE that matched nothing and an UPDATE whose driver returned no tag
    /// both changed no row this side can name, and inventing a distinction the
    /// list has no way to draw would be worse than the one it can.
    private struct WrittenStatement: Sendable {
        let sql: String
        let affected: Int
    }

    /// Puts the deletion question, and answers whether Save may go on.
    ///
    /// A modal alert rather than the inline banner every failure here uses. The
    /// banner is right for reporting something that has already happened and
    /// wrong for asking something: this has to be answered before the statements
    /// go, and a strip the user can ignore is not a question. It is the only
    /// dialog in this application, which is most of what keeps it from being
    /// clicked through without reading.
    ///
    /// A property rather than a method because it is the one thing here a script
    /// has to be able to answer: `--preferences` presses Save with the setting
    /// both ways, and there is nobody at the keyboard to click a sheet.
    @ObservationIgnored
    var confirmDeletion: @MainActor (DeleteConfirmation) -> Bool = { confirmation in
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = confirmation.question
        alert.informativeText = confirmation.detail
        // Delete leads because it is what pressing Save asked for; Cancel takes
        // the escape key, so dismissing the sheet without reading it sends
        // nothing rather than everything.
        alert.addButton(withTitle: "Delete")
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// The pending changes as one request, or nil where a row cannot be named.
    ///
    /// The rules are `StagedChanges`'s, so that they can be checked without a
    /// database; what this supplies is the two things only a connected window
    /// knows — which relation is being browsed, and which of its columns the
    /// catalogue says identify a row.
    private func editRequest(for relation: RelationInfo) -> EditRequest? {
        staged.request(
            schema: relation.schema, relation: relation.name,
            keyColumns: rowIdentity?.columns ?? [],
            rows: browseResult.table)
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
        // What the table being left was showing, saved before the fields below
        // are overwritten with the new one's. This is the last moment they still
        // mean the previous table.
        if let previous {
            browseStore.save(
                BrowseState(
                    whereClause: whereClause, orderClause: orderClause,
                    rules: filterRules, compiledClause: compiledClause,
                    selection: browseResult.selection),
                for: previous.id)
        }
        // These fields used to be cleared here, because a filter naming the
        // previous table's columns cannot be assumed to run against this one.
        // That is still true of a table being opened for the first time, and a
        // fresh state is what the store answers for one. A table being returned
        // to is the case that changes: it had these filters, and re-typing them
        // was the cost of every A→B→A comparison.
        // The first selection is the exception: it is where --where/--order land.
        let restored = browseStore.state(for: selected.id)
        whereClause = appliedInitialFilters ? restored.whereClause : (initialFilters.where ?? "")
        orderClause = appliedInitialFilters ? restored.orderClause : (initialFilters.order ?? "")
        // Both halves or neither. `--where` is the first selection's exception
        // for the field only: it is a WHERE somebody typed on the command line,
        // and there are no rows that compiled to it.
        filterRules = appliedInitialFilters ? restored.rules : []
        compiledClause = appliedInitialFilters ? restored.compiledClause : ""
        appliedInitialFilters = true
        stateToRestore = restored
        recordVisit()
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
        //
        // The core writes it, for the reason `browseAsk` gives: this side wrote
        // PostgreSQL for every database, and the seed for a MySQL table was a
        // statement that could not run. A row ceiling here and not on the
        // browse, because the Query pane holds everything it fetches.
        let schema = selected.schema
        let name = selected.name
        run { db in
            try db.browseStatement(
                schema: schema, relation: name, filter: nil, order: nil, keys: [], limit: 1000)
        } then: { [self] suggestion in
            if queryText.isEmpty || queryText == suggestedQueryText {
                queryText = suggestion
                suggestedQueryText = suggestion
            }
        }
    }

    /// The columns of the selected relation, and which of them name one row.
    ///
    /// Both in one hop, because they are read for the same reason and a window
    /// that had the columns but not the identity would have to decide what to
    /// draw for an editing control it cannot yet answer for.
    private func loadColumns(for relation: RelationInfo, then next: @escaping @MainActor () -> Void)
    {
        // Busy for the duration, which the browse queued behind it sets again.
        // `canCancel`'s note says this flag covers metadata reads; that was true
        // of the refresh and not of this read, so a columns query that hung was
        // the one piece of work with no way to stop it. It is also what lets the
        // Structure pane tell "no relation chosen" from "the columns of the
        // chosen one have not arrived", which are the same empty list.
        isBusy = true
        run { db in
            (
                try db.columns(schema: relation.schema, relation: relation.name),
                try db.rowIdentity(schema: relation.schema, relation: relation.name),
                // Soft, and the only one of the three that is. A database this
                // build writes no statements for still has columns worth
                // showing, and letting this throw would take the Structure pane
                // down with the filter popup. Empty is the honest answer: no
                // column can be asked anything, so nothing is offered.
                (try? db.filterColumns(schema: relation.schema, relation: relation.name)) ?? []
            )
        } then: { [self] catalog in
            isBusy = false
            columns = catalog.0
            rowIdentity = catalog.1
            filterColumns = catalog.2
            next()
        }
    }

    /// Whether the Structure pane is waiting for the columns of the relation
    /// that has just been picked.
    ///
    /// An empty `columns` is three situations — nothing selected, a read in
    /// flight, a read that failed — and the pane draws each differently. Here
    /// rather than in the view because it is a statement about the connection,
    /// and because the view would have to know that `isBusy` is what covers the
    /// read.
    var isLoadingStructure: Bool { selected != nil && columns.isEmpty && isBusy }

    /// Empties every Structure section at once, so a section added later cannot
    /// be forgotten at one of the two places that has to drop the old one.
    private func clearRelationDetail() {
        indexes = []
        foreignKeys = []
        referencedBy = []
        constraints = []
        triggers = []
        ddl = nil
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
                // Swallowed rather than thrown: a database whose DDL the core
                // has not learned, or a kind it cannot assemble one for, is a
                // section with nothing in it — not a failure worth a banner
                // over a pane the user may not even have open.
                ddl: try? db.ddl(schema: schema, relation: name))
        } then: { [self] detail in
            indexes = detail.indexes
            foreignKeys = detail.foreignKeys
            referencedBy = detail.referencedBy
            constraints = detail.constraints
            triggers = detail.triggers
            ddl = detail.ddl
        }
    }

    /// Whether the Structure tab's sections below the columns have been asked
    /// for and not yet arrived.
    ///
    /// One question for all six, because they come back as one unit: no section
    /// can be further along than another, and the strip's counts are as unread
    /// as the rows under them. It matters that they land after the columns do —
    /// the pane switches from `isLoadingStructure` to the split, and every
    /// section is empty across that window, which is a table reported as having
    /// no indexes before anything has asked the server whether it has any.
    ///
    /// Empty here means never read. `selectionChanged` clears the sections for a
    /// new relation, while a refresh of the same one deliberately leaves the old
    /// values up, so this stays false there and the sections go on showing what
    /// they last knew — the answer the browse's stale rows already get. A
    /// relation that really has none of them holds this true until the browse's
    /// first page lands, since an empty arrival looks exactly like no arrival;
    /// that window is one the grid's veil is describing anyway, and it ends.
    ///
    /// Derived rather than stored for the reason `isLoadingStructure` gives, with
    /// one of its own: this read reports failure through `fail`, which puts
    /// `isBusy` back and touches nothing else, so a flag would leave the section
    /// spinning over an error the banner has already delivered.
    var isLoadingRelationDetail: Bool { selected != nil && !hasRelationDetail && isBusy }

    private var hasRelationDetail: Bool {
        !indexes.isEmpty || !foreignKeys.isEmpty || !referencedBy.isEmpty
            || !constraints.isEmpty || !triggers.isEmpty || ddl != nil
    }

    /// The sections the strip offers for the selected relation.
    ///
    /// DDL is the only conditional one. The other five are empty on a relation
    /// that has none of them, and an empty section still answers a question; a
    /// DDL section with nothing under it would offer to show a statement this
    /// build cannot write.
    var structureSections: [StructureDetail] {
        StructureDetail.allCases.filter { $0 != .ddl || ddl != nil }
    }

    /// How many rows a section holds, or nil for one that is not a list.
    ///
    /// A statement is a single value, and "1" beside it would answer a question
    /// nobody asked — the section being offered at all is what says there is one.
    func structureDetailCount(_ section: StructureDetail) -> Int? {
        // No count is the honest answer while the sections are on their way: a
        // zero beside every one of them is the same false claim the section
        // below is being kept from making, and it is the more emphatic of the
        // two — the counts exist so that "does this table have triggers" can be
        // answered without a click. Nothing new appears when they land, the
        // strip only gains the numbers, and it is already reshaping itself at
        // that moment because `structureSections` gains DDL along with them.
        guard !isLoadingRelationDetail else { return nil }
        switch section {
        case .indexes: return indexes.count
        case .foreignKeys: return foreignKeys.count
        case .referencedBy: return referencedBy.count
        case .constraints: return constraints.count
        case .triggers: return triggers.count
        case .ddl: return nil
        }
    }

    private struct RelationDetail: Sendable {
        let indexes: [IndexInfo]
        let foreignKeys: [RelationshipInfo]
        let referencedBy: [RelationshipInfo]
        let constraints: [ConstraintInfo]
        let triggers: [TriggerInfo]
        let ddl: String?
    }

    /// What the browse asks the core to write.
    ///
    /// The filter and the order go to the server rather than filtering rows
    /// already fetched, so they apply to the whole table instead of only the
    /// window in memory. Nothing here is a statement: the driver writes that,
    /// because quoting is the database's own and MongoDB's browse is not SQL.
    ///
    /// No limit. The cursor is what bounds a page, and it holds its own
    /// position.
    private func browseAsk(for relation: RelationInfo) -> BrowseAsk {
        let predicate = browsePredicate
        let user = orderClause.trimmingCharacters(in: .whitespacesAndNewlines)
        return BrowseAsk(
            schema: relation.schema, relation: relation.name,
            filter: predicate.isEmpty ? nil : predicate,
            order: user.isEmpty ? nil : user,
            keys: keyColumnsAfterTheUsersOrder)
    }

    /// One relation and the filter bar, as the core is asked for it.
    private struct BrowseAsk: Sendable {
        let schema: String
        let relation: String
        let filter: String?
        let order: String?
        let keys: [String]
    }

    /// The browse's ORDER BY, made total by the primary key.
    ///
    /// No longer what makes paging correct — the cursor holds one statement's
    /// position, so a later page cannot repeat or skip rows however the server
    /// chose to order them. What it still buys is a browse that looks the same
    /// each time it is run: without it the rows arrive in whatever order the
    /// plan produced, which is stable within a cursor and arbitrary between
    /// two. A relation with no primary key gets no such promise, and now pages
    /// anyway.
    private var keyColumnsAfterTheUsersOrder: [String] {
        // The user's own order may already name the key; repeating it is
        // harmless to a server and noise in the statement.
        columns.filter { $0.isPrimaryKey && $0.name != parsedOrder?.column }.map(\.name)
    }

    /// Whether the browse can fetch a further page. Needs a page boundary to
    /// fetch past and a cursor still open at it.
    var canLoadMore: Bool {
        activeTab == .content && browseResult.capped && !browseResult.isLoading
            && browseCursor != nil
            && browseResult.rowCount < browseResultBound
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
        guard browseResult.capped else { return nil }
        if browseCursor == nil {
            return PagingObstacle(
                label: "stopped part way",
                detail: "The read of \(selected?.name ?? "this relation") did not finish, "
                    + "so there is no position left to continue from. Run it again to page on.")
        }
        if browseResult.rowCount >= browseResultBound {
            return PagingObstacle(
                label: "maximum rows reached",
                detail:
                    "This result is holding the maximum of \(Self.formatted(browseResultBound)) rows. "
                    + "Use a filter to see more rows.")
        }
        return nil
    }

    /// Example filters, written against the selected relation. A fixed `id > 100`
    /// hint names a column most tables do not have, which reads as the field
    /// having been prefilled with something that will not run.
    ///
    /// Naming the relation's own first column fixed half of that and left the
    /// other half: `> 100` against a `text` column is a comparison no server
    /// accepts, so browsing anything whose first column is a name produced a
    /// hint spelling out a statement that errors. `IS NOT NULL` is the one
    /// predicate that is valid over every column of every type in every dialect
    /// this build speaks, which is what a hint that cannot be run against the
    /// table in front of it has to be. It teaches the same thing — that this
    /// field takes an expression over a column of this relation — without
    /// teaching an idiom that fails.
    var filterHint: (where: String, order: String) {
        guard let first = columns.first?.name else { return ("", "") }
        return ("\(first) IS NOT NULL", "\(first) desc")
    }

    /// Reads the relation from the top, through a cursor of its own.
    ///
    /// Opening and reading are two trips through the core queue rather than one,
    /// so the cursor reaches the main actor before its first page is asked for.
    /// Done in one, Cancel would have nothing to name for the whole of that
    /// page — which is the page most worth stopping, and the one a slow filter
    /// makes long. Opening does not execute the statement, so the window where
    /// there is still nothing to name is a round trip rather than a scan.
    private func runBrowse() {
        guard let selected else { return }
        // A pending edit points at the nth row of what was fetched, so the fetch
        // being replaced is the moment it stops meaning anything. Dropped here
        // rather than warned about: every path into this one is either the user
        // asking for the rows again or the rows already having been written.
        staged = StagedChanges()
        emptyColumns.reset()
        // The old cursor goes before the new one opens. Two cursors would mean
        // two connections and two open transactions for one pane showing one
        // result, and the second would outlive every reference to it.
        discardBrowse()
        let ask = browseAsk(for: selected)
        let label = selected.name
        let page = browsePage
        beginBrowseFetch()
        let started = CFAbsoluteTimeGetCurrent()
        run { db -> (String, Cursor) in
            // Written and opened in one trip. Writing it is string building in
            // the core rather than a question for the server, so it costs
            // nothing worth splitting the round trip for.
            let sql = try db.browseStatement(
                schema: ask.schema, relation: ask.relation, filter: ask.filter,
                order: ask.order, keys: ask.keys)
            return (sql, try db.cursor(sql, batchRows: page))
        } then: { [self] opened in
            let (sql, cursor) = opened
            browseCursor = cursor
            // Carried out of the closure because the core wrote it and nothing
            // on this side can write it again: the filter, the order and the
            // key columns are all things the user can change while the rows
            // from the old statement are still on screen.
            browseStatementText = sql
            fetchBrowsePage(
                from: cursor, takingSchema: true, describedAs: label,
                since: started, appending: false)
        }
    }

    /// Fetches the next page and appends it to the rows already on screen.
    ///
    /// The cursor holds the position, so this is a fetch rather than a second
    /// statement: nothing is re-read, and nothing can shift under it between
    /// pages the way a second `OFFSET` could.
    func loadMore() {
        guard canLoadMore, let selected, let cursor = browseCursor else { return }
        beginBrowseFetch(appending: true)
        fetchBrowsePage(
            from: cursor, takingSchema: false, describedAs: selected.name,
            since: CFAbsoluteTimeGetCurrent(), appending: true)
    }

    /// Pulls one page off `cursor` and installs it. Only the page that opened the
    /// cursor takes its schema; a later page would be re-describing columns the
    /// rows on screen were already built against.
    private func fetchBrowsePage(
        from cursor: Cursor, takingSchema: Bool, describedAs label: String,
        since started: CFAbsoluteTime, appending: Bool
    ) {
        run { () -> BrowsePage in
            // Taken here rather than a stage earlier so that it is released on
            // the way out of the same closure that allocated it: a fetch that
            // fails must not leave an ArrowSchema behind holding Rust's memory.
            let schema = takingSchema ? try cursor.schema() : nil
            do {
                let batch = try cursor.nextBatch()
                return BrowsePage(
                    cursor: cursor, schema: schema, batch: batch,
                    milliseconds: (CFAbsoluteTimeGetCurrent() - started) * 1000)
            } catch {
                if let schema { releaseSchema(schema) }
                throw error
            }
        } then: { [self] fetched in
            install(fetched, describedAs: label, appending: appending)
        }
    }

    /// Lets go of the browse's cursor, which is what closes its connection and
    /// rolls back the transaction the cursor was declared in.
    private func discardBrowse() {
        browseCursor = nil
        browseFetchInFlight = false
    }

    private func beginBrowseFetch(appending: Bool = false) {
        isBusy = true
        browseResult.beginLoading(appending: appending)
        browseFetchInFlight = true
        status = "Running…"
        // A new fetch supersedes the previous failure; leaving the banner up
        // would attribute an old error to the result now on screen.
        errorMessage = nil
    }

    /// Installs a fetched page into the browse result.
    private func install(_ fetched: BrowsePage, describedAs label: String, appending: Bool) {
        // A page whose browse was abandoned while it was in flight — the user
        // picked another relation, or disconnected — belongs to nothing on
        // screen. It has to be dropped rather than installed, and dropping it
        // means letting go of what it carries: installing it would put a
        // different relation's rows in the grid and, worse, hand back the cursor
        // `discardBrowse` had already released, leaving a connection open on a
        // browse nobody is looking at.
        guard browseFetchInFlight, fetched.cursor === browseCursor else {
            if let schema = fetched.schema { releaseSchema(schema) }
            if let batch = fetched.batch { releaseArray(batch) }
            return
        }
        browseFetchInFlight = false
        let grid = browseResult.table
        // A page carries a schema only when it opened the cursor. Re-installing
        // one on an append would drop the columns the existing batches were
        // built against.
        if let schema = fetched.schema {
            grid.reset()
            browseResult.selection = nil
            grid.setSchema(schema)
            releaseSchema(schema)
        }

        let before = grid.rowCount
        if let batch = fetched.batch { grid.append(batch: batch) }
        emptyColumns.weigh(
            rows: before..<grid.rowCount, columnCount: grid.columns.count,
            isNull: { grid.isNull(row: $0, column: $1) })
        // The selection this table had when it was last open, put back now that
        // its rows are here. Only on a fresh browse: an appended page is more of
        // the same result, and the selection was dealt with when the first page
        // landed. `selection(within:)` is what drops a row that has not come
        // back rather than pointing at the wrong one.
        if !appending, let restoring = stateToRestore {
            browseResult.selection = restoring.selection(within: grid.rowCount)
            stateToRestore = nil
        }
        // A short page is the end of the result: the server fills a FETCH to the
        // count asked for until it runs out. Exhausted is not a state the cursor
        // has to be asked about twice, and asking would cost a round trip per
        // page to learn what the page already said.
        let capped = grid.rowCount - before >= browsePage
        let summary = browseSummary(
            label: label, rows: grid.rowCount, capped: capped,
            seconds: fetched.milliseconds / 1000)
        if appending {
            browseResult.extend(
                capped: capped, milliseconds: fetched.milliseconds, summary: summary)
        } else {
            browseResult.finish(
                statement: browseStatementText, capped: capped,
                milliseconds: fetched.milliseconds, summary: summary)
            // Here, and deliberately not for an appended page. A later page is
            // the same statement still running — one FETCH after another on one
            // cursor — and an entry per page would say a table was browsed forty
            // times when it was opened once.
            //
            // The count is the whole result's as it stands, not this page's. An
            // entry saying 200 rows for a table of a million is the truthful
            // answer to what came back, and `capped` is what says there is more.
            history.record(
                browseStatementText, from: .browse, outcome: .rows(grid.rowCount),
                milliseconds: fetched.milliseconds)
        }
        // The rows describe themselves from here on, so `status` goes back to
        // describing the connection. Not putting it back is what left "Running…"
        // under a Query tab that had run nothing.
        status = settledStatus
        isBusy = false
    }

    /// The browse's cursor as the grid and the inspector strip both see it.
    ///
    /// Clamped on read rather than corrected on write, so that turning the
    /// setting off puts the cursor back where the user left it. Both readers go
    /// through this, which is what keeps the cell the strip describes and the
    /// cell the grid outlines from being two different cells.
    var browseSelection: GridSelection? {
        get { drawn(browseResult.selection) }
        set { browseResult.selection = newValue }
    }

    /// `selection` moved off a column the grid is not drawing, or nil where
    /// there is no drawn column left to move it to.
    private func drawn(_ selection: GridSelection?) -> GridSelection? {
        let hidden = hiddenBrowseColumns
        guard var s = selection, hidden.contains(s.column) else { return selection }
        guard
            let first = browseResult.table.columns.indices.first(where: { !hidden.contains($0) })
        else { return nil }
        s.column = first
        return s
    }

    /// Turns a cell menu's choice into a filter row, and runs the browse.
    ///
    /// No round trip any more. This used to ask the core for one cell's
    /// predicate and paste the text into the filter field; the row it makes now
    /// is compiled at Apply along with every other row, by the one compiler
    /// there is. The text never has to exist on this side at all.
    ///
    /// What it costs is the SQL that used to appear in the field, and that is
    /// not lost: the field goes on showing the whole stack's clause, greyed,
    /// once Apply has been through the core.
    ///
    /// `extend` appends and anything else starts again, which is the question
    /// the menu always asked — *Add to Filter* against *Filter on this column*.
    /// Answering it with rows is what makes replacing safe: it used to overwrite
    /// whatever somebody had typed, and now a Custom filter is cleared by the
    /// arrival of the first row rather than silently ANDed onto or wrapped in
    /// brackets it did not have.
    ///
    /// The list is opened. A row that landed in a shut disclosure would leave a
    /// number on a chevron as the only thing on screen saying what happened.
    func filterByCell(_ request: CellFilterRequest) {
        guard selected != nil else { return }
        if !request.extend { filterRules = [] }
        addFilterRule(
            FilterRule(column: request.column, op: request.op, value: request.value))
        isFilterRowsOpen = true
        applyFilters()
    }

    /// Puts the selected rows on the pasteboard as INSERT statements.
    ///
    /// Written by the core rather than assembled here, which is the same rule
    /// the Save button follows and for the same reason: quoting is the
    /// database's own, and whether a value is written bare or in quotes depends
    /// on the type its column was declared with. It also means the text copied
    /// out of this window is the text this window would have sent.
    ///
    /// Every column goes in, hidden ones included. A statement that quietly left
    /// out a column would insert a row that is not the row that was copied.
    ///
    /// A relation nothing can name a row of is fine here: an INSERT names no
    /// existing row, so the key this crate cannot find is one it does not need.
    func copyRowsAsInsert(_ rows: ClosedRange<Int>) {
        guard let relation = selected else { return }
        let grid = browseResult.table
        let names = grid.columnNames
        let request = EditRequest(
            schema: relation.schema, relation: relation.name,
            inserts: rows.map { row in
                EditRequest.Insert(
                    set: names.indices.map {
                        EditRequest.Cell(column: names[$0], value: grid.value(row: row, column: $0))
                    })
            })
        run { db in
            try db.editStatements(request)
        } then: { statements in
            // Terminated, because these are being pasted somewhere as a script
            // and the core writes statements to be sent one at a time.
            let script = statements.map { $0 + ";" }.joined(separator: "\n")
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(script, forType: .string)
        }
    }

    /// Adds a row, and takes the Custom field with it.
    func addFilterRule(_ rule: FilterRule) {
        // Emptied rather than kept and ignored. A filter that is still on screen
        // but no longer being sent is the one that gets blamed for the row
        // count, and nothing anywhere would contradict it.
        whereClause = ""
        filterRules.append(rule.settled(in: filterColumns))
    }

    /// The row the *Filters* list adds, or nil where nothing here can be
    /// filtered — a database this build writes no statements for, or a relation
    /// whose columns have not arrived.
    ///
    /// The first column and its first operator, which is `equals` for every
    /// type. A row that arrives already asking something is one popup away from
    /// the question somebody wanted; an empty one is a form to fill in.
    var newFilterRule: FilterRule? {
        guard let column = filterColumns.first, let op = column.operators.first else { return nil }
        return FilterRule(column: column.name, op: op)
    }

    /// Replaces one row, for a popup or a field that changed.
    ///
    /// Out of range is ignored rather than trapped. The rows are drawn from this
    /// array and each carries its own index, so a view built one frame ago can
    /// address a row a Remove button has since taken away — a real sequence, and
    /// not one worth crashing a window over.
    func updateFilterRule(at index: Int, to rule: FilterRule) {
        guard filterRules.indices.contains(index) else { return }
        filterRules[index] = rule.settled(in: filterColumns)
    }

    /// Drops one row, and with the last of them the clause they compiled to.
    ///
    /// The clause has to go with them or the next browse would send a WHERE that
    /// nothing on screen says. The Custom field is left as it is — empty, since
    /// `addFilterRule` emptied it — which is the unfiltered browse.
    func removeFilterRule(at index: Int) {
        guard filterRules.indices.contains(index) else { return }
        filterRules.remove(at: index)
        if filterRules.isEmpty { compiledClause = "" }
    }

    /// Runs the browse with what the filter bar now says.
    ///
    /// Rows go to the core to be compiled and the browse waits for the answer.
    /// It has to: the clause is written in this database's own quoting, and the
    /// only other way to have one here would be a second compiler in Swift that
    /// disagrees with `dbedit`'s the day either is corrected.
    ///
    /// A stack the core refuses — a comparison with nothing typed into it, a
    /// range with one end — stops here rather than running unfiltered. `run`
    /// puts the reason on screen and does not call back, so the grid goes on
    /// showing the rows the last filter returned, which is the honest thing for
    /// it to be showing.
    func applyFilters() {
        activeTab = .content
        guard !filterRules.isEmpty, let relation = selected else {
            // No rows means the Custom field is the filter, and it needs no
            // round trip. The clause is dropped in case rows were what ran last.
            compiledClause = ""
            runBrowse()
            return
        }
        let schema = relation.schema
        let name = relation.name
        let rules = filterRules
        run { db in
            try db.filterClause(schema: schema, relation: name, rules: rules)
        } then: { [self] clause in
            compiledClause = clause
            runBrowse()
        }
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
        if isImporting { return importStatus }
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
            let line = current.summary.isEmpty ? status : current.summary
            // The only feedback an appended page gets, now that the veil is not
            // over it. `canLoadMore` reads `isLoading`, so the button disappears
            // the instant it is pressed — without this sentence the window
            // answers the click by doing nothing visible for the length of a
            // hundred-thousand-row fetch.
            return current.isExtending ? "\(line) · loading more…" : line
        }
    }

    /// Whether ⌘R has anything to run. Drives the Run button's disabled state,
    /// so the button is never offered when pressing it would do nothing.
    ///
    /// A buffer holding only comments has text in it and nothing to run, which
    /// is why this asks the splitter rather than measuring the string.
    var canRun: Bool {
        activeTab == .query ? !scan.statements.isEmpty : selected != nil
    }

    /// The core's reading of the editor buffer as it stands.
    ///
    /// One property rather than a call at each site, because one scan answers
    /// every question the window asks about the buffer and `SQLScript` hands
    /// back the same one to all of them.
    private var scan: SQLScript.Scan {
        SQLScript.scan(queryText, scheme: scheme, selection: editorSelection)
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
    var runTarget: SQLScript.Target? { scan.target }

    /// What could be typed at `caret`, answered on the main actor.
    ///
    /// Silent on failure, which every other call here is not: a completion that
    /// could not be fetched is a list that does not appear, and the user has
    /// already moved on to the next character. Raising the error banner over it
    /// would put this application's plumbing on screen in the middle of somebody
    /// typing a word.
    ///
    /// Skipped outright while something is running. The core queue is serial, so
    /// a request made now would be answered when the statement finishes — long
    /// after the caret it was about moved — and every keystroke until then would
    /// add another.
    func completions(
        in text: String, caret: Int, then apply: @escaping @MainActor (SQLCompletion.Answer) -> Void
    ) {
        guard let db, !isBusy else {
            apply(.none)
            return
        }
        queue.async {
            let answer = (try? db.completions(in: text, caret: caret)) ?? .none
            DispatchQueue.main.async {
                MainActor.assumeIsolated { apply(answer) }
            }
        }
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
        activeTab == .query && !isBusy && !scan.statements.isEmpty
    }

    /// Whether there is any text to lay out. Unlike `canRunScript` this does not
    /// wait on `isBusy`: formatting never reaches the server, so there is no
    /// reason to refuse it while a statement is in flight.
    var canFormatQuery: Bool {
        activeTab == .query && !queryText.isEmpty
    }

    /// Lays the buffer out again, in place.
    ///
    /// The core answers with the text unchanged for anything it cannot read, so
    /// the only case to handle here is the one where nothing moved — and
    /// assigning the same string back would still push an edit onto the undo
    /// stack, leaving ⌘Z to undo a command that did nothing.
    func formatQuery() {
        let formatted = Database.formatted(queryText)
        guard formatted != queryText else { return }
        queryText = formatted
        // The caret cannot be left where it was: a `TextSelection` holds
        // `String.Index` values belonging to the old string, and every position
        // past the first moved token now names a different character anyway. The
        // end of the buffer is the one place that means the same thing before and
        // after.
        querySelection = TextSelection(insertionPoint: queryText.endIndex)
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
        let all = scan.statements
        guard !all.isEmpty else { return }
        // A buffer holding one statement is "query" here as it is under ⌘R, so
        // running a one-liner whole is described exactly as it always was.
        let labels =
            all.count == 1
            ? ["query"] : all.indices.map { "statement \($0 + 1) of \(all.count)" }
        runStatements(all, labelled: labels)
    }

    /// The words this database writes in front of a statement to ask for its
    /// plan, or nothing where it has none.
    ///
    /// Asked of the core on every read rather than cached: the answer is a fact
    /// about the connection's scheme, and this window outlives more than one
    /// connection. It is a table lookup and a string copy, which is nothing
    /// beside the menu validation it answers.
    private var explainPrefix: String? { Database.explainPrefix(for: scheme) }

    /// Whether the database can be asked for a plan of what ⌘R would run.
    ///
    /// Refuses while a run is in flight for the reason `canRunScript` does — the
    /// core queue is serial, so a second statement would only queue behind the
    /// first and land looking like a command that did nothing — and refuses
    /// outright where the database has no prefix, which is what the core
    /// answering nil rather than guessing a word is for.
    var canExplainStatement: Bool {
        activeTab == .query && !isBusy && runTarget != nil && explainPrefix != nil
    }

    /// Asks the database how it would run the statement the caret is in.
    ///
    /// Explains what ⌘R would send rather than the whole buffer: a plan is read
    /// against one statement, and the caret already says which one. The single
    /// space is joined here rather than kept in the dialect table, because the
    /// table records how a database spells the request and not how a caller lays
    /// it out.
    func explainCurrentStatement() {
        guard canExplainStatement, let target = runTarget, let prefix = explainPrefix
        else { return }
        runStatements([target.range], labelled: ["explain"], prefixedWith: prefix + " ")
    }

    /// What is about to be sent to a connection somebody marked production.
    struct ProductionRun {
        /// How many statements the run would send, of which `worst` is one.
        let count: Int
        /// The statement that set the level, which is the one the question is
        /// about — the first of the worst, so that a script's answer does not
        /// move when a later statement ties with it.
        let worst: String
        let danger: SQLScript.Danger
        /// The connection as the window is naming it, so that the dialog and the
        /// title bar cannot disagree about which server this is.
        let label: String

        var question: String {
            count == 1
                ? "Run this statement on “\(label)”?"
                : "Run \(count) statements on “\(label)”?"
        }

        /// The mark is not the news — the window has been showing it all along.
        /// What somebody cannot see from the Run button is which statement the
        /// caret was in, so the statement is what this shows.
        var detail: String {
            let opening =
                "This connection is marked production, and what you are about to run "
                + "\(danger.sentence)."
            let scope =
                count == 1
                ? "" : " It is one of \(count) statements this run would send, in order."
            return "\(opening)\(scope)\n\n\(preview)"
        }

        /// Cut rather than shown whole. A dialog that has become a scrolling
        /// document is one people dismiss without reading, which is the failure
        /// this whole mark exists to avoid.
        private var preview: String {
            worst.count <= 300 ? worst : "\(worst.prefix(300))…"
        }
    }

    /// Puts the production question, and answers whether the statements go.
    ///
    /// A property rather than a method for the reason `confirmDeletion` is one:
    /// the alert runs a modal loop, and the check suites have nobody at the
    /// keyboard to answer it.
    @ObservationIgnored
    var confirmProductionRun: @MainActor (ProductionRun) -> Bool = { run in
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = run.question
        alert.informativeText = run.detail
        // Run leads because it is what pressing ⌘R asked for; Cancel takes the
        // escape key, so dismissing the sheet without reading it sends nothing.
        alert.addButton(withTitle: "Run")
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// Whether `sql` may go, having asked if the connection says to ask.
    ///
    /// Asked here rather than at each of the three callers, because this is the
    /// one place editor text leaves for the server. A fourth way to run a
    /// statement, added later, is gated by having to come through here at all —
    /// three separate guards would leave it ungated by being forgotten.
    ///
    /// Asked every time, with no memory of a previous yes. dbx queues one
    /// confirmation per connection and drops it on disconnect; a remembered yes
    /// is the same as no mark for the rest of the session, and asking twice is
    /// cheaper than protecting nothing.
    private func mayRun(_ sql: [String]) -> Bool {
        let judged = sql.map { (text: $0, danger: SQLScript.danger(of: $0, scheme: scheme)) }
        // The first of the worst, written out rather than left to `max(by:)`,
        // whose answer among equals is not something to depend on.
        guard var worst = judged.first else { return true }
        for candidate in judged.dropFirst() where candidate.danger > worst.danger {
            worst = candidate
        }
        guard safety.asks(about: worst.danger) else { return true }
        return confirmProductionRun(
            ProductionRun(
                count: sql.count, worst: worst.text, danger: worst.danger,
                label: connectionLabel))
    }

    /// Runs `ranges` of the editor buffer in order on the one connection, and
    /// installs an outcome for each.
    ///
    /// The whole run is a single trip to the core queue rather than one trip per
    /// statement. The queue is serial anyway — one connection cannot service two
    /// statements — so hopping back to the main actor between them would buy
    /// nothing but a chance for a browse to interleave into the middle of
    /// somebody's script.
    ///
    /// A prefix, where there is one, is written in front of every statement in
    /// the run — which is what an Explain sends. What goes out is what the step
    /// list and the history then show: a run that asked for a plan reports the
    /// statement it actually sent rather than the one it was made from.
    private func runStatements(
        _ ranges: [Range<Int>], labelled labels: [String], prefixedWith prefix: String = ""
    ) {
        // The buffer as it is now. An error arrives after a round trip, and the
        // caret may only be moved while the text it indexes still exists.
        let script = queryText
        let sql = ranges.map { prefix + SQLScript.text($0, in: script) }
        // Before anything on screen moves. A window that had already dimmed the
        // result and said "Running…" behind the question would be describing a
        // run nobody had agreed to, and answering Cancel would leave it saying so.
        guard mayRun(sql) else { return }
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
            install(
                output, ranges: ranges, statements: sql, labels: labels, script: script,
                prefix: prefix)
        }
    }

    /// Turns a finished run into the steps the pane shows.
    private func install(
        _ output: ScriptOutput, ranges: [Range<Int>], statements: [String], labels: [String],
        script: String, prefix: String
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
            result.finish(
                statement: statements[i], capped: false, milliseconds: out.milliseconds,
                summary: summary)
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
                    i == stopped
                    ? (failure.cancelled ? .cancelled : .failed(failure.description))
                    : .notRun
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
            // The duration off the step's own result rather than re-measured
            // here: that is the number the status bar shows for the same step,
            // and a history disagreeing with the line under the grid about one
            // statement would make both of them worth checking.
            history.record(
                step.sql, from: .query, outcome: outcome,
                milliseconds: step.result.milliseconds)
        }
        // Where the eye should go. A run that stopped has exactly one place
        // worth looking and it is the statement that stopped it; a run that
        // finished lands on the last statement that returned anything, which is
        // where a script that ends by checking its own work keeps the answer.
        selectedStep =
            stopped
            ?? steps.lastIndex { $0.outcome.hasGrid }
            ?? max(steps.count - 1, 0)
        // The steps describe the run from here on, so `status` goes back to
        // describing the connection. Set before the failure branch below rather
        // than instead of it: `fail` writes "Failed" over this, which is the
        // right word for a run that stopped and the wrong one for a status line
        // still saying "Running…" about a run that ended either way.
        status = settledStatus
        isBusy = false
        // A statement in manual-commit mode opens the transaction it belongs to,
        // so this is where "uncommitted work" starts being true. A statement
        // that failed opens one too — the BEGIN went out before it.
        refreshTransaction()

        if let failure = output.failure, let stopped {
            // Through `fail`, so the banner, the status word and the caret are
            // the same ones every other failure gets.
            self.fail(
                with: StatementFailure(
                    error: failure,
                    sent: SentStatement(
                        script: script, range: ranges[stopped], prefix: prefix)))
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
        case .failed, .cancelled, .notRun:
            // No elapsed time: one of these has nothing to time, and for the
            // other two the number would sit beside "failed" or "cancelled"
            // looking like a measurement of the answer rather than of the wait
            // — and for a cancellation it would be a measurement of how long
            // the user put up with it, which is not a fact about the database.
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
        insertIntoEditor(entry.sql)
    }

    /// The same, for a statement somebody kept by name.
    ///
    /// A favorite arrives exactly as a recalled statement does, because from the
    /// editor's side there is no difference: it is a statement appended to the
    /// buffer and selected, ready for the ⌘R that follows.
    func recall(_ favorite: QueryFavorite) {
        insertIntoEditor(favorite.sql)
    }

    /// Appends a statement to the buffer and selects it.
    ///
    /// One implementation for both lists. Two would be two answers to where the
    /// caret ends up, and the whole value of either list is that the statement
    /// arrives ready to run.
    private func insertIntoEditor(_ sql: String) {
        activeTab = .query
        isHistoryOpen = false
        let existing = queryText.trimmingCharacters(in: .whitespacesAndNewlines)
        // The splitter strips the terminator from a statement, so a buffer that
        // ends without one would run straight into what is being appended and
        // the two would go to the server as a single statement.
        let prefix = existing.isEmpty ? "" : existing + (existing.hasSuffix(";") ? "\n\n" : ";\n\n")
        queryText = prefix + sql
        let start = prefix.unicodeScalars.count
        let end = start + sql.unicodeScalars.count
        if let selection = SQLScript.range(start..<end, in: queryText) {
            querySelection = TextSelection(range: selection)
        }
    }

    /// What Save Query would keep: exactly what ⌘R would send.
    ///
    /// One rule rather than three, and the one already on screen — the hint in
    /// the editor's corner names this same statement before it runs, so what is
    /// about to be saved is legible before anybody saves it. Keeping the whole
    /// buffer instead would file four statements under one name; keeping only a
    /// highlighted selection would leave the commonest case, a caret standing in
    /// a statement, with nothing to save at all.
    ///
    /// Nil where there is nothing to keep, which is what disables the control
    /// rather than offering one that files an empty statement.
    var savedQuery: String? {
        guard activeTab == .query, let target = runTarget else { return nil }
        let sql = SQLScript.text(target.range, in: queryText)
        return sql.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : sql
    }

    /// The favorites worth offering against the connection that is open.
    ///
    /// Empty scheme and all, which is `QueryFavorites.offered(to:)`'s decision
    /// rather than this one — before a connection lands `scheme` is empty too,
    /// and a list that showed nothing until you connected would hide the
    /// statements somebody keeps precisely so they do not have to remember them.
    var offeredFavorites: [QueryFavorite] { favorites.offered(to: scheme) }

    /// Keeps `savedQuery` under `name`, and says whether anything was kept.
    ///
    /// The scheme is taken from the connection that is open and is empty where
    /// there is none — which is what later makes the statement offered
    /// everywhere rather than nowhere.
    @discardableResult
    func saveQuery(named name: String) -> Bool {
        guard let sql = savedQuery else { return false }
        return favorites.save(name: name, sql: sql, scheme: scheme) != nil
    }

    // MARK: - Saved queries, in and out of a file

    /// Whether there is a list worth writing out. The item is greyed rather than
    /// producing a file holding `[]`, which is a file somebody would then try to
    /// work out what was wrong with.
    var canExportFavorites: Bool { !favorites.favorites.isEmpty }

    /// What the save panel proposes. Named for the application rather than for
    /// the connection: the file is a list of statements, and which database
    /// happened to be open when it was written says nothing about what is in it.
    var favoritesFilename: String { "dbclient-queries.json" }

    /// Writes every saved query, not only the ones the open connection is
    /// offered. The list belongs to the person, not to the session — exporting
    /// while connected to PostgreSQL must not silently drop their MySQL ones.
    func exportFavorites(to url: URL) {
        do {
            try QueryFavorites.encoded(favorites.favorites).write(to: url, options: .atomic)
            errorMessage = nil
        } catch {
            errorMessage =
                "\(url.lastPathComponent) could not be written: \(error.localizedDescription)"
        }
    }

    /// Reads a file of saved queries into the list.
    ///
    /// Merged rather than replaced — see `QueryFavorites.merge` — so opening the
    /// wrong file is something a person can walk back from.
    ///
    /// On success the window goes to the list that just changed. An import with
    /// no visible consequence is indistinguishable from one that silently did
    /// nothing, and this is the only evidence it worked.
    func importFavorites(from url: URL) {
        do {
            let incoming = try QueryFavorites.decoded(try Data(contentsOf: url))
            favorites.merge(incoming)
            errorMessage = nil
            activeTab = .query
            queryPanelTab = .favorites
            isHistoryOpen = true
        } catch {
            errorMessage =
                "\(url.lastPathComponent) is not a saved-queries file this build can read."
        }
    }

    // MARK: - The statement log, out to a file

    /// Whether there is anything to write.
    ///
    /// Against what is drawn rather than against the store, because that is what
    /// the file will hold. A history of two hundred browses with All switched off
    /// is a full store and an empty log, and a menu item that opened a save panel
    /// there would produce a file holding a header and nothing else.
    var canExportHistory: Bool { !shownHistory.isEmpty }

    /// What the save panel proposes. `.sql`, because that is what is in it —
    /// calling it `.log` would hide it from the application somebody would open
    /// it with.
    var historyFilename: String { "dbclient-statements.sql" }

    /// Writes the statements the panel is showing.
    ///
    /// What is shown and not what is stored. The All toggle and the filter field
    /// are already the scope control, so a second one in the save panel would be
    /// a way for the file to disagree with the list somebody read before asking
    /// for it — and the answer to "why is this statement not in here" would then
    /// live in two places.
    func exportHistory(to url: URL) {
        do {
            try Data(QueryHistory.script(shownHistory).utf8).write(to: url, options: .atomic)
            errorMessage = nil
        } catch {
            errorMessage =
                "\(url.lastPathComponent) could not be written: \(error.localizedDescription)"
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
    ///
    /// The suffix is only proposed for the scope that earns it. Now that the
    /// whole result can be written, `-first-200000-rows` on a file holding the
    /// whole table would be the same lie pointing the other way.
    func exportFilename(_ format: ExportFormat, scope: ExportScope) -> String {
        let base = activeTab == .query ? "query" : (selected?.name ?? "result")
        // Raw digits, not `formatted`: a comma in the name of a CSV file is a
        // joke that stops being funny at the first script that splits on one.
        let suffix: String
        switch scope {
        case .wholeResult: suffix = ""
        case .firstRows(let rows): suffix = "-first-\(rows)-rows"
        }
        return "\(base)\(suffix).\(format.fileExtension)"
    }

    /// Whether the panel has to ask how much to write.
    ///
    /// Only when the two answers differ. A result the grid holds in full has
    /// one, and a choice with no wrong answer is worse than no choice: it puts
    /// a decision in front of somebody who has no way to make it.
    var exportScopeIsAChoice: Bool { current.capped }

    /// What the save panel says above the name field.
    var exportMessage: String {
        guard current.capped else {
            return "Writes this result in full — \(Self.pluralized(current.rowCount, "row"))."
        }
        let count = Self.formatted(current.rowCount)
        let shown =
            "The grid is holding the first \(count) rows of this statement. "
            + "Choose whether to write those or re-read the whole result from the server."
        guard let obstacle = pagingObstacle else { return shown }
        return "\(shown) \(obstacle.detail)"
    }

    /// Writes the result the window is showing to `url`.
    ///
    /// Reads the statement again through a cursor of its own rather than
    /// writing out the rows the grid is holding. Those are as many rows as the
    /// grid stopped at, which is why the old export could not write a table
    /// longer than the scrollback at all; and a cursor is a snapshot, so the
    /// file is one moment of the table even while the grid moves on.
    ///
    /// Nothing is formatted here. Every row is written in the core, which is
    /// what keeps a large export bound by the socket and the disk rather than
    /// by this process.
    func exportCurrentResult(to url: URL, format: ExportFormat, scope: ExportScope) {
        guard canExport else { return }
        let statement = current.statement
        guard !statement.isEmpty else {
            errorMessage = "This result did not come from a statement that can be read again."
            return
        }
        let table = exportTableName
        let limit = scope.rowLimit
        let page = batchRows
        isExporting = true
        exportStatus = "Exporting to \(url.lastPathComponent)…"
        // A new export supersedes the previous failure, as a new query does.
        errorMessage = nil
        exportCursor = nil
        run { [self] db -> Int64 in
            let cursor = try db.cursor(statement, batchRows: page)
            // Published before the drain begins, so Stop has something to name
            // for all of it. A cursor handed over after `export` returns would
            // arrive once there was nothing left to cancel.
            DispatchQueue.main.async { [self] in exportCursor = cursor }
            if format.needsTable {
                return try cursor.exportSql(to: url, handle: db, table: table, rowLimit: limit)
            }
            return try cursor.export(to: url, format: format, rowLimit: limit)
        } then: { [self] rows in
            isExporting = false
            exportCursor = nil
            status = "\(Self.pluralized(Int(rows), "row")) written to \(url.lastPathComponent)"
        }
    }

    /// The table an `INSERT` script names.
    ///
    /// The relation being browsed, where there is one, because a script whose
    /// statements name the table they came from is one somebody can run. A
    /// query has no such answer, so it gets a placeholder that is obviously a
    /// placeholder rather than a plausible name pointing at the wrong table.
    private var exportTableName: String {
        guard activeTab != .query, let selected else { return "exported_rows" }
        guard !selected.schema.isEmpty else { return selected.name }
        return "\(selected.schema).\(selected.name)"
    }

    /// Stops an export part way through.
    ///
    /// The export runs on a cursor of its own, so `Database.cancel()` does not
    /// reach it for the same reason it does not reach a browse.
    func cancelExport() {
        exportCursor?.cancel()
    }

    // MARK: - Import

    /// Whether a file has a table to go into.
    ///
    /// Rows go into a relation, and the Query tab is not showing one — a result
    /// is not a table, however much it looks like one on screen. A connection
    /// marked read-only has no table to offer either, which greys the menu item;
    /// `importFile` below is what answers the drop that never went near a menu.
    var canImport: Bool {
        safety.writeRefusal == nil && activeTab != .query && selected != nil && !isBusy
            && !isExporting && !isImporting
    }

    /// The table a file is read into.
    ///
    /// Nil rather than a placeholder, which is where this parts company with
    /// `exportTableName` above. A placeholder in a file name is a bad name; a
    /// placeholder here is rows written into a table nobody chose.
    var importTableName: String? {
        guard canImport, let selected else { return nil }
        guard !selected.schema.isEmpty else { return selected.name }
        return "\(selected.schema).\(selected.name)"
    }

    /// Reads `url` into the relation being browsed.
    ///
    /// The format comes from the extension rather than from a menu, because the
    /// file already says what it is and a picker would only offer a way to
    /// disagree with it.
    func importFile(from url: URL) {
        // Before the table is worked out, because a dropped file arrives here
        // without passing a menu that could have been greyed out. Answering a
        // drop with silence looks like the drop was missed rather than refused,
        // and the difference matters when the file is somebody's afternoon.
        if let refusal = safety.writeRefusal {
            errorMessage = "\(refusal) Nothing was read from \(url.lastPathComponent)."
            return
        }
        guard let table = importTableName else { return }
        guard let format = ExportFormat(importPathExtension: url.pathExtension) else {
            let named =
                url.pathExtension.isEmpty ? "a file with no extension" : ".\(url.pathExtension)"
            errorMessage =
                "Nothing here reads \(named). Import reads CSV, TSV, JSON Lines and Parquet."
            return
        }
        isImporting = true
        importStatus = "Reading \(url.lastPathComponent) into \(table)…"
        // A new import supersedes the previous failure, as a new query does.
        errorMessage = nil
        run { db -> Int64 in
            try db.importFile(from: url, format: format, table: table)
        } then: { [self] rows in
            isImporting = false
            // Left in `importStatus` and not in `status`, because `refresh` sets
            // `status` on the next line and the count would be gone before it
            // could be read. What tells the user the rows arrived is the table
            // itself reloading under them, which is better evidence anyway.
            importStatus = "\(Self.pluralized(Int(rows), "row")) read into \(table)"
            // The grid is showing the table as it was a moment ago. Nothing else
            // will notice the rows arrived, because nothing else knows they did.
            refresh()
        }
    }

    // MARK: - Query execution

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
        if let estimate = selected.flatMap(\.estimatedRows), estimate > Int64(rows) {
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
        dispatch(on: queue, applyingInto: session, { try work(db) }, then: apply)
    }

    private func run<T>(
        _ work: @escaping @Sendable () throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        dispatch(on: queue, applyingInto: session, work, then: apply)
    }

    /// Runs `work` off the main actor and applies its result into `asked`.
    ///
    /// `asked` is read at the call, not at the arrival. That is the whole of what
    /// makes several connections in one window safe: an apply block writes
    /// through properties that reach whichever tab is in front, and by the time a
    /// slow statement answers, the tab in front may be a different database
    /// entirely. The question and its answer have to name the same session.
    private func dispatch<T>(
        on queue: DispatchQueue,
        applyingInto asked: Session,
        _ work: @escaping @Sendable () throws -> T,
        then apply: @escaping @MainActor (T) -> Void
    ) where T: Sendable {
        queue.async { [weak self] in
            do {
                let value = try work()
                DispatchQueue.main.async {
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        self.applying(to: asked) { apply(value) }
                    }
                }
            } catch {
                DispatchQueue.main.async {
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        self.applying(to: asked) { self.fail(with: error) }
                    }
                }
            }
        }
    }

    /// Puts `session` in front of the forwarding properties for the length of
    /// `body`, and puts back whatever was there.
    ///
    /// Nested rather than assigned flat, because an apply block starts work of
    /// its own — the transaction read after a connection lands is exactly that —
    /// and the dispatch inside it has to name the session being applied into
    /// rather than the tab on screen.
    ///
    /// A session that has since been closed is written into all the same, and
    /// nothing reads it: the object outlives the closure and is then let go.
    /// Checking for that would be a branch with no observable difference.
    private func applying(to session: Session, _ body: () -> Void) {
        let previous = applyingTo
        applyingTo = session
        defer { applyingTo = previous }
        body()
    }

    /// Whether there is a statement running for Cancel to stop.
    ///
    /// `isBusy` covers metadata reads and the browse as well as the Query pane,
    /// and all of them are the same thing to the server: one connection with one
    /// statement on it. A metadata read that hangs is exactly as worth stopping
    /// as a mistyped join.
    var canCancel: Bool { isBusy && db != nil }

    /// Asks the server to stop what this connection is running.
    ///
    /// Not on the core queue, which is the point: that queue is serial and is
    /// what is blocked, so a cancel dispatched onto it would be delivered after
    /// the statement it exists to interrupt had already finished. Not on the
    /// main actor either — the request opens a connection of its own and waits
    /// for the server, which is a round trip the window must not sit through.
    ///
    /// Nothing here changes what the window says. The statement is still running
    /// until the server says otherwise, and the answer arrives where every other
    /// answer does: as the running call failing, with `cancelled` set.
    func cancelRunningStatement() {
        guard canCancel, let db else { return }
        // Two different things to say, because two different things happen. Where
        // the request reaches the server this stops the work; where it does not,
        // it stops the waiting and the server finishes a page nobody will read.
        // Somebody who presses this after four minutes believes the first one, so
        // the second says what it is instead of borrowing the word.
        status = capabilities.cancelStopsTheStatement ? "Cancelling…" : "Giving up waiting…"
        // A browse page is fetched through a cursor, which runs on a connection
        // of its own — `db.cancel()` names the session's backend and would leave
        // the fetch running behind a button that said it had stopped it. Which
        // one is running is knowable here because the core queue is serial.
        if browseFetchInFlight, let cursor = browseCursor {
            DispatchQueue.global(qos: .userInitiated).async { cursor.cancel() }
        } else {
            DispatchQueue.global(qos: .userInitiated).async { db.cancel() }
        }
    }

    // MARK: - Health

    /// The open connections it is worth asking about.
    ///
    /// Two exclusions, and both are the difference between a useful answer and a
    /// misleading one. A session with nothing open has no connection to have
    /// lost. A session that is busy is running a statement on the one connection
    /// a ping would go down, and that connection is serial — so the ping would
    /// queue behind the statement and report the connection healthy at the
    /// moment it finally stopped being interesting. A statement in flight is also
    /// its own health check: if the connection is gone, the statement is about to
    /// say so.
    var connectionsWorthProbing: [Session] {
        sessions.filter { $0.db != nil && !$0.isBusy }
    }

    /// Asks each open connection whether it is still there.
    ///
    /// Called when the application comes back to the front, which is when this is
    /// worth knowing: the connection that was fine when somebody switched away
    /// may have been closed by a server timeout, a laptop lid, or a network that
    /// moved. Until now the first anyone heard of it was a statement failing.
    ///
    /// Nothing is closed and nothing is reopened. This moves the dot and stops —
    /// deciding on somebody's behalf to tear down a session, or to silently
    /// reconnect one, are both larger decisions than "the light went red".
    func probeOpenConnections() {
        for session in connectionsWorthProbing {
            guard let db = session.db else { continue }
            dispatch(
                on: session.queue, applyingInto: session, { db.ping() },
                then: { [weak self] alive in
                    guard let self else { return }
                    // Only ever downgrades. A session already showing a failure it
                    // reported for its own reasons is not talked out of it by a
                    // ping that happened to succeed afterwards.
                    if !alive { self.connectionState = .failed }
                })
        }
    }

    // MARK: - Transactions

    /// Whether this connection can be taken out of autocommit at all.
    ///
    /// False for most databases here, and it is the driver's answer rather than
    /// a preference: a session that runs each statement on a connection from a
    /// pool has nowhere for a transaction to stay open. Showing the control
    /// anyway would be offering a mode the connection cannot enter.
    var canControlTransactions: Bool { db != nil && transaction.transactional }

    /// Whether Commit and Rollback have anything to act on.
    var hasUncommittedWork: Bool { transaction.open }

    /// What quitting or closing the window would throw away, or nil where it
    /// would throw away nothing.
    ///
    /// Read by the guard in front of ⌘Q and ⌘W, which is the only thing between
    /// either of them and a process that ends. Derived rather than tracked: a flag
    /// kept beside the edits would be a second answer to a question the staged
    /// changes and the transaction already answer, and the day the two disagreed
    /// the wrong one would be the one nobody was asked about.
    var unsavedWork: UnsavedWork? {
        staged.lostOnQuitting(withOpenTransaction: hasUncommittedWork)
    }

    /// Enters or leaves manual-commit mode.
    ///
    /// The core refuses this while work is uncommitted rather than deciding what
    /// to do with it, so the refusal arrives as an ordinary error banner naming
    /// what to do first. Which is the right place for it: the window cannot
    /// answer "commit or discard?" on the user's behalf either.
    func setAutocommit(_ on: Bool) {
        guard canControlTransactions, !isBusy else { return }
        change({ try $0.setAutocommit(on) }, saying: on ? "Autocommit" : "Manual commit")
    }

    func commit() {
        guard hasUncommittedWork else { return }
        change({ try $0.commit() }, saying: "Committed")
    }

    func rollback() {
        guard hasUncommittedWork else { return }
        change({ try $0.rollback() }, saying: "Rolled back")
    }

    /// Takes one transaction step and reads the state back from the core.
    ///
    /// The read is part of the same queued job rather than a second one: between
    /// two jobs the window would be showing a state the connection has already
    /// left, and it is the state a person is about to press Commit against.
    private func change(
        _ step: @escaping @Sendable (Database) throws -> Void, saying said: String
    ) {
        guard !isBusy else { return }
        isBusy = true
        run { db -> TransactionState in
            try step(db)
            return try db.transactionState()
        } then: { [self] state in
            transaction = state
            isBusy = false
            status = said
        }
    }

    /// Reads the transaction state back after something that may have changed
    /// it — a connection opening, or a statement that opened a transaction
    /// without being asked to.
    private func refreshTransaction() {
        run { db in
            try db.transactionState()
        } then: { [self] state in
            transaction = state
        }
    }

    private func fail(with error: Error) {
        // A Query-pane failure arrives wrapped in the statement that caused it;
        // everything else is its own error.
        let statement = error as? StatementFailure
        let message = String(describing: statement?.error ?? error)
        isBusy = false
        isExporting = false
        isImporting = false
        // The core queue is serial, so at most one of these was running; clearing
        // both saves threading the target through the generic dispatch helper.
        browseResult.abandonLoading()
        queryResult.abandonLoading()
        // A fetch that failed — cancelled included — leaves its cursor inside an
        // aborted transaction, where every later fetch fails too. Letting it go
        // closes that connection; the rows already on screen stay, and
        // `pagingObstacle` is what tells the user there is no position left.
        if browseFetchInFlight { discardBrowse() }

        // A connect attempt answers into the form, which is the only surface on
        // screen while one is in flight. Routed by the flag rather than by
        // `db == nil`, because a reconnect fails with the previous connection
        // still open — and that connection is not what went wrong.
        if isConnecting {
            isConnecting = false
            connectionError = message
            // The tab the attempt made goes with it. A window that kept one dead
            // tab per mistyped password would be asking the user to clean up
            // after a refusal — and the connection they were working in is still
            // there, in front, untouched.
            //
            // Except where the attempt filled the tab the window already had.
            // There is nothing behind that one, so it stays and says so, which is
            // the state a window opens in.
            if let opened = sessionBeingOpened, sessions.count > 1,
                let index = sessions.firstIndex(where: { $0 === opened })
            {
                sessions.remove(at: index)
                if index < activeSession { activeSession -= 1 }
            } else {
                connectionState = .failed
                status = "Not connected"
            }
            sessionBeingOpened = nil
            return
        }

        // A cancellation is not a fault and does not get the banner. The server
        // reports it as an error — "canceling statement due to user request" —
        // and rendering that in red would tell the user their statement was
        // wrong when what happened is that the button they pressed worked. The
        // caret stays where it was for the same reason: there is nothing in the
        // statement to point at.
        if ((statement?.error ?? error) as? DbError)?.cancelled == true {
            status = "Cancelled"
            return
        }

        errorMessage = message
        status = "Failed"
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
    /// A position swallowed by a prefix this application wrote leaves the banner
    /// as it is rather than falling through to the bare wording below: "at
    /// position 12 of the statement" would be naming a character in words the
    /// user never typed.
    private func pointAtSyntaxError(_ statement: StatementFailure) {
        guard let failure = statement.error as? DbError, let reported = failure.position,
            let position = SQLScript.position(reported, without: statement.sent.prefix)
        else { return }
        let sent = statement.sent
        guard sent.script == queryText,
            let offset = SQLScript.errorOffset(ofPosition: position, in: sent.range),
            let selection = SQLScript.range(scan.tokenRange(at: offset), in: queryText)
        else {
            errorMessage = "\(failure.description) · at position \(position) of the statement"
            return
        }
        let place = SQLScript.lineColumn(of: offset, in: queryText)
        errorMessage = "\(failure.description) · line \(place.line), column \(place.column)"
        querySelection = TextSelection(range: selection)
    }

    private static func label(for connString: String) -> String {
        // "postgres://user@host/db" → "db@host", which is how these tools name
        // a session and how users refer to one. A database that is a file has no
        // host, and is named by its file instead.
        ConnectionURL.label(for: connString)
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
/// One fetched browse page on its way back from the core queue.
///
/// Carries the cursor because the first page is what opens it: the main actor
/// cannot hold a cursor it has not been given yet, and the fetch cannot install
/// itself.
/// Hands an exported schema back through its own release callback.
///
/// Outside the model because it is called from the core queue as well as from
/// the main actor: the closure that allocates one has to be able to let it go
/// again when the fetch after it fails.
private func releaseSchema(_ schema: UnsafeMutablePointer<ArrowSchema>) {
    if let release = schema.pointee.release { release(schema) }
    schema.deallocate()
}

/// The same, for a batch that never reached a grid. One that did is owned by
/// `ArrowTable`, which releases it when the table is reset.
private func releaseArray(_ array: UnsafeMutablePointer<ArrowArray>) {
    if let release = array.pointee.release { release(array) }
    array.deallocate()
}

private struct BrowsePage: @unchecked Sendable {
    let cursor: Cursor
    /// Only the page that opened the cursor brings one.
    let schema: UnsafeMutablePointer<ArrowSchema>?
    /// Absent once the cursor is exhausted.
    let batch: UnsafeMutablePointer<ArrowArray>?
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
