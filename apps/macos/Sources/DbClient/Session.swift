import Foundation
import Observation
import SwiftUI

/// One open connection, and the state that belongs to it rather than to the
/// window around it.
///
/// The split is by lifetime. A property lives here if disconnecting would make
/// it wrong, and stays on `AppModel` if it describes the window instead: the
/// connection form's draft is the window's, because it is what somebody is
/// typing whichever database is open; the label in the chrome is the
/// connection's, because it is a claim about which server answered.
///
/// `@Observable` in its own right. `AppModel` reaches these through forwarding
/// properties of the same names, and a view that reads one still updates,
/// because observation registers whatever was actually read while a body was
/// evaluated — which is the property on this object, not the forwarder that
/// arrived at it.
@Observable
@MainActor
final class Session: Identifiable {
    /// Its own, so a strip of tabs can be drawn from the list without using a
    /// position as a name. Two connections to the same server have the same
    /// label and a position moves when a tab to the left of it closes.
    let id = UUID()

    /// The live connection. Nil before one opens and after one is dropped;
    /// letting go of it is what closes it.
    var db: Database?

    /// One serial queue per session.
    ///
    /// Serial because a single connection cannot service overlapping
    /// statements — that is a property of the connection, which is why the
    /// queue is one too. Two connections have no reason to wait on each other,
    /// and a queue shared between them would be exactly that wait.
    let queue = DispatchQueue(label: "dev.dbclient.core", qos: .userInitiated)

    /// The string this connection was opened with.
    var connString = ""

    /// The bastion it was opened through, or nil for one dialled directly.
    ///
    /// Here rather than looked up again from the form, because by the time it is
    /// wanted the form has moved on. Opening another database on this server is
    /// a second connection to the *same* bastion, and the row the chooser is
    /// showing by then may be a different connection with a different one — or
    /// with none, which would quietly dial a host only the bastion can reach.
    var bastion: SshConfig?

    /// Which entry in the navigator cache this session's tree is, or nil where
    /// there is nothing to file it under — a connection string that is not a URL
    /// names no database to key on.
    var cacheKey: NavigatorCacheKey?

    /// Whether what the navigator is drawing came off disk rather than off the
    /// server.
    ///
    /// True only for the moment between a connection being asked for and its
    /// tree arriving, and only where there was something cached to draw. The
    /// navigator dims itself while it is set: rows that may since have gone are
    /// worth showing, and worth showing as provisional.
    var isTreeStale = false

    /// The name on the saved entry this connection was opened from, or nil off
    /// the paths that have no entry — quick connect and `--conn`. Carried on
    /// the session so another database opened or switched to from this tab
    /// keeps the name; like the safety marks, a rename in the sidebar reaches
    /// this tab's title at its next connection, not before.
    var savedName: String?

    /// What the tab is called: the saved entry's name where there is one, the
    /// address in the connection string otherwise. The default is what a tab
    /// holding nothing but the connection form is called, which is the only
    /// time it is read: every other path sets it when a connection is opened.
    var connectionLabel = "New Connection"
    var connectionState: StatusDot.State = .connecting

    /// Which driver this connection is on, read back out of the string it was
    /// opened with rather than kept as a second copy that can disagree with it.
    /// Empty until an attempt is made, which is what `DriverBadge`'s callers
    /// test before drawing a mark for a database nobody has named yet.
    var scheme: String { ConnectionURL.scheme(in: connString) }

    /// Whether this tab has ever been pointed at a server.
    ///
    /// The dot draws only then. All three states it can show are claims about
    /// an attempt — amber that one is running, green that one worked, red that
    /// one did not — and a tab holding a blank form has made none. A dot there
    /// would have to pick one of the three and be wrong.
    var hasBeenAsked: Bool { !connString.isEmpty }

    /// What answered, as the driver reported it — "PostgreSQL 17.0", "TiDB
    /// 8.1.0", or empty until a connection has been made.
    ///
    /// On the session as well as on the saved entry, and the two are not the same
    /// claim: the entry remembers what was there last time, and this is what is
    /// there now. Quick connect and `--conn` have no entry at all, and they are
    /// the connections most likely to be pointed at something unfamiliar.
    var server = ""

    /// How long this connection was given to answer, in seconds, and how long
    /// another database opened from this tab will be given. Zero is the driver's
    /// own patience; see `ConnectionSettings.timeoutSeconds`.
    var timeoutSeconds = 10

    /// How often this connection is pinged while idle, in seconds, and zero for
    /// never. Carried on the session for the reason `timeoutSeconds` is —
    /// another database opened from this tab dials the same server and should
    /// be kept alive at the same rate — and already resolved: the form's "use
    /// the Settings default" answer was turned into a number when the
    /// connection opened, so the timer never has to ask two places.
    var keepAliveSeconds = 0

    /// When this connection was dialled or last deliberately pinged, which is
    /// what the keep-alive clock measures from.
    ///
    /// Stamped when a ping is *sent* rather than when it answers — see
    /// `AppModel.keepAliveTick` for why that direction matters. The distant
    /// past rather than now for a session nobody has dialled, so a connection
    /// handed to a session by other means is simply due at once instead of
    /// carrying a stamp about a dial that never happened.
    var lastKeptAlive = Date.distantPast

    /// The whole of what this tab is, for the tooltip over a 100pt name.
    ///
    /// A tab has room for one of the three things somebody needs to tell two of
    /// them apart — what it is called — and the two it drops are the two that
    /// matter when the names are similar: which product answered and which
    /// address it is at. The pointer is already on the tab by the time this is
    /// read, so it costs nothing to be complete.
    var tabDescription: String {
        guard hasBeenAsked else { return connectionLabel }
        // The saved row's line, from the string this tab was actually opened
        // with: `ConnectionSettings.address` drops the password, which a tooltip
        // built out of `connString` would print.
        let address = ConnectionSettings(connectionString: connString).address
        var parts = server.isEmpty ? [address] : ["\(server) · \(address)"]
        // Last, and spelled out. The two glyphs beside the name are the mark; a
        // glyph is a reminder for somebody who already knows what it means, and
        // the tooltip is where the person who does not finds out.
        parts.append(contentsOf: safety.labels)
        return parts.joined(separator: " · ")
    }

    /// What this connection's tab is called out loud.
    ///
    /// The stale mark is a dimmed tree, and dimming is not a thing a screen
    /// reader can report — so the one fact the sighted reading carries has to be
    /// said in words somewhere, or a tree that came off disk is announced to
    /// some people as the live one. The tab is that somewhere: it is the control
    /// that names this connection, and the tree below it is this connection's.
    ///
    /// Here rather than in the tab's own body, so that a check can read it. A
    /// sentence built in a view body is one nothing but a person looking at the
    /// screen can see, and this is the half of the mark no screenshot shows.
    var accessibleDescription: String {
        // Without the state for a tab that has made no attempt, for the reason
        // `hasBeenAsked` gives about the dot: naming one of the three would be
        // saying out loud the thing the dot is not drawing.
        guard hasBeenAsked else { return connectionLabel }
        var parts = ["Connection \(connectionLabel)", connectionState.label]
        // Before the stale note, for the reason the saved row puts them before
        // its own two marks: these are why somebody stops at this tab, and the
        // rest is a detail about its state. Said out loud because the glyphs
        // carrying them on screen are the half a screen reader cannot report.
        parts.append(contentsOf: safety.labels)
        if isTreeStale { parts.append("showing the objects from the last time it was open") }
        return parts.joined(separator: ", ")
    }

    /// What the open connection can do, read once when it is adopted.
    ///
    /// On the session rather than on the window because it describes one
    /// connection: a window holding a PostgreSQL tab and a Cassandra tab has two
    /// different answers to the same question, and one of them says Cancel works.
    var capabilities: Capabilities = .unknown

    /// The colour of the connection now open.
    ///
    /// Carried out of the chooser into the session, because the colour is not there
    /// to decorate the sidebar: somebody marks a connection red so that they can
    /// tell, while looking at a grid of rows, which server they are about to change.
    /// A mark that stopped at the moment of connecting would be a mark shown only
    /// when it does not matter yet.
    var connectionColor: ConnectionColor = .none

    /// What the connection now open is allowed to do.
    ///
    /// Carried out of the chooser into the session for the reason `connectionColor`
    /// above is, and answering a sharper question than the colour does: a mark
    /// that stopped at the moment of connecting would be a mark that applied only
    /// while nothing could go wrong.
    var safety = ConnectionSafety()

    var status = "Connecting…"
    var isBusy = false
    var errorMessage: String?

    /// What the connection's transaction is doing, as of the last thing that
    /// could have changed it.
    ///
    /// Read back from the core after each of those rather than predicted here.
    /// The core is the side that sent the BEGIN, so a copy kept in the window
    /// would be a second answer with nothing to keep it honest — and the one
    /// that is drawn on screen would be the one nobody checked.
    var transaction: TransactionState = .none

    // MARK: - Navigator

    var schemas: [SchemaInfo] = []
    /// The databases on this server, or nil where the engine has no level above
    /// schemas.
    ///
    /// Not flattened to an empty array. Nil and empty are different answers —
    /// no such level, against a login that can see none of them — and
    /// `Metadata.swift` is where that difference is written down. A session that
    /// has not read them yet is nil too, which is the same as "nothing to draw"
    /// and is the right thing to draw before the first answer arrives.
    var databases: [DatabaseInfo]?
    var relations: [String: [RelationInfo]] = [:]

    /// Functions and procedures, by schema. Empty on a connection whose
    /// `capabilities.reportsRoutines` is false, which is why the navigator reads
    /// that flag and not this: a driver never taught to look and a schema with
    /// none to find leave behind the same empty dictionary.
    var routines: [String: [RoutineInfo]] = [:]
    var expanded: Set<String> = []

    /// Schemas whose Routines group is open, kept apart from `expanded` so that
    /// opening a schema and opening the group inside it are two arrangements
    /// rather than one — the second survives collapsing and reopening the first.
    var expandedRoutineGroups: Set<String> = []
    var selected: RelationInfo?

    /// The routine the detail panes are describing, or nil while they are
    /// describing `selected`.
    ///
    /// Beside `selected` rather than replacing it. A routine and a relation are
    /// selected from the same tree and only one of them at a time, but they are
    /// not the same kind of thing — a relation carries browsed rows, a paging
    /// position, a staged edit and a WHERE clause, and folding the two into one
    /// property would mean clicking a function throws all of that away. Clicking
    /// back onto the table finds it exactly as it was.
    var selectedRoutine: RoutineInfo?

    /// The source of `selectedRoutine`, nil while it is on its way and nil for a
    /// routine the driver hands back nothing for. The two are told apart by
    /// `AppModel.isLoadingRoutineSource`, the same way an empty `columns` is.
    var routineSource: String?

    /// The sequence the detail panes are describing, under the same arrangement
    /// as `selectedRoutine` and mutually exclusive with it.
    ///
    /// Two optionals rather than one enum of the two, because every pane that
    /// draws one of them wants it unwrapped and nothing wants to switch over
    /// both. `AppModel.navigatorSelection` is the only writer, which is what
    /// keeps at most one of them set.
    var selectedSequence: SequenceInfo?

    /// Sequences, by schema. Empty where `capabilities.reportsSequences` is
    /// false, which is what the navigator reads rather than this.
    var sequences: [String: [SequenceInfo]] = [:]

    /// Schemas whose Sequences group is open, kept apart from the routine
    /// groups for the reason those are kept apart from `expanded`.
    var expandedSequenceGroups: Set<String> = []

    /// Set while `refresh` swaps `selected` for the freshly read value naming
    /// the same relation. The two are the same object to a user but not to
    /// `==` — `estimatedRows` moves on its own — and that assignment must not
    /// look like the user picking a table: `selectionChanged` clears the WHERE
    /// and ORDER BY fields, and a refresh that threw the filters away would be
    /// a worse answer than the stale pane it was pressed to fix.
    var isReselecting = false

    /// Name filter for the navigator. A schema with hundreds of objects is the
    /// normal case, and scrolling to find one is the slowest thing a user does.
    var navigatorFilter = ""

    // MARK: - Detail

    /// Which pane is showing.
    ///
    /// Recorded in the history on its way through, because moving between a
    /// table's structure and its rows is moving: Back from the rows should mean
    /// the description of the same table, not the table before it.
    var activeTab: DetailTab = .content

    var columns: [ColumnInfo] = []

    /// Which of those columns name one row, as the core decides it. Read
    /// alongside the columns, because every question about editing is a question
    /// about this one.
    var rowIdentity: RowIdentity?
    var indexes: [IndexInfo] = []
    var foreignKeys: [RelationshipInfo] = []
    var referencedBy: [RelationshipInfo] = []
    var constraints: [ConstraintInfo] = []
    var triggers: [TriggerInfo] = []

    /// The statements that would recreate the selected relation. Nil where the
    /// core cannot write them, which is what keeps the DDL section off a
    /// relation it would have nothing to show for.
    var ddl: String?

    // MARK: - Content pane

    let browseResult = ResultSet()

    /// What each relation's Content tab was showing, so that leaving a table and
    /// coming back is not the same as opening it.
    ///
    /// Per session rather than per window, because its keys are `schema.name`
    /// strings and the same string names a different table on a different
    /// server — which is the same reason `reset` clears it.
    var browseStore = BrowseStore()

    /// The state to put back once the newly selected relation's rows arrive.
    ///
    /// Held rather than applied at selection time because there is nothing to
    /// select yet: the grid is emptied and refilled by a round trip, and
    /// `install` clears any selection made before that lands.
    var stateToRestore: BrowseState?

    /// Stands in before anything has run here. A result that has never run is a
    /// different state from a statement that returned nothing, and the pane
    /// draws them differently.
    let pristine = ResultSet()

    // MARK: - Content pane filters

    var whereClause = ""
    var orderClause = ""

    /// The filter rows, in the order they are drawn.
    ///
    /// Read from anywhere and written only through the three functions beside
    /// `applyFilters`. That is what holds the one invariant this pair has: these
    /// and `whereClause` are never both filled, because they are two ways of
    /// saying one thing and a browse can only send one WHERE.
    var filterRules: [FilterRule] = []

    /// The WHERE those rows last compiled to, as the core wrote it.
    ///
    /// Somewhere other than `whereClause` on purpose. Writing it there would put
    /// the rows and a text saying the same thing in two editable places, and the
    /// next keystroke in either would make them disagree with nothing on screen
    /// saying which one the grid is showing.
    var compiledClause = ""

    /// What each column of the selected relation may be asked, as the core
    /// answers it.
    ///
    /// Empty until a relation's columns have arrived, and empty for good against
    /// a database this build writes no statements for. The rows are drawn from
    /// this, so empty is also how the *Filters* disclosure knows not to offer
    /// itself — an offer this side cannot compile is not one.
    ///
    /// Per session because the answers are the dialect's: the same column name
    /// on the next server may be asked different things.
    var filterColumns: [FilterColumn] = []

    // MARK: - Query pane

    /// The statements the pane last ran, in order, each with what it did.
    ///
    /// A run of one is what ⌘R makes and a run of five is what ⌥⌘R makes. Five
    /// statements produce five outcomes and this pane has one grid, so the list
    /// is where the other four go: showing one and saying nothing about the rest
    /// is the class of lie the status bar's "first 100,000 of ~1,000,000 rows"
    /// exists to prevent.
    var scriptSteps: [ScriptStep] = []

    /// Which step the pane is showing, as an index into `scriptSteps`.
    var selectedStep = 0

    /// What the history list is being narrowed by.
    ///
    /// Matched against the whole statement rather than the one-line preview, so
    /// a table named in the fifteenth line of a script is still findable by
    /// name — which is how somebody looks for "the one that touched orders".
    ///
    /// Per session, unlike the store it narrows: the needle almost certainly
    /// names a table of the database it was typed against.
    var historyFilter = ""

    var queryBuffers: [AppModel.QueryBuffer] = [AppModel.QueryBuffer(name: "query 1")]
    var activeQueryBufferIndex = 0

    /// Where the caret or selection is in the editor.
    ///
    /// Owned here rather than by the pane because the Run button lives in the
    /// window's toolbar, which has no view of the editor. ⌘R has to know which
    /// statement the user is standing in, and this is the only place both ends
    /// can see.
    var querySelection: TextSelection?

    /// The last statement `selectionChanged` put in the editor, so a later
    /// selection can tell "untouched suggestion" from "the user's work".
    var suggestedQueryText = ""

    // MARK: - Cursors

    /// The cursor the Content tab is reading through, and whether a page of it
    /// is in the air right now.
    ///
    /// Held here rather than on `ResultSet` because a cursor is a database
    /// connection, not a property of the rows on screen: the query pane's
    /// results never have one and `current` can hand back a `ResultSet` that
    /// never did. Letting go of it is what closes that connection, so every
    /// path that abandons a browse goes through `discardBrowse`.
    ///
    /// The most load-bearing thing on this object. A cursor is a connection the
    /// server is holding open on this session's behalf; one left behind when a
    /// session goes is a connection nothing will ever close.
    var browseCursor: Cursor?

    /// The statement `browseCursor` was opened over, so an export can ask the
    /// server the same question through a cursor of its own.
    var browseStatementText = ""

    /// The cursor an export is draining, so Stop can reach it. Nil except
    /// while one is running.
    var exportCursor: Cursor?
    var browseFetchInFlight = false

    /// What the pages fetched so far say about which browse columns are empty.
    ///
    /// The evidence, kept whether or not anything acts on it, so that switching
    /// the setting on acts on the result already on screen rather than on the
    /// next one: a checkbox that appears to do nothing until you reload is a
    /// checkbox nobody believes. Gathering it costs one null check per column
    /// per page for every column that holds anything, which is nearly all of
    /// them.
    var emptyColumns = EmptyColumns()

    // MARK: - Back and forward

    /// Where this session has been, and where Back and Forward go.
    var browseHistory = BrowseHistory()

    /// Set while Back or Forward is doing the moving.
    ///
    /// Without it the arrival that Back performs would be recorded as a new
    /// place, which appends to the path and throws away everything ahead — so
    /// Back would work once and Forward would never have anywhere to go.
    var isNavigatingHistory = false

    // MARK: - Editing

    /// Changes made to the browse result and not yet sent.
    ///
    /// Held here rather than in the grid because they outlive the view and are
    /// what Save reads. Cleared whenever the result is re-read, which is the same
    /// moment the database's own answer replaces what was typed.
    var staged = StagedChanges()

    /// Whether the value viewer under the inspector strip is open.
    ///
    /// It survives moving between the detail tabs, because somebody comparing a
    /// long value against a query result should not have to reopen it on the way
    /// across. It does not survive changing connection, because the cell it was
    /// opened over does not either.
    var isValueViewerOpen = false

    /// Whether the viewer is in its editing mode.
    var isEditingValue = false

    // MARK: - Transfers

    /// Set while a result is being written to a file. The write happens off the
    /// main thread, so without this the window would sit looking idle for
    /// however long a million rows take to reach the disk.
    var isExporting = false

    /// What the status bar reads while that write is in progress.
    ///
    /// A property of its own rather than a sentence in `status`, and it was worth
    /// re-deriving why once `status` stopped being left on "Running…" for the
    /// rest of a session. Two reasons, both still true, and neither is the one
    /// that used to be written here:
    ///
    /// Precedence. `statusLine` prefers the pane's own `current.summary` and
    /// falls back to `status`, so a sentence put in `status` would not be read
    /// out at all over a result that has a summary — which is every result an
    /// export can be taken from.
    ///
    /// Interleaving. `exportQueue` is separate from the core queue on purpose, so
    /// the window stays usable while a million rows go to disk: `canRun` and the
    /// navigator are both live, and a query started mid-export writes `status`
    /// twice — "Running…", then the settled sentence. The export's own line would
    /// be gone while the export was still running.
    ///
    /// Reading it through `isExporting` is what makes it self-clearing: the flag
    /// going false restores whatever the tab was saying, so no stale "Exported…"
    /// can outlive the write it described.
    var exportStatus = ""

    /// Set while a file is being read into a table, for the reason above and one
    /// more: an import ends by refreshing the table it wrote to, and the refresh
    /// puts its own sentence in `status` immediately. Without a flag of its own,
    /// "1,000 rows read into orders" is overwritten before it is legible.
    var isImporting = false
    var importStatus = ""

    /// Rows on their way to another connection in this window, and what the
    /// status bar says while they are.
    ///
    /// On the *source* session, which is the tab the transfer was started from
    /// and the one whose queue is driving it. The target is marked busy for the
    /// duration and is held here so that whatever ends the transfer — the last
    /// batch, a Stop, a failure — can hand it back.
    var isTransferring = false
    var transferStatus = ""
    var transferHandle: Transfer?
    var transferTarget: Session?

    /// Whether `--where` / `--order` have been spent. One session's worth: they
    /// describe the database the window opened against.
    var appliedInitialFilters = false
}
