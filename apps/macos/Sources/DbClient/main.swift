import AppKit
import MetalKit
import SwiftUI

// Two entry points share one window.
//
// `--bench` runs the bare Metal grid with a scripted scroll and frame
// statistics. It deliberately skips the SwiftUI shell so the numbers measure
// the data surface rather than the chrome around it, and stay comparable with
// every earlier measurement.
//
// Without it, the full application shell starts — on the connection form.
// There is no built-in one: a default would silently connect to whatever
// happened to be listening on a port, which is the one thing a database client
// must never do.

let benchSQL = "SELECT * FROM bench_wide"
let benchMode = CommandLine.arguments.contains("--bench")
let verifyMode = CommandLine.arguments.contains("--verify")
let benchFrames = 600

/// Value following `flag` on the command line, if any.
func argument(_ flag: String) -> String? {
    guard let i = CommandLine.arguments.firstIndex(of: flag),
        i + 1 < CommandLine.arguments.count
    else { return nil }
    return CommandLine.arguments[i + 1]
}

/// `--conn "postgres://user:password@host:port/database"` connects to that
/// database without asking. The scheme names the driver — `sqlite:///path.db`
/// reaches a file instead. Every automated path — the benchmarks, the
/// screenshot captures — comes in this way, and nothing it opens is remembered:
/// a capture run must not change which database the next launch opens.
let connArgument = argument("--conn")

/// `--safety production,read-only` opens `--conn` with the marks a saved
/// connection would have carried.
///
/// The marks live in `connections.json` and `--conn` is a string with no entry
/// behind it, so this path is otherwise always unmarked — which means the tab's
/// two glyphs, the danger line under a production tab and the read-only
/// refusal in the editing row cannot be photographed at all. Both words, either
/// order, comma-separated; anything else is ignored rather than refused, because
/// a capture switch that exits over a typo is a capture nobody gets.
let safetyMarks: ConnectionSafety = {
    let asked = Set((argument("--safety") ?? "").split(separator: ",").map(String.init))
    return ConnectionSafety(
        isReadOnly: asked.contains("read-only"), isProduction: asked.contains("production"))
}()

/// `--reconnect "postgres://…/other"` opens a second database once the first
/// connection has landed, through the File menu's own Connect… item, printing
/// what the window holds before and after.
///
/// Exists for the reason `--refresh-after` does: Connect… is reachable only
/// from a menu item, and the form's own Connect button only from a click —
/// synthetic events need accessibility permission this environment does not
/// grant. The two reports are the whole claim: what the window shows after has
/// to be the second database and nothing of the first, and launching straight
/// into that database would show the same window while proving nothing about
/// getting there. Pointed at a connection that fails, it also leaves the form up
/// over a live session, which is the one state a capture cannot otherwise reach.
///
/// What "and nothing of the first" means changed when a window learned to hold
/// several connections. It used to mean the first connection's state had been
/// cleared; it now means the second arrived in a tab of its own, which the first
/// one's state was never in. `--sessions-probe` is what checks the first
/// connection is still there behind it — this one photographs the front.
let reconnectTo = argument("--reconnect")

// `--verify-splitter`, `--verify-connection`, `--verify-completion`,
// `--verify-transaction`, `--verify-editing`, `--verify-clipboard`, `--verify-goto`,
// `--verify-favorites`, `--verify-record`, `--verify-value`,
// `--verify-browse-state`, `--verify-history`, `--verify-progressive`,
// `--verify-filter-rows`,
// `--verify-metadata`,
// `--verify-schema-metadata`, `--verify-import`, `--verify-fk-nav`,
// `--verify-grid-find`,
// `--verify-preferences`,
// `--verify-keep-alive`,
// `--verify-accessibility` and `--verify-quitting` run
// the checks for the pieces of pure logic in the front-end and exit with their
// verdict. None needs a window or a database, so they run before either exists.
if CommandLine.arguments.contains("--verify-splitter") {
    exit(SQLScriptChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-connection") {
    exit(ConnectionChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-completion") {
    exit(SQLCompletionChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-transaction") {
    exit(TransactionChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-editing") {
    exit(EditingChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-clipboard") {
    exit(GridClipboardChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-goto") {
    exit(GoToChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-favorites") {
    exit(QueryFavoritesChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-record") {
    exit(RecordChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-value") {
    exit(ValueViewerChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-browse-state") {
    exit(BrowseStateChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-browse-restore") {
    exit(MainActor.assumeIsolated { BrowseRestoreChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-metadata") {
    exit(MetadataChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-schema-metadata") {
    exit(SchemaMetadataChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-fk-nav") {
    exit(FKNavigationChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-grid-find") {
    exit(MainActor.assumeIsolated { GridFindChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-import") {
    exit(ImportChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-connection-form") {
    exit(MainActor.assumeIsolated { AppModelConnectionChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-history") {
    exit(BrowseHistoryChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-progressive") {
    exit(ProgressiveLoadChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-filter-rows") {
    exit(MainActor.assumeIsolated { FilterRowChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-query-history") {
    exit(QueryHistoryChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-query-buffers") {
    exit(QueryBufferChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-driver-badge") {
    exit(DriverBadgeChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-sidebar") {
    exit(SidebarChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-navigator-groups") {
    exit(NavigatorGroupChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-editor-typing") {
    exit(EditorTypingChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-find-bar") {
    exit(MainActor.assumeIsolated { FindBarChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-keep-alive") {
    exit(MainActor.assumeIsolated { KeepAliveChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-editor-theme") {
    exit(MainActor.assumeIsolated { EditorThemeChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-mcp") {
    exit(MCPChecks.run() ? 0 : 1)
}

// The three that have to state their isolation. `Preferences`, the grid's
// accessibility tree and the sentences put to somebody quitting are main-actor
// isolated because the window reads them, and top-level code runs on the main
// thread without being statically known to.
if CommandLine.arguments.contains("--verify-preferences") {
    exit(MainActor.assumeIsolated { PreferencesChecks.run() } ? 0 : 1)
}

if CommandLine.arguments.contains("--verify-accessibility") {
    exit(MainActor.assumeIsolated { AccessibilityChecks.run() } ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-quitting") {
    exit(MainActor.assumeIsolated { QuittingChecks.run() } ? 0 : 1)
}

/// `--view-image` opens the Query tab on a result holding a picture, so that
/// the value viewer has one to draw.
///
/// A capture flag for the reason `--cell` is one, a step further back: no
/// fixture database has a picture in it, and putting one in would mean a table
/// to create, fill and drop around every screenshot. The picture is drawn here
/// and sent through `decode()`, so what gets photographed is a blob that came
/// back from the server as Arrow binary — the path a real `bytea` takes —
/// rather than bytes handed to the viewer by the process that made them.
///
/// It sets the tab, the statement and the column together, because a picture in
/// a result nobody selected a cell of is a picture of a grid. `--cell` still
/// wins where it is given, which is how a capture reaches the column beside it.
/// PostgreSQL's spelling of a hex literal, that being the connection every
/// capture uses.
let imageStatement: String? =
    CommandLine.arguments.contains("--view-image") ? pictureStatement() : nil

/// `--tab structure|content|query` opens straight to a pane. Screenshots are
/// how rendering defects get caught here, and a screenshot cannot click.
let initialTab =
    imageStatement != nil
    ? DetailTab.query
    : (argument("--tab").flatMap { DetailTab(rawValue: $0.capitalized) } ?? .content)

/// `--sql "SELECT …"` opens on the Query tab with that statement already run.
let initialSQL = imageStatement ?? argument("--sql")

/// `--caret 42` puts the editor's caret at that offset, counted in Unicode
/// scalars from the start of `--sql`.
///
/// ⌘R runs the statement the caret is in, and a capture cannot click into a
/// buffer to move it. Without this there is no way to see a script's third
/// statement run, or to check that a server error position lands in it rather
/// than in the first.
let initialCaret = argument("--caret").flatMap(Int.init)

/// `--offers` prints what the editor would put in its list at `--caret` in
/// `--sql`, and how long each of two identical questions took.
///
/// Exists because the claim this feature has to make is about time, and a
/// screenshot of a popup cannot make it. The two passes are the whole report:
/// the first pays for the metadata the connection had not been told yet, the
/// second is answered from what the core remembered, and the difference between
/// them is the cache doing its job. A single number would not distinguish "fast"
/// from "asked the server nothing because there was nothing to ask".
///
/// No window and no model: this is the FFI, the decode and the ranking, which is
/// everything between a keystroke and a list.
if CommandLine.arguments.contains("--offers") {
    guard let conn = connArgument, let sql = initialSQL else {
        fputs("--offers needs --conn and --sql\n", stderr)
        exit(2)
    }
    let caret = initialCaret ?? sql.unicodeScalars.count
    do {
        let db = try Database(connString: conn)
        for pass in ["cold", "warm"] {
            let started = Date()
            let answer = try db.completions(in: sql, caret: caret)
            let ms = Date().timeIntervalSince(started) * 1000
            fputs(
                String(
                    format: "%@: %d offers in %.1f ms, replacing %d..<%d\n", pass,
                    answer.offers.count, ms, answer.start, answer.end), stderr)
            if pass == "cold" {
                for offer in answer.offers.prefix(8) {
                    fputs("  \(offer.kind.rawValue) \(offer.label) — \(offer.detail)\n", stderr)
                }
            }
        }
        exit(0)
    } catch {
        fputs("offers: \(error)\n", stderr)
        exit(1)
    }
}

/// `--ddl` prints the statements that would recreate `--relation`, which is what
/// the Structure tab's DDL section shows.
///
/// Exists because that section is a wall of text a screenshot cannot be read
/// from, and because the claim it makes is that the text matches DBeaver's for
/// the same object — a claim checked by reading it, against upstream's DDL tab
/// open beside it. The core's own tests pin the string; this prints what the
/// window will actually put on screen.
///
/// No window and no model, like `--offers`: what is under test is the bridge.
if CommandLine.arguments.contains("--ddl") {
    guard let conn = connArgument, let relation = argument("--relation") else {
        fputs("--ddl needs --conn and --relation\n", stderr)
        exit(2)
    }
    // `schema.name` names a schema; a bare name is looked for in the one that
    // opens by default, which is the same reading `--relation` has everywhere.
    let parts = relation.split(separator: ".", maxSplits: 1).map(String.init)
    let (schema, name) = parts.count == 2 ? (parts[0], parts[1]) : ("public", relation)
    do {
        let db = try Database(connString: conn)
        print(try db.ddl(schema: schema, relation: name))
        exit(0)
    } catch {
        fputs("ddl: \(error)\n", stderr)
        exit(1)
    }
}

/// `--edit` puts one cell's worth of change through the whole write path and
/// prints the statements it produced.
///
/// Exists because the shape of the request is the part that can be wrong without
/// anything failing to compile: the fields this side encodes have to be the
/// fields the core decodes, and a name that drifts turns into an edit that is
/// silently empty. The core's tests pin its half; this pins the crossing.
///
/// It writes. `edit_probe` is created and dropped in the database `--conn`
/// names, so point it at a scratch database rather than at anything that
/// matters.
///
/// `--schema` says which schema the request should name, because the request
/// says and the connection cannot be asked: PostgreSQL puts a bare table in
/// `public`, MySQL in the database the connection opened, SQL Server in `dbo`.
/// Getting it wrong is not a silent pass — the core reads that relation's columns
/// to find the key, so a wrong schema fails by name.
if CommandLine.arguments.contains("--edit") {
    guard let conn = connArgument else {
        fputs("--edit needs --conn\n", stderr)
        exit(2)
    }
    let probeSchema = argument("--schema") ?? "public"
    do {
        let db = try Database(connString: conn)
        func ran(_ sql: String) throws -> Int {
            let query = try db.query(sql, batchRows: 1000)
            while try query.nextBatch() != nil {}
            return query.rowsAffected ?? 0
        }
        _ = try ran("DROP TABLE IF EXISTS edit_probe")
        // `varchar` rather than `text`: SQL Server's `text` is the deprecated
        // LOB type and cannot be compared with `=`, which the read-back does.
        _ = try ran(
            "CREATE TABLE edit_probe (id int PRIMARY KEY, label varchar(32), "
                + "qty numeric(9,2) DEFAULT 9.99)")
        _ = try ran("INSERT INTO edit_probe VALUES (1, 'before', 1.00), (2, 'doomed', 2.00)")

        // One request with both arms, because that is what one press of Save
        // sends: a change and a deletion staged together cross as one document,
        // and a field only the second arm carries would go missing there.
        let request = EditRequest(
            schema: probeSchema, relation: "edit_probe",
            updates: [
                EditRequest.Update(
                    key: [EditRequest.Cell(column: "id", value: "1")],
                    set: [
                        EditRequest.Cell(column: "label", value: "after"),
                        EditRequest.Cell(column: "qty", value: "3.25")
                    ])
            ],
            // Two columns of three, because that is what a new row usually is:
            // `qty` is left out so the table's default applies to it, which is
            // the difference between adding a row and dictating every column.
            inserts: [
                EditRequest.Insert(set: [
                    EditRequest.Cell(column: "id", value: "3"),
                    EditRequest.Cell(column: "label", value: "added")
                ])
            ],
            deletes: [EditRequest.Delete(key: [EditRequest.Cell(column: "id", value: "2")])])
        let statements = try db.editStatements(request)
        for sql in statements { print(sql) }
        for sql in statements { _ = try ran(sql) }
        // Read back rather than trusted: a statement that ran is not the same
        // claim as a row that holds what was typed.
        let kept = try ran("SELECT id FROM edit_probe WHERE label = 'after' AND qty = 3.25")
        let gone = try ran("SELECT id FROM edit_probe WHERE id = 2")
        // The default is what proves the column was left out rather than sent
        // empty: a NULL here would mean the insert overrode a schema's defaults.
        let added = try ran("SELECT id FROM edit_probe WHERE id = 3 AND qty = 9.99")
        print("rows matching the edit: \(kept)")
        print("rows left of the deleted one: \(gone)")
        print("rows added with the default: \(added)")
        _ = try ran("DROP TABLE edit_probe")
        exit(kept == 1 && gone == 0 && added == 1 ? 0 : 1)
    } catch {
        fputs("edit: \(error)\n", stderr)
        exit(1)
    }
}

/// `--preferences` drives all three settings through the live window, each way
/// round, and prints what the window did.
///
/// Exists because `--verify-preferences` checks the rules and cannot check the
/// wiring: which side of a switch a behaviour is on is the one mistake here that
/// compiles, passes every unit check, and is invisible until somebody loses a
/// row. So this presses Save and reads the grid, with each setting off and then
/// on, and reports the difference — the pair of lines is the whole claim, and a
/// build ignoring a setting prints the same thing twice.
///
/// It writes. `prefs_probe` is created and dropped in the database `--conn`
/// names, so point it at a scratch database rather than at anything that
/// matters. `--relation prefs_probe` is what opens it, so the two go together;
/// the Makefile target passes both.
///
/// The table is built here, before the window connects, because the navigator
/// reads its inventory once at connect time — a table created after that is one
/// the sidebar has never heard of.
let preferencesProbe = CommandLine.arguments.contains("--preferences")
if preferencesProbe {
    guard let conn = connArgument else {
        fputs("--preferences needs --conn\n", stderr)
        exit(2)
    }
    do {
        let db = try Database(connString: conn)
        func ran(_ sql: String) throws {
            let query = try db.query(sql, batchRows: 1000)
            while try query.nextBatch() != nil {}
        }
        try ran("DROP TABLE IF EXISTS prefs_probe")
        // `gap` is the column this fixture exists for: declared, and null in
        // every row. It stands in for MongoDB's `_extra`, which is the column
        // the hiding setting was asked about — and which needs a MongoDB to
        // produce, while any database at all can produce this.
        //
        // `serial` rather than a plain `int`, which makes this the one probe
        // here that is PostgreSQL-shaped rather than portable: a row of nothing
        // but defaults can only be inserted into a table whose primary key has
        // a default, so a fixture for that setting has to have one. That is the
        // setting's real precondition rather than an accident of this file.
        try ran(
            "CREATE TABLE prefs_probe (id serial PRIMARY KEY, label varchar(32), "
                + "gap varchar(32), note varchar(32) DEFAULT 'from the schema')")
        try ran(
            "INSERT INTO prefs_probe (label, note) VALUES "
                + "('one', 'typed'), ('two', 'typed'), ('three', 'typed')")
    } catch {
        fputs("preferences: could not build the fixture: \(error)\n", stderr)
        exit(1)
    }
}

/// `--history-probe` browses a table, deletes a row from it, and prints what the
/// statement history holds after each.
///
/// Exists for the reason `--preferences` does. `--verify-query-history` checks
/// the store's own rules — the two caps, the replacement, what an entry keeps —
/// and cannot check that anything calls it. Both recordings sit at the far end
/// of the model's background pipeline, which a model with no connection never
/// reaches, so the mistakes left are a call site that is missing, one that fires
/// per page instead of per browse, and one that passes the wrong origin. All
/// three are visible in the two lists this prints and in nothing else.
///
/// It writes. `history_probe` is created and dropped in the database `--conn`
/// names, so point it at a scratch database rather than at anything that
/// matters. `--relation history_probe` is what opens it, so the two go together;
/// the Makefile target passes both, and `--history-store` as well, because a
/// probe has no business writing into somebody's real history.
///
/// The table is built before the window connects, for the reason `--preferences`
/// builds its own there: the navigator reads its inventory once at connect time,
/// and a table created after that is one the sidebar has never heard of.
let historyProbe = CommandLine.arguments.contains("--history-probe")
if historyProbe {
    guard let conn = connArgument else {
        fputs("--history-probe needs --conn\n", stderr)
        exit(2)
    }
    do {
        let db = try Database(connString: conn)
        func ran(_ sql: String) throws {
            let query = try db.query(sql, batchRows: 1000)
            while try query.nextBatch() != nil {}
        }
        try ran("DROP TABLE IF EXISTS history_probe")
        // A primary key, because the Save half needs a row the core can name.
        try ran("CREATE TABLE history_probe (id serial PRIMARY KEY, label varchar(32))")
        try ran("INSERT INTO history_probe (label) VALUES ('one'), ('two'), ('three')")
    } catch {
        fputs("history: could not build the fixture: \(error)\n", stderr)
        exit(1)
    }
}

/// How many rows `--transfer-probe` moves.
///
/// A hundred thousand and not three. What is under test is a transfer polled a
/// batch at a time and stopped part way through, and a source small enough to
/// cross in one fetch — `AppModel` reads 8192 rows at a time — is a transfer
/// with no middle to stop in. Twelve fetches is a middle wide enough that the
/// poll below sees the count move without having to race it.
let transferProbeRows = 100_000

/// `--transfer-probe` fills a table on `--conn` for that transfer to move.
///
/// Built here rather than in the probe, for the reason `--history-probe` builds
/// its own here: the navigator reads its inventory once at connect time, and a
/// table created after that is one the sidebar has never heard of.
let transferProbe = CommandLine.arguments.contains("--transfer-probe")
if transferProbe {
    guard let conn = connArgument else {
        fputs("--transfer-probe needs --conn\n", stderr)
        exit(2)
    }
    do {
        let db = try Database(connString: conn)
        func ran(_ sql: String) throws {
            let query = try db.query(sql, batchRows: 1000)
            while try query.nextBatch() != nil {}
        }
        try ran("DROP TABLE IF EXISTS transfer_probe_src")
        try ran("DROP TABLE IF EXISTS transfer_probe_dst")
        try ran("CREATE TABLE transfer_probe_src (id int PRIMARY KEY, note varchar(32))")
        try ran(
            "INSERT INTO transfer_probe_src SELECT g, 'row ' || g "
                + "FROM generate_series(1, \(transferProbeRows)) AS g")
        // The same shape, and a key of its own: an index to maintain is part of
        // what makes one batch take long enough for Stop to land inside it.
        try ran("CREATE TABLE transfer_probe_dst (id int PRIMARY KEY, note varchar(32))")
    } catch {
        fputs("transfer: could not build the fixture: \(error)\n", stderr)
        exit(1)
    }
}

/// `--sessions-probe` opens a second connection to `--conn` beside the first.
///
/// No fixture, unlike `--history-probe`: it reads nothing but `pg_sleep`, so it
/// writes nothing and leaves nothing behind. Point it at any database that
/// answers.
let sessionsProbe = CommandLine.arguments.contains("--sessions-probe")

/// `--transaction` drives one manual-commit transaction against `--conn` and
/// prints what the connection says at each step.
///
/// Exists because the claim is about what the server was told and not about what
/// the window looks like. `--verify-transaction` checks that this side reads the
/// core's answer correctly; the core's own checks prove the transaction against
/// a server through the C API. This is the piece between them: that the Swift
/// wrapper's six calls reach those entry points and come back with the state
/// they changed.
///
/// It writes. `tx_probe` is created and dropped in the database `--conn` names,
/// so point it at a scratch database rather than at anything that matters.
///
/// No window and no model, like `--offers`: what is under test is the bridge.
if CommandLine.arguments.contains("--transaction") {
    guard let conn = connArgument else {
        fputs("--transaction needs --conn\n", stderr)
        exit(2)
    }
    func report(_ what: String, _ state: TransactionState, rows: Int? = nil) {
        let counted = rows.map { ", \($0) row(s)" } ?? ""
        fputs(
            "\(what): autocommit=\(state.autocommit) open=\(state.open)"
                + " savepoints=\(state.savepoints)\(counted)\n", stderr)
    }
    do {
        let db = try Database(connString: conn)
        /// Runs `sql` to the end and answers with what the server counted, which
        /// for a SELECT is the rows it returned.
        func ran(_ sql: String) throws -> Int {
            let query = try db.query(sql, batchRows: 1000)
            while try query.nextBatch() != nil {}
            return query.rowsAffected ?? 0
        }

        let initial = try db.transactionState()
        guard initial.transactional else {
            fputs("this database has no transaction to control\n", stderr)
            exit(1)
        }
        _ = try ran("DROP TABLE IF EXISTS tx_probe")
        _ = try ran("CREATE TABLE tx_probe (n int)")
        report("connected", initial)

        try db.setAutocommit(false)
        report("manual", try db.transactionState())

        _ = try ran("INSERT INTO tx_probe (n) VALUES (1)")
        report("inserted", try db.transactionState(), rows: try ran("SELECT n FROM tx_probe"))

        try db.savepoint("halfway")
        _ = try ran("INSERT INTO tx_probe (n) VALUES (2)")
        try db.rollback(to: "halfway")
        report(
            "rolled back to savepoint", try db.transactionState(),
            rows: try ran("SELECT n FROM tx_probe"))

        try db.rollback()
        report("rolled back", try db.transactionState(), rows: try ran("SELECT n FROM tx_probe"))

        _ = try ran("INSERT INTO tx_probe (n) VALUES (3)")
        try db.commit()
        report("committed", try db.transactionState(), rows: try ran("SELECT n FROM tx_probe"))

        // The count that came back with the line above ran in manual-commit mode
        // as well, so it opened a transaction of its own — every statement does,
        // including one that changes nothing — and that one has to be ended
        // before the mode can change.
        try db.rollback()
        try db.setAutocommit(true)
        _ = try ran("DROP TABLE tx_probe")
        report("done", try db.transactionState())
        exit(0)
    } catch {
        fputs("transaction: \(error)\n", stderr)
        exit(1)
    }
}

/// `--run-script` runs the whole of `--sql` instead of the statement `--caret`
/// is in, by sending the Query menu's own item.
///
/// Exists for the reason `--refresh-after` does: Run Script is reachable only
/// from a menu item, and a synthetic click needs accessibility permission this
/// environment does not grant. Sending the item rather than calling the model is
/// the point — an item wired to nothing would pass the second check and fail
/// this one. The outcomes are printed as they land and the window is left up,
/// because the thing being verified is what the screen says.
let runScriptMode = CommandLine.arguments.contains("--run-script")

/// `--complete` opens the list of names under the caret once the connection is
/// up, through the Edit menu's own Complete item. Use with `--tab query`,
/// `--sql` and `--caret` to choose where the caret sits.
///
/// Pair it with an `--sql` the server accepts. A statement that fails leaves the
/// editor with the offending token selected — that is what points at a syntax
/// error — and nothing is completed into a selection, so the list would
/// correctly refuse to open.
let completeMode = CommandLine.arguments.contains("--complete")

/// `--where` and `--order` seed the browse filters, for the same reason `--tab`
/// exists: reproducing a particular view without clicking into it.
let initialWhere = argument("--where")
let initialOrder = argument("--order")

/// `--relation bench_wide` opens on a named table instead of the first one.
/// Accepts `schema.name` to reach a schema other than the one that opens by
/// default.
let initialRelation = argument("--relation")

/// `--filter bench` opens with that text already in the navigator's filter
/// field.
///
/// Exists for the reason `--tab` and `--relation` do: the field is reachable
/// only by typing into it, and a screenshot cannot type. Without it the one
/// thing that catches a layout defect in a filtered sidebar — a screenshot of a
/// filtered sidebar — cannot be taken, and neither can the capture that proves
/// a word matching nothing says so rather than going blank.
let initialFilter = argument("--filter")

/// `--load-more 3` browses, then asks for that many further pages, reporting the
/// row count and the paging state after each.
///
/// Exists because paging is a claim about what the *second* page contains, and
/// a screenshot of a grid cannot say whether a row is one it already showed.
/// The counts can: a page that repeats rows and one that continues from them
/// look identical on screen and differ here.
let loadMorePages = argument("--load-more").flatMap(Int.init)

/// `--stop-after 0.5` runs `--sql`, waits that many seconds, and stops it
/// through the Query menu's own Stop item, reporting what the window says
/// before and after.
///
/// Exists because the thing being checked cannot be photographed. A cancellation
/// is a race with the server: the interesting states are "still running" and
/// "stopped", they are half a second apart, and the shutter cannot be told to
/// fire between them. It goes through the menu item rather than calling the
/// model, so validation, target and action are all part of what is checked —
/// which matters more here than elsewhere, because a Stop item that is greyed
/// out at the moment it is needed is indistinguishable from no Stop item at all.
let stopAfter = argument("--stop-after").flatMap(Double.init)

/// `--section triggers` opens the Structure tab on one of its lower sections.
/// Matched loosely so `foreignkeys`, `foreign-keys` and `Foreign keys` all work
/// — this is a capture switch, not a parser.
let initialSection = argument("--section").flatMap { requested in
    let wanted = requested.lowercased().filter { $0.isLetter }
    return StructureDetail.allCases.first {
        $0.rawValue.lowercased().filter(\.isLetter) == wanted
    }
}

/// `--cell json_val:17` selects a cell by column name and 1-based row, and opens
/// the value viewer on it.
///
/// Exists for the reason `--section` does. The viewer is reachable by clicking a
/// cell and then a chevron, or by a menu item's shortcut, and a capture can do
/// neither — synthetic events need accessibility permission this environment
/// does not grant. Without it the one thing that catches a rendering defect in
/// the viewer, a screenshot of the viewer, cannot be taken.
let initialCell = argument("--cell") ?? (imageStatement != nil ? "image_data" : nil)

/// `--edit-value` opens the box on the cell `--cell` chose, instead of the
/// reading pane.
///
/// Exists for the reason `--cell` does, one step further along the same path:
/// the box is reached by clicking a pencil, and a capture cannot click. Without
/// it the one thing that catches a `TextEditor` rendering as a white slab over
/// this theme — a screenshot of the box — cannot be taken. Only useful with
/// `--cell`, which is what selects the cell there is a value to edit.
let editValue = CommandLine.arguments.contains("--edit-value")

/// `--filter-cell` turns the cell `--cell` chose into a filter row, the way the
/// grid's context menu does, and opens the list it lands in.
///
/// Exists for the reason `--edit-value` does. A row is made by right-clicking a
/// cell and choosing an item, or by pressing Add and working two popups, and a
/// capture can do none of it. Without this the only picture of the filter rows
/// is of the shut disclosure, which is a picture of a chevron.
///
/// Only useful with `--cell`, which is what selects the cell the row is about.
let filterOnCell = CommandLine.arguments.contains("--filter-cell")

/// `--inline-edit` opens the editor over the cell `--cell` chose.
///
/// Exists for the reason `--edit-value` does. The editor opens on a double-click
/// or on Return and a capture can send neither — synthetic events need
/// accessibility permission this environment does not grant. Everything this
/// slice is likely to get wrong is a matter of a few points: the field over the
/// wrong row, the characters sitting high or shifted sideways from the cells
/// beside them, a border covering its neighbours. All of it is visible in a
/// screenshot and in nothing else.
///
/// Only useful with `--cell`, which is what selects the cell it opens over.
let inlineEdit = CommandLine.arguments.contains("--inline-edit")

/// `--inline-type x` opens the editor the way typing `x` over the cell does,
/// rather than the way Return does. Exists for the reason `--inline-edit` does:
/// what a capture has to show is that the character replaces the value instead
/// of being appended to it, and that the caret sits after it.
let inlineTyped = argument("--inline-type")

/// `--inline-tab` then sends the editor a Tab, so a capture shows which cell it
/// lands on. The move is the half of this that a screenshot can be wrong about
/// in a way nothing else catches: one column too far, or none at all.
let inlineTab = CommandLine.arguments.contains("--inline-tab")

/// `--delete-row 2` marks that 1-based row of the browse to be deleted, and
/// `--delete-row 2-4` marks a span of them. Nothing is sent: the rows are left
/// crossed out with Save waiting, which is the state this exists to photograph.
///
/// Exists for the reason `--cell` does. The rows are marked by selecting them
/// and pressing a button, and a capture can do neither — synthetic events need
/// accessibility permission this environment does not grant. Without it the one
/// thing that catches a mark drawn in the wrong place, or in a colour that
/// disappears under the selection band, is a screenshot of a marked row.
let deleteRowSpec = argument("--delete-row")

/// `--add-row 2` adds that many rows to the browse and fills nothing in, so a
/// capture can show what a draft row looks like before anything is typed into
/// it. Exists for the reason `--delete-row` does.
let addRowCount = argument("--add-row").flatMap(Int.init)

/// `--history-store dev.dbclient.capture` keeps the query history in a named
/// defaults suite, emptied at launch, instead of the user's own.
///
/// The history is the one thing in this window that outlives the process, which
/// makes it the one thing a capture cannot simply launch into. Reading
/// `UserDefaults.standard` would put whatever this machine last ran into the
/// picture — different on every machine and different on the second run of the
/// same command — and writing there would file a capture's props in somebody's
/// real history.
let historyStore = argument("--history-store")

/// `--history` opens the history panel, through the menu item that owns it.
let showHistory = CommandLine.arguments.contains("--history")

/// `--history-all` turns the panel's *All* checkbox on before the shutter.
///
/// Exists for the reason `--history` does, one control further in: the origin
/// column is drawn only while All is on, so without this there is no way to
/// photograph the half of the panel that browses and edits appear in.
let historyAll = CommandLine.arguments.contains("--history-all")

/// `--rename-buffer` opens the name field on the active query buffer.
///
/// Exists for the reason `--edit-value` does: the field is reached by
/// double-clicking a name and a capture cannot double-click. Everything a text
/// field dropped into a 26pt strip is likely to get wrong is a matter of a few
/// points — no border, the line sitting high against the names beside it, a
/// width that shoves its neighbours along — and all of it is visible only in a
/// picture. Best with `--tab query`, which is the pane the strip is in.
let renameBuffer = CommandLine.arguments.contains("--rename-buffer")

/// `--switch-database archive` moves the tab onto another database, once the
/// first one has finished opening.
///
/// The gesture is a double-click on a row in the tree and a capture cannot
/// double-click, which is the reason every flag in this group exists. This one
/// also reports what it left behind: the tab count, the editor and the tree,
/// before and after — the three things a switch is supposed to move exactly one
/// of.
let switchToDatabase = argument("--switch-database")

/// `--drop-connection` marks the open connection as gone, once it has landed.
///
/// The state cannot be reached from a keyboard or a mouse: it needs a server to
/// go away while the window is looking at it, which is exactly the thing a
/// capture cannot arrange. What it stages is the answer to a ping that did not
/// come back — the same call `probeOpenConnections` makes when the application
/// returns to the front — so the picture is of the tab a person would actually
/// be shown, with the rows still on it.
let dropConnection = CommandLine.arguments.contains("--drop-connection")

/// `--transfer-picker` opens a second connection and puts the target picker up.
///
/// Another state a capture cannot click into, and for a plainer reason than the
/// one above: the menu item is grey until a second connection is open, and
/// nothing on a screenshot run opens one.
let transferPicker = CommandLine.arguments.contains("--transfer-picker")

/// `--collapse-sidebar` opens the window with the objects as a rail.
///
/// Exists for the reason `--rename-buffer` does: the rail is reached through a
/// menu item and a capture cannot open a menu. It is also the only way to see
/// what the column actually does at 44pt — whether the count fits, whether the
/// two glyphs land where the field and the footer button they stand for were.
let collapseSidebar = CommandLine.arguments.contains("--collapse-sidebar")

/// `--filter-objects` runs the View menu's Filter Objects a beat after launch.
///
/// Its own flag rather than part of the one above because the pair is the check:
/// with both, the picture shows whether asking for the filter while the rail is
/// up actually brings the tree back *and* lands the caret in the field. That is
/// two state changes in one turn of the run loop, which is the shape of thing
/// that works in the model and fails on screen.
let filterObjects = CommandLine.arguments.contains("--filter-objects")

/// `--find-bar` opens the editor's find bar once the connection is up, with a
/// term already in it. Exists because the bar appears in answer to ⌘F and a
/// capture cannot type one; the term goes through the find pasteboard — which
/// is where the bar reads it from anyway — so the shot shows matches and a
/// count instead of an empty field.
let showFindBar = CommandLine.arguments.contains("--find-bar")

/// `--settings <pane>` opens the Settings window on the named pane, for the
/// captures that show it. The whole run is pointed at a scratch defaults suite
/// — a capture must not edit the person's real settings — and the MCP pane is
/// captured with its server switched on, because the pane's lower half, the
/// endpoint and token being paired from, exists only while one runs.
let capturePane = argument("--settings").flatMap { name in
    SettingsPane.allCases.first { $0.rawValue.lowercased() == name.lowercased() }
}

/// `--mcp-probe 8791` serves MCP on that port and prints the endpoint and the
/// token to stderr, so a script can talk to the socket.
///
/// Exists because `--verify-mcp` holds every rule this server has and none of
/// them over a wire: parsing, routing, the walls and the dispatcher are pure
/// functions checked as pure functions, which leaves the listener, the
/// connection-per-request read loop and the live data source with no coverage
/// at all. The one thing a check cannot do is open a socket; this is what
/// lets something else do it.
///
/// The port is named rather than defaulted, because a probe must not answer
/// on the port somebody's real client is paired to. The whole run is on a
/// scratch defaults suite for the reason `--settings` is: a probe must not
/// turn on a server in the person's own settings and leave it on.
let mcpProbePort = argument("--mcp-probe").flatMap(Int.init)

/// `--history-pick 2` recalls the nth-newest statement into the editor.
///
/// Both exist for the reason `--cell` does: the panel is opened with a keystroke
/// or a click and a row is chosen with another, and a capture can do neither.
/// Both wait for the run that fills the history, so they are only useful
/// alongside something that runs — `--sql`, with or without `--run-script`.
let historyPick = argument("--history-pick").flatMap(Int.init)

/// `--export out.csv` writes the opened result to a file and exits.
///
/// Exists for the same reason `--tab` and `--relation` do, one step further on:
/// the export is otherwise reachable only through a save panel, and a script
/// cannot click one. Without it there is no way to check what actually lands in
/// a file. The format follows the extension.
let exportPath = argument("--export")

/// `--import <file>` reads a file into the relation `--relation` opened, then
/// exits. The format follows the extension, as it does through the menu.
let importPath = argument("--import")

/// `--map-import <file>` opens the import sheet on a file and leaves it there.
///
/// Exists for the reason `--cell` does: the sheet is reachable by a menu item and
/// an open panel, and a capture can click neither. Unlike `--import` it presses
/// nothing — what is being photographed is the mapping, and a sheet that starts
/// the import would be gone before the shutter.
let mapImportPath = argument("--map-import")

/// Drives `--map-import`. Polls for the reason `openValueViewer` does, and like
/// it does not exit: the window has to stay up for the shutter.
@MainActor
func openImportSheet(model: AppModel, from path: String) {
    let url = URL(fileURLWithPath: path)
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var asked = false

    func poll() {
        if let error = model.errorMessage {
            fputs("import sheet failed: \(error)\n", stderr)
            exit(1)
        }
        if let plan = model.importPlan {
            fputs(
                "import mapping  "
                    + zip(plan.fileColumns, plan.mapping)
                    .map { "\($0) → \($1 ?? "(skipped)")" }
                    .joined(separator: ", ") + "\n", stderr)
            return
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("import sheet timed out waiting for a table\n", stderr)
            exit(1)
        }
        if !asked, model.importTableName != nil {
            asked = true
            model.prepareImport(from: url)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// `--create-table <file>` opens the Create Table sheet on a file and leaves it
/// there.
///
/// The companion to `--map-import`, and for the same reason: the sheet is behind
/// a menu item and an open panel, and a capture can click neither. It presses
/// nothing either — what is being photographed is the statement before it runs.
let createTablePath = argument("--create-table")

/// Drives `--create-table`. Waits for the statement as well as the sheet: an
/// empty pane is what this looks like a fifth of a second before it is worth
/// photographing.
@MainActor
func openCreateTableSheet(model: AppModel, from path: String) {
    let url = URL(fileURLWithPath: path)
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var asked = false

    func poll() {
        if let error = model.errorMessage {
            fputs("create table sheet failed: \(error)\n", stderr)
            exit(1)
        }
        if let statement = model.createPlan?.statement {
            fputs("create table\n\(statement)\n", stderr)
            return
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("create table sheet timed out\n", stderr)
            exit(1)
        }
        if !asked, model.canCreateTableFromFile {
            asked = true
            model.prepareCreateTable(from: url)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// `--fk-jump parent_id` follows the key on that column of the first row and
/// leaves the window on whatever it landed on.
///
/// Exists for the reason `--cell` does: the jump is behind a right-click menu,
/// and a capture cannot open one — a menu is a window of its own and not part of
/// the window being photographed. What is worth a picture is the other end
/// anyway: the table it went to, filtered to the row it named, with the filter
/// list saying so.
let fkJumpColumn = argument("--fk-jump")

/// `--find-in-grid "carol"` opens the find bar over the result and searches for
/// that text, the way ⌘F and then Return do.
///
/// Exists for the reason `--cell` does: the bar opens on a menu command sent to
/// whichever view holds the keyboard, and a capture can neither press ⌘F nor
/// click into a grid — synthetic events need accessibility permission this
/// environment does not grant. Without it the only picture of the bar is of the
/// pane it is not in.
let findText = argument("--find-in-grid")

/// `--find-column name` narrows that search to one column, as the bar's popup
/// does. Only useful with `--find-in-grid`.
let findColumn = argument("--find-column")

/// Drives `--fk-jump`. Polls for the reason `openValueViewer` polls: the rows
/// and the relation's keys arrive through the model's own background pipeline,
/// and a jump needs both.
@MainActor
func followForeignKey(model: AppModel, from column: String) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var jumped = false

    func poll() {
        let result = model.browseResult
        if jumped {
            // The far end, once it has settled. Printed rather than only shown,
            // so a script driving this can say whether the jump landed on rows
            // without reading the picture.
            if result.hasRun, !result.isLoading {
                fputs("fk landed       \(model.statusLine)\n", stderr)
                return
            }
            return again()
        }
        guard result.hasRun, !result.isLoading, result.rowCount > 0,
            let index = result.table.columns.firstIndex(where: { $0.name == column })
        else { return again() }

        // The whole row, the way the menu builds it: NULL columns left out,
        // because a key with a NULL in it references nothing.
        var values: [String: String] = [:]
        for at in result.table.columnNames.indices {
            if let value = result.table.value(row: 0, column: at) {
                values[result.table.columnNames[at]] = value
            }
        }
        // The keys may still be on their way — `loadRelationDetail` is a second
        // round trip after the browse — so an empty answer is waited on rather
        // than reported, until the deadline says otherwise.
        let jumps = model.jumps(atColumn: column, in: values)
        guard let jump = jumps.referenced.first ?? jumps.referencing.first else { return again() }

        // Selected as well as jumped from. The cell the jump came from is what a
        // reader of the picture would look for first, and the grid it was on is
        // about to be replaced.
        result.selection = GridSelection(row: 0, column: index)
        let filter = jump.match
            .map { "\($0.column) = \($0.value ?? "NULL")" }
            .joined(separator: " AND ")
        fputs("fk jump         \(column) → \(jump.label) via \(jump.via)\n", stderr)
        fputs("fk filter       \(filter)\n", stderr)
        model.jump(jump)
        jumped = true
        return again()
    }

    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("nothing on \(column) to follow within the deadline\n", stderr)
            exit(1)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// `--refresh-after 4` reloads the navigator that many seconds in, printing its
/// contents before and after, then exits.
///
/// Exists for the reason `--export` does, one step further on: Refresh is
/// reachable only from a menu item and a sidebar button, and a script can click
/// neither — synthetic events need accessibility permission this environment
/// does not grant. The delay is the window in which DDL gets applied out of
/// band, so the two reports are a before and an after of one running process.
/// That is the whole claim: a second launch would read the new catalogue anyway
/// and prove nothing about whether the client noticed.
let refreshAfter = argument("--refresh-after").flatMap(Double.init)

/// Drives `--export` once the opened result has landed, then exits.
///
/// Polls rather than observing: the result arrives through the model's own
/// background pipeline, there is no completion hook to hang this on, and a
/// capture switch does not justify inventing one. Progress goes to stderr
/// because this process ends in `exit`, and stdout is block-buffered — a
/// `print` here is lost exactly when it is most wanted.
@MainActor
func exportWhenReady(model: AppModel, to path: String) {
    let url = URL(fileURLWithPath: path)
    let format = ExportFormat(pathExtension: url.pathExtension) ?? .csv
    // The whole result, which is what a script asking for a file wants: the
    // grid's cap is a property of a window nobody is looking at here.
    let scope = ExportScope.wholeResult
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var started = false

    func poll() {
        if let error = model.errorMessage {
            fputs("export failed: \(error)\n", stderr)
            exit(1)
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("export timed out waiting for a result\n", stderr)
            exit(1)
        }
        if started {
            if !model.isExporting {
                fputs("export wrote    \(path)\n", stderr)
                exit(0)
            }
        } else if model.canExport {
            started = true
            // The two things the save panel would have shown, printed where a
            // script can assert on them.
            fputs("export name     \(model.exportFilename(format, scope: scope))\n", stderr)
            fputs("export message  \(model.exportMessage)\n", stderr)
            model.exportCurrentResult(to: url, format: format, scope: scope)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--import` once the opened relation has landed, then exits.
///
/// Exists for the reason `--export` does: the import is otherwise reachable only
/// through an open panel, and a script cannot click one. Without it there is no
/// way to check that what the panel would have started actually reaches the
/// table — the layer between the menu and `db_import` is the one part of this
/// path no Rust test can see.
@MainActor
func importWhenReady(model: AppModel, from path: String) {
    let url = URL(fileURLWithPath: path)
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var started = false

    func poll() {
        if let error = model.errorMessage {
            fputs("import failed: \(error)\n", stderr)
            exit(1)
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("import timed out waiting for a table\n", stderr)
            exit(1)
        }
        // The sheet, answered. A capture cannot press its Import button, and the
        // mapping it opens with is the one this is checking reaches the table —
        // so what is pressed here is the same call the button makes, once the
        // plan the sheet would have shown exists.
        if let plan = model.importPlan {
            fputs(
                "import mapping  "
                    + zip(plan.fileColumns, plan.mapping)
                    .map { "\($0) → \($1 ?? "(skipped)")" }
                    .joined(separator: ", ") + "\n", stderr)
            model.startPlannedImport()
        } else if started {
            // `isImporting` and not `isBusy`: the refresh that follows a
            // successful import sets `isBusy` itself, so waiting on that would
            // report whatever sentence the reload had reached.
            if !model.isImporting, !model.isBusy {
                fputs("import result   \(model.importStatus)\n", stderr)
                exit(0)
            }
        } else if let table = model.importTableName {
            started = true
            fputs("import table    \(table)\n", stderr)
            model.prepareImport(from: url)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--find-in-grid`. Polls for the reason `openValueViewer` does: the
/// rows arrive through the model's own background pipeline.
///
/// Every command goes through the Edit menu rather than through the model,
/// because the half of this that can be wrong without a compiler noticing is the
/// responder chain. The four find items share one selector, name no target, and
/// mean four different things through their tags; the grid joins that chain only
/// by implementing the selector, and only reaches it while it holds the
/// keyboard. `sendAction` returning false is exactly that failure, and it is
/// loud — a capture of a window with no find bar in it would otherwise be filed
/// as a picture of the bar.
///
/// The scan is timed because the claim this feature makes is about a search that
/// runs in this process over the rows already here, and "fast enough to press
/// Return on" is a number rather than an opinion.
@MainActor
func findInGrid(model: AppModel, text: String, column: String?) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    let selector = #selector(NSTextView.performFindPanelAction(_:))

    func send(_ action: NSFindPanelAction, _ what: String) {
        guard
            let item = (AppMenu.editMenu().submenu?.items ?? []).first(where: {
                $0.action == selector && $0.tag == Int(action.rawValue)
            })
        else {
            fputs("the Edit menu has no \(what)\n", stderr)
            exit(1)
        }
        guard NSApp.sendAction(selector, to: nil, from: item) else {
            fputs("\(what) reached nothing — the grid does not hold the keyboard\n", stderr)
            exit(1)
        }
    }

    func open() {
        send(.showFindPanel, "Find…")
        guard model.isFindingInGrid else {
            fputs("Find… did not open the bar\n", stderr)
            exit(1)
        }
        fputs("find bar        open\n", stderr)
        // A turn later than the bar, deliberately, and for the reason
        // `--edit-value` waits a turn after its selection. The bar takes thirty
        // points off the grid, and the scroll that brings a match into view is
        // computed from the grid's height: searching in the same turn scrolls
        // against a height that is about to be two rows smaller, and lands the
        // match just under the bottom edge. A person typing into the field pays
        // this delay without noticing it.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
            MainActor.assumeIsolated { search() }
        }
    }

    func search() {
        model.gridFindText = text
        model.gridFindColumn = column
        let grid = model.current.table
        let cells = model.current.rowCount * (column == nil ? grid.columns.count : 1)
        let began = CFAbsoluteTimeGetCurrent()
        // Through the model rather than the menu, and only here: ⌘F has just put
        // the keyboard in the find field, so the menu's Find Next now belongs to
        // that field's editor. Which responder owns it is the thing the bar's own
        // Return exists to sidestep, and this is that Return.
        model.findInGrid()
        let took = CFAbsoluteTimeGetCurrent() - began
        let scope = column.map { " in \($0)" } ?? ""
        fputs(
            "find scan       “\(text)”\(scope) · up to \(AppModel.formatted(cells)) cells · "
                + String(format: "%.2f s\n", took), stderr)
        fputs("find report     \(model.gridFindReport)\n", stderr)
        if let at = model.current.selection {
            fputs("find cursor     row \(at.row + 1) · \(grid.columns[at.column].name)\n", stderr)
        }
    }

    func poll() {
        let result = model.current
        if result.hasRun, !result.isLoading, result.rowCount > 0 {
            open()
            return
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("no rows to search: --find-in-grid needs a result\n", stderr)
            exit(1)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// The statement `--view-image` runs: a picture, hex-encoded into a SELECT.
///
/// Shapes rather than a photograph, and shapes chosen so that the two defects a
/// screenshot is taken to catch announce themselves. A circle distorts visibly
/// when the aspect ratio is wrong, where a photograph only looks slightly odd; a
/// frame drawn one pixel inside the edge disappears when the picture is drawn
/// past its bounds, where a photograph would just be cropped.
func pictureStatement() -> String {
    let width = 320
    let height = 240
    guard
        let canvas = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
        let context = NSGraphicsContext(bitmapImageRep: canvas)
    else {
        fputs("could not draw the picture --view-image sends\n", stderr)
        exit(1)
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    NSColor(calibratedRed: 0.12, green: 0.15, blue: 0.22, alpha: 1).setFill()
    NSRect(x: 0, y: 0, width: width, height: height).fill()
    NSColor(calibratedRed: 0.35, green: 0.72, blue: 0.95, alpha: 1).setFill()
    NSBezierPath(ovalIn: NSRect(x: 90, y: 50, width: 140, height: 140)).fill()
    NSColor.white.setStroke()
    NSBezierPath(
        rect: NSRect(x: 0.5, y: 0.5, width: Double(width) - 1, height: Double(height) - 1)
    ).stroke()
    NSGraphicsContext.restoreGraphicsState()

    guard let png = canvas.representation(using: .png, properties: [:]) else {
        fputs("could not encode the picture --view-image sends\n", stderr)
        exit(1)
    }
    return "SELECT decode('\(ValueRendering.hex(png))', 'hex') AS image_data"
}

/// Drives `--cell`. Polls for the same reason `exportWhenReady` does: the result
/// arrives through the model's own background pipeline and there is no
/// completion hook to hang this on.
///
/// Unlike the other probes this does not exit — the window has to stay up for
/// the shutter — so a failure to find the column has to be loud, or a capture of
/// an unopened viewer would read as the viewer failing to draw.
@MainActor
func openValueViewer(model: AppModel, on spec: String) {
    let parts = spec.split(separator: ":", maxSplits: 1)
    let column = String(parts[0])
    let row = parts.count > 1 ? (Int(parts[1]) ?? 1) : 1
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    /// Opens the viewer the way ⌥⌘V does rather than by setting the flag.
    ///
    /// The flag is one assignment and would prove nothing about the command:
    /// this walks the menu bar for the item, checks validation lets it fire, and
    /// sends its action to its target, which is every link in the chain except
    /// AppKit's own key-equivalent dispatch. A capture cannot press the keys, so
    /// that last link is the only part left to trust.
    func openViewerThroughMenu() {
        let items = NSApp.mainMenu?.items.compactMap(\.submenu).flatMap(\.items) ?? []
        guard
            let item = items.first(where: {
                $0.action == #selector(ValueViewerCommand.toggleValueViewer(_:))
            }), let action = item.action
        else {
            fputs("no value-viewer item in the menu bar\n", stderr)
            exit(1)
        }
        guard (item.target as? NSMenuItemValidation)?.validateMenuItem(item) == true else {
            fputs("the value-viewer item is disabled with a cell selected\n", stderr)
            exit(1)
        }
        fputs("menu item      “\(item.title)”\n", stderr)
        NSApp.sendAction(action, to: item.target, from: item)
    }

    /// Turns the pane into the box for `--edit-value`.
    ///
    /// By the flag rather than through a menu item, unlike the viewer above,
    /// because the box has no menu item: it opens from a button in the strip,
    /// and the whole of what this exists to photograph is what that button
    /// produces.
    ///
    /// Loud for the reason the caller is loud. `ValueEdit` refuses a binary
    /// value and one too long to lay out, and a capture that quietly got the
    /// reading pane instead would be filed as a screenshot of the box — a
    /// picture of the wrong thing is worse than no picture, because nobody
    /// looks at it twice.
    func openEditor() {
        guard let offer = model.editedValue else {
            fputs("no cell to edit: --edit-value needs --cell to have selected one\n", stderr)
            exit(1)
        }
        if let why = offer.refusal {
            fputs("this cell cannot be edited: \(why)\n", stderr)
            exit(1)
        }
        model.isEditingValue = true
        fputs("value editor    open\n", stderr)
    }

    /// Adds the row the cell menu would add, for `--filter-cell`.
    ///
    /// Through `filterByCell` with the request the menu builds, rather than by
    /// appending a rule to the model. What this exists to photograph is the row
    /// that entry point produces, and a rule pushed straight in would skip the
    /// settling that shapes it and the Apply that compiles it — the two things
    /// most likely to be wrong and the two a picture would show.
    ///
    /// Loud for the reason the caller is loud: a capture that quietly filtered
    /// on nothing would be filed as a screenshot of the rows.
    func filterOnSelectedCell() {
        let result = model.current
        guard let selection = result.selection,
            selection.column < result.table.columnNames.count
        else {
            fputs(
                "no cell to filter on: --filter-cell needs --cell to have selected one\n", stderr)
            exit(1)
        }
        let name = result.table.columnNames[selection.column]
        let value = result.table.value(row: selection.row, column: selection.column)
        model.filterByCell(
            CellFilterRequest(column: name, value: value, op: .equals, extend: false))
        fputs("filter row      \(name) = \(value ?? "NULL")\n", stderr)
    }

    /// Opens the editor over the selected cell, for `--inline-edit`.
    ///
    /// Reaches the AppKit view by walking the window, which nothing else here
    /// does. Nothing hands a `GridView` out of SwiftUI, and the editor is a
    /// subview of one — the alternative is a flag on the model that exists only
    /// to be photographed, which is a mode the application would then have.
    ///
    /// The browse grid is the one that can be written to. The window has two.
    func openInlineEditor() {
        func grid(in view: NSView) -> GridView? {
            if let found = view as? GridView, found.offersValueEditing { return found }
            for sub in view.subviews {
                if let found = grid(in: sub) { return found }
            }
            return nil
        }
        guard let content = NSApp.windows.first(where: \.isVisible)?.contentView,
            let found = grid(in: content)
        else {
            fputs("no writable grid in the window: --inline-edit needs a table\n", stderr)
            exit(1)
        }
        found.beginInlineEdit(typing: inlineTyped)
        // Loud for the reason the other probes are loud. A refused edit leaves
        // the grid looking exactly as it does with none open, and a capture of
        // that would be filed as a picture of the editor.
        guard var field = found.subviews.compactMap({ $0 as? InlineCellEditor }).first else {
            fputs("the editor did not open over the selected cell\n", stderr)
            exit(1)
        }
        if inlineTab {
            fputs("inline editor   before tab \(NSStringFromRect(field.frame))\n", stderr)
            // Through the field editor's own command dispatch rather than by
            // calling the grid's move directly, so what is exercised is the path
            // a Tab key actually takes: text view, delegate, commit, move.
            field.currentEditor()?.doCommand(by: #selector(NSResponder.insertTab(_:)))
            guard let moved = found.subviews.compactMap({ $0 as? InlineCellEditor }).first else {
                fputs("tab closed the editor instead of moving it\n", stderr)
                exit(1)
            }
            field = moved
        }
        fputs("inline editor   frame \(NSStringFromRect(field.frame))\n", stderr)
        fputs("inline editor   seed “\(field.stringValue)”\n", stderr)
    }

    func poll() {
        let result = model.current
        if let index = result.table.columns.firstIndex(where: { $0.name == column }),
            result.hasRun, !result.isLoading, result.rowCount > 0
        {
            result.selection = GridSelection(
                row: min(max(0, row - 1), result.rowCount - 1), column: index)
            openViewerThroughMenu()
            fputs("cell selected   \(column) (column \(index)) row \(row)\n", stderr)
            if editValue {
                // A turn later than the selection above, deliberately. Setting
                // the selection is what makes the inspector's `onChange` fire,
                // and that is what ends an edit begun over a different cell;
                // opening the box in the same turn has it closed again before
                // it is ever drawn. Measured rather than reasoned about: the
                // first capture taken with this flag came back showing the
                // reading pane, twice, and came back showing the box with this.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
                    MainActor.assumeIsolated { openEditor() }
                }
            }
            if filterOnCell {
                // Later than the selection for the reason above, and later than
                // `--edit-value` on purpose: this one re-runs the browse, and a
                // filter applied while the first result is still settling would
                // photograph the grid mid-reload.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
                    MainActor.assumeIsolated { filterOnSelectedCell() }
                }
            }
            if inlineEdit {
                // Later still. The field is placed at a rectangle worked out
                // from where the grid is scrolled to, and SwiftUI has to have
                // pushed this selection into the renderer before that rectangle
                // means anything.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
                    MainActor.assumeIsolated { openInlineEditor() }
                }
            }
            return
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("no column named \(column) in the opened result\n", stderr)
            exit(1)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--history-probe`. Polls for the reason `openValueViewer` does: the
/// rows arrive through the model's own background pipeline and there is no
/// completion hook to hang this on.
///
/// Unlike the capture probes it exits — nothing here is being photographed, and
/// the two lists it prints are the whole claim. A build that records nothing
/// prints two empty lists; one that records per page prints a browse entry for
/// every FETCH; one that passes the wrong origin prints the wrong word.
@MainActor
func probeHistory(model: AppModel) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    // Answered yes without anybody being asked. `confirmsDeletions` may be on,
    // and a probe that stopped on a question would hang rather than fail.
    model.confirmDeletion = { _ in true }

    /// The top of the list, which is as much of it as this can be wrong about.
    func report(_ what: String) {
        let lines = model.history.entries.prefix(4).map { entry in
            let origin = entry.origin.rawValue.padding(
                toLength: 7, withPad: " ", startingAt: 0)
            return "  \(origin) \(Int(entry.milliseconds)) ms · \(entry.outcome.label) · "
                + entry.preview
        }
        let body = lines.isEmpty ? "  (nothing)" : lines.joined(separator: "\n")
        fputs("history \(what)\n\(body)\n", stderr)
    }

    /// Drops the fixture, so a database used for this twice is not left holding
    /// a table nobody asked for.
    func tidy() {
        guard let conn = connArgument, let db = try? Database(connString: conn) else { return }
        guard let query = try? db.query("DROP TABLE IF EXISTS history_probe", batchRows: 1)
        else { return }
        while (try? query.nextBatch()) ?? nil != nil {}
    }

    var saved = false
    func poll() {
        let browse = model.browseResult
        if !saved {
            guard browse.hasRun, !browse.isLoading, browse.rowCount > 0, !model.isBusy else {
                return again()
            }
            report("after the browse")
            // The smallest Save there is: one row marked, one DELETE sent.
            browse.selection = GridSelection(row: 0, column: 0)
            model.toggleDeleteSelectedRows()
            model.applyEdits()
            saved = true
            return again()
        }
        // The re-browse that follows a write has to have settled too, or the
        // second list would be read in the gap between the DELETE landing and
        // the SELECT that follows it being recorded.
        guard !model.isBusy, model.staged.count == 0, browse.hasRun, !browse.isLoading
        else { return again() }
        report("after the save")
        tidy()
        exit(0)
    }

    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("history: nothing arrived within the deadline\n", stderr)
            tidy()
            exit(1)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--sessions-probe`: opens a second connection beside the first and
/// proves the two do not touch each other.
///
/// This is the only test of multi-connection there is, and it needs a server for
/// a reason no arrangement of the code could remove. Every claim item 1 makes is
/// about two live handles — that a tab keeps its own editor, that a result lands
/// in the connection that asked for it rather than the one on screen, that
/// closing one leaves the other working — and a model with nothing open has one
/// session, no cursors and no results, so it can be asked none of them. The
/// checks in `--verify-connection-form` pin the rules that decide whether a
/// second tab appears; this pins what happens once it has.
///
/// The middle phase is the load-bearing one. It starts a statement that sleeps
/// on the second connection, switches to the first while that statement is still
/// in the air, and then reports which session's list of steps it arrived in. A
/// build that resolved the target when the answer came back instead of when the
/// question was asked prints the result under `one`, and a person looking at the
/// first connection would have watched another database's rows appear in it.
///
/// It exits, and prints to stderr, for the reasons `probeHistory` does. It
/// writes nothing: `pg_sleep` and `select 1` leave no fixture to clean up.
@MainActor
func probeSessions(model: AppModel) {
    guard let conn = connArgument else {
        fputs("sessions: --sessions-probe needs --conn\n", stderr)
        exit(2)
    }
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    let marker = "select 'kept' as marker"

    /// One line per session plus the pointer, which together are the whole
    /// claim. Printed rather than asserted so that a wrong answer is legible as
    /// a wrong answer rather than as a line number.
    func report(_ what: String) {
        fputs(
            "sessions \(what): \(model.sessions.count) open, showing \(model.activeSession)\n",
            stderr)
        for (index, session) in model.sessions.enumerated() {
            let editor = session.queryBuffers[session.activeQueryBufferIndex].text
            let steps = session.scriptSteps.map(\.summary).joined(separator: " | ")
            fputs(
                "  [\(index)] open=\(session.db != nil)"
                    + " editor=\(editor.isEmpty ? "(empty)" : editor)"
                    + " steps=\(steps.isEmpty ? "(none)" : steps)"
                    + " error=\(session.errorMessage ?? "(none)")\n", stderr)
        }
    }

    func fail(_ why: String) -> Never {
        fputs("sessions FAIL: \(why)\n", stderr)
        report("at the failure")
        exit(1)
    }

    /// A session that has answered and is not in the middle of anything.
    func settled(_ index: Int) -> Bool {
        guard model.sessions.indices.contains(index) else { return false }
        let session = model.sessions[index]
        return session.db != nil && !session.isBusy
    }

    enum Phase { case first, second, away, landed }
    var phase = Phase.first

    func poll() {
        switch phase {
        case .first:
            guard settled(0) else { return again() }
            report("one")
            guard model.sessions.count == 1 else {
                fail("the first connection did not fill the tab that was already there")
            }
            // Typed into the first connection's editor, and never typed again.
            // Everything after this reads it back: a buffer that is shared
            // between connections shows this text under the second one, and a
            // buffer that is thrown away shows nothing under the first.
            model.queryText = marker
            model.connect(using: conn)
            phase = .second
            return again()

        case .second:
            guard model.sessions.count == 2, settled(1), model.activeSession == 1 else {
                return again()
            }
            report("two")
            guard model.sessions[0].queryBuffers[0].text == marker else {
                fail("the first connection's editor did not survive the second opening")
            }
            // Not "is empty": a connection that lands on a table puts a
            // suggested SELECT in its own editor, and that suggestion is the
            // second connection's own. What must not be there is the marker.
            guard model.sessions[1].queryBuffers[0].text != marker else {
                fail("the second connection opened holding the first one's text")
            }
            // A statement that takes long enough to still be running after the
            // switch below. One second rather than a tenth, because what is
            // being timed is a person's hand moving between tabs.
            //
            // The tab has to be the Query one: ⌘R means "run what I am looking
            // at", and on the Content tab that is the browse.
            // `::text` because `pg_sleep` answers `void`, which is not a column
            // type this client reads — the statement has to succeed, or what
            // arrives proves only that failures route correctly.
            model.activeTab = .query
            model.queryText = "select pg_sleep(1)::text, 'landed' as where_it_went"
            model.runCurrentQuery()
            model.selectSession(0)
            report("away")
            guard model.activeSession == 0 else { fail("switching back did not take") }
            guard model.queryText == marker else {
                fail("switching back showed the wrong editor")
            }
            phase = .away
            return again()

        case .away:
            guard !model.sessions[1].scriptSteps.isEmpty else { return again() }
            report("landed")
            guard model.sessions[0].scriptSteps.isEmpty else {
                fail(
                    "the result landed in the connection that was on screen, "
                        + "not the one that asked")
            }
            guard model.sessions[1].scriptSteps[0].result.rowCount == 1 else {
                fail("the statement did not succeed, so where its rows went proves nothing")
            }
            guard model.sessions[0].errorMessage == nil else {
                fail("the connection on screen was given the other one's banner")
            }
            // Closing the one that is not in front. The pointer has to stay on
            // the connection somebody is looking at, which is the case a
            // by-position pointer gets wrong.
            model.closeSession(1)
            phase = .landed
            return again()

        case .landed:
            report("closed")
            guard model.sessions.count == 1, model.activeSession == 0 else {
                fail("closing the second connection left the window pointing somewhere else")
            }
            guard model.sessions[0].db != nil, model.queryText == marker else {
                fail("closing one connection disturbed the other")
            }
            exit(0)
        }
    }

    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fail("nothing arrived within the deadline")
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// How many rows `--import-probe` reads.
///
/// Four times what the transfer probe moves, and the difference is the sheet.
/// An import goes through one now, and presenting or dismissing a sheet runs the
/// main run loop in a mode that does not drain the main queue — so for a couple
/// of hundred milliseconds either side of the button, the model's published
/// counts do not reach a poll that lives on that queue. A file that arrives
/// inside that window is one this probe watches with its eyes shut: no progress
/// line to sample, and no middle left for Stop to land in.
let importProbeRows = 400_000

/// `--import-probe` reads a file into a table on `--conn`, twice: once to the
/// end, and once with Stop pressed part way.
let importProbe = CommandLine.arguments.contains("--import-probe")

/// Drives `--import-probe`. Reads a generated CSV into a table on `--conn`,
/// watching the count climb, and then does it again and stops it part way.
///
/// One connection, unlike the transfer probe: an import has a file at one end,
/// and the file is not a thing the window holds a handle to. What is under test
/// is the same three claims — a handle that can be stepped, a running total the
/// status bar publishes between batches, and a Stop that reaches a job already
/// in flight — with the reading end replaced.
///
/// It writes, and it drops what it wrote. Point `--conn` at a scratch database.
@MainActor
func probeImport(model: AppModel) {
    guard let conn = connArgument else {
        fputs("import: --import-probe needs --conn\n", stderr)
        exit(2)
    }
    let deadline = CFAbsoluteTimeGetCurrent() + 300
    let table = "import_probe"
    let path = FileManager.default.temporaryDirectory
        .appendingPathComponent("import-probe-\(ProcessInfo.processInfo.processIdentifier).csv")
    /// The table the window makes for itself, and the small file it makes it
    /// from. Separate from the pair above: what the first three phases are about
    /// is a table nobody wrote a `CREATE TABLE` for, and the rest of the probe
    /// needs a table whose columns are deliberately in the wrong order.
    let made = "create_probe"
    let madeFrom = FileManager.default.temporaryDirectory
        .appendingPathComponent("create-probe-\(ProcessInfo.processInfo.processIdentifier).csv")

    /// Runs `sql` on a connection of its own, for the reason the transfer probe
    /// opens one: the window's connection is the one doing the importing, and a
    /// probe that borrowed it would be measuring its own interference.
    func ran(_ sql: String) -> Int? {
        guard let db = try? Database(connString: conn),
            let query = try? db.query(sql, batchRows: 1)
        else { return nil }
        while (try? query.nextBatch()) ?? nil != nil {}
        return query.rowsAffected
    }

    /// Drops a fixture table on a connection of its own, waiting a bounded time
    /// for the lock.
    ///
    /// A browse is a server-side cursor inside an open transaction, so the window
    /// holds a lock on every table it has shown until that cursor is closed.
    /// `SET lock_timeout` — on the same connection, which is why this does not go
    /// through `ran` — turns an indefinite wait into an error: a probe that hangs
    /// on its own cleanup is a `make test-import` that never returns, which is
    /// worse than a table left behind and a line saying so.
    func dropped(_ relation: String) {
        guard let db = try? Database(connString: conn) else {
            fputs("import: nothing to drop \(relation) with\n", stderr)
            return
        }
        do {
            let limit = try db.query("SET lock_timeout = '20s'", batchRows: 1)
            while try limit.nextBatch() != nil {}
            let drop = try db.query("DROP TABLE IF EXISTS \(relation)", batchRows: 1)
            while try drop.nextBatch() != nil {}
        } catch {
            fputs("import: \(relation) was left behind: \(error)\n", stderr)
        }
    }

    func fail(_ why: String) -> Never {
        fputs("import FAIL: \(why)\n", stderr)
        fputs("  status=\(model.statusLine) error=\(model.errorMessage ?? "(none)")\n", stderr)
        exit(1)
    }

    /// Empties the table and reports what was in it — a DELETE rather than a
    /// count for the reason the transfer probe gives.
    func emptied(_ what: String, from relation: String = table) -> Int {
        guard let rows = ran("DELETE FROM \(relation)") else {
            fail("could not read \(relation) back")
        }
        fputs("import \(what): \(rows) rows had arrived\n", stderr)
        return rows
    }

    func settled() -> Bool {
        guard let first = model.sessions.first else { return false }
        return first.db != nil && !first.isBusy
    }

    /// Every distinct progress line this run has shown. An import that reported
    /// only at the end would leave one.
    var progress: Set<String> = []

    enum Phase {
        case creating, naming, renaming, filling, filled
        case building, planning, reading, restarting, replanning, stopping, stopped
    }
    var phase = Phase.creating

    func poll() {
        switch phase {
        case .creating:
            guard settled() else { return again() }
            guard ran("DROP TABLE IF EXISTS \(made)") != nil else {
                fail("could not clear \(made)")
            }
            // Four columns and four inferred types, with a blank row to say the
            // columns came out nullable. The second row is empty in three of the
            // four, which is what a table built with NOT NULL anywhere would
            // refuse half way through the import that created it.
            let body = """
                id,note,seen_at,ratio
                1,hello,2026-08-24 09:08:19,2.5
                2,,,

                """
            do {
                try body.write(to: madeFrom, atomically: true, encoding: .utf8)
            } catch {
                fail("could not write the file to make a table from: \(error)")
            }
            guard model.canCreateTableFromFile else {
                fail("a live connection cannot make a table from a file")
            }
            model.prepareCreateTable(from: madeFrom)
            phase = .naming
            return again()

        case .naming:
            // The statement is waited for rather than assumed: it is written by
            // the connection, and until it has answered there is nothing to run.
            guard let plan = model.createPlan else {
                guard model.errorMessage == nil else {
                    fail("no table could be written for the file: \(model.errorMessage ?? "")")
                }
                return again()
            }
            guard let statement = plan.statement else { return again() }
            // The name the file's own is turned into, before it is replaced with
            // one this probe can drop afterwards. A temporary file's stem carries
            // hyphens, and a hyphen in an unquoted name is a subtraction.
            guard plan.name.hasPrefix("create_probe_") else {
                fail("the file's name was not made into a name a table can have: \(plan.name)")
            }
            guard plan.name != made else { fail("the probe is not renaming anything") }
            guard statement.contains("public.\(plan.name)") else {
                fail("the statement does not name the table on the sheet: \(statement)")
            }
            // Renamed, which re-asks the connection. The name is typed on this
            // side and the statement is written on the other, so for a moment
            // there is no statement at all — which is the point of the phase
            // below: `startPlannedCreate` before it arrives must run nothing.
            model.setCreateTableTarget(name: made)
            guard model.createPlan?.statement == nil else {
                fail("a renamed table already had a statement, so nothing was re-asked")
            }
            model.startPlannedCreate()
            guard model.createPlan != nil, !model.isBusy else {
                fail("Create ran a statement written for the previous name")
            }
            phase = .renaming
            return again()

        case .renaming:
            guard let renamed = model.createPlan?.statement else {
                guard model.errorMessage == nil else {
                    fail("the renamed table could not be written: \(model.errorMessage ?? "")")
                }
                return again()
            }
            guard renamed.contains("CREATE TABLE public.\(made) (") else {
                fail("the statement is not about the table the sheet now names:\n\(renamed)")
            }
            // The four kinds a delimited file is read as, in PostgreSQL's words.
            for word in ["id bigint", "note text", "seen_at timestamp", "ratio double precision"] {
                guard renamed.contains(word) else {
                    fail("the statement is missing `\(word)`:\n\(renamed)")
                }
            }
            fputs("import made:\n\(renamed)\n", stderr)
            model.startPlannedCreate()
            phase = .filling
            return again()

        case .filling:
            // The import sheet, opened by the create rather than by a menu. This
            // is the whole of the chain: a table nobody typed, offered the file
            // it was shaped from.
            guard let plan = model.importPlan else {
                guard model.errorMessage == nil else {
                    fail("the table was not made: \(model.errorMessage ?? "")")
                }
                return again()
            }
            guard plan.table == "public.\(made)" else {
                fail("the file was offered to \(plan.table) rather than to the table just made")
            }
            guard plan.mapping == ["id", "note", "seen_at", "ratio"] else {
                fail(
                    "a table made from this file does not match it column for column: \(plan.mapping)"
                )
            }
            guard model.selected?.name == made else {
                fail(
                    "the window is not showing the table it just made: \(model.selected?.name ?? "nothing")"
                )
            }
            guard
                model.sessions[0].relations["public"]?.contains(where: { $0.name == made }) == true
            else {
                fail("the navigator has not heard of the table that was just made")
            }
            model.startPlannedImport()
            phase = .filled
            return again()

        case .filled:
            guard !model.isImporting, settled() else { return again() }
            guard model.errorMessage == nil else {
                fail(
                    "the file would not go into the table made for it: \(model.errorMessage ?? "")")
            }
            // Deleted by a predicate the inferred types have to be right for: a
            // `timestamp` compared against a timestamp and a `double precision`
            // against a number. Had every column come out as text, this matches
            // nothing and the count below is 2 rather than 1.
            guard
                let typed = ran(
                    "DELETE FROM \(made) WHERE id = 1 AND ratio = 2.5 "
                        + "AND seen_at = timestamp '2026-08-24 09:08:19'"), typed == 1
            else {
                fail("the values did not land in columns of the types that were inferred")
            }
            let rest = emptied("into the table it made", from: made)
            guard rest == 1 else {
                fail("the second row of the file did not arrive: \(rest) rows were left")
            }
            // Not dropped here. The window is browsing this table, and a browse
            // is a server-side cursor inside an open transaction — a DROP from
            // another connection waits on that lock until the cursor is closed,
            // which is what selecting a different table does. It is dropped at
            // the end, several tables later.
            try? FileManager.default.removeItem(at: madeFrom)
            phase = .building
            return again()

        case .building:
            guard settled() else { return again() }
            guard ran("DROP TABLE IF EXISTS \(table)") != nil,
                ran("CREATE TABLE \(table) (id int, note varchar(32))") != nil
            else {
                fail("could not build the fixture")
            }
            // Written in the other order from the table's columns, on purpose.
            // Read by position this file puts "row 1" in an integer column and
            // fails at the first batch, so an import that lands is one where the
            // mapping crossed the boundary and was honoured — which is not
            // something the sheet's own state can show.
            var body = "note,id\n"
            body.reserveCapacity(importProbeRows * 16)
            for n in 1...importProbeRows {
                body += "row \(n),\(n)\n"
            }
            do {
                try body.write(to: path, atomically: true, encoding: .utf8)
            } catch {
                fail("could not write the file: \(error)")
            }
            // Selected rather than navigated to. The navigator read its
            // inventory at connect time and has never heard of this table; what
            // the import needs from the selection is the name it writes into.
            model.sessions[0].selected = RelationInfo(
                schema: "public", name: table, kind: .table, estimatedRows: nil)
            model.activeTab = .content
            guard model.canImport else { fail("a freshly built table is not importable into") }
            model.prepareImport(from: path)
            phase = .planning
            return again()

        case .planning:
            // The sheet's own state, checked rather than skipped past: the
            // columns of this file are named for the table's, so a mapping that
            // came back with anything unmapped would mean the default matching
            // never ran.
            guard let plan = model.importPlan else {
                guard model.errorMessage == nil else {
                    fail("the file could not be read: \(model.errorMessage ?? "")")
                }
                return again()
            }
            guard plan.mapping == ["note", "id"] else {
                fail("the columns were not matched by name: \(plan.mapping)")
            }
            model.startPlannedImport()
            guard model.isImporting else {
                fail("the import did not start: \(model.errorMessage ?? "no reason given")")
            }
            phase = .reading
            return again()

        case .reading:
            if model.isImporting {
                progress.insert(model.importStatus)
                // The other half of the claim: a running total the status bar
                // outranks is a window that still looks stuck.
                guard model.statusLine == model.importStatus else {
                    fail("the status bar is showing \(model.statusLine), not the import")
                }
                return again()
            }
            guard settled() else { return again() }
            guard progress.count > 1 else {
                fail("the import published no running total: \(progress.sorted())")
            }
            guard !model.importStatus.hasPrefix("Stopped") else {
                fail("an import nobody stopped reported itself stopped: \(model.importStatus)")
            }
            fputs("import finished: \(model.importStatus)\n", stderr)
            let read = emptied("after the whole file")
            guard read == importProbeRows else {
                fail("\(importProbeRows) rows were in the file and \(read) arrived")
            }
            phase = .restarting
            return again()

        case .restarting:
            guard settled(), model.canImport else { return again() }
            progress.removeAll()
            model.prepareImport(from: path)
            phase = .replanning
            return again()

        case .replanning:
            guard model.importPlan != nil else { return again() }
            model.startPlannedImport()
            guard model.isImporting else {
                fail("the second import did not start: \(model.errorMessage ?? "no reason given")")
            }
            phase = .stopping
            return again()

        case .stopping:
            // Stopped as soon as there is a handle to stop it by. What had
            // already arrived stays in the table, so the count this ends on is
            // deliberately not pinned to a number.
            guard model.sessions[0].importHandle != nil else { return again() }
            model.stopImport()
            phase = .stopped
            return again()

        case .stopped:
            guard !model.isImporting, settled() else { return again() }
            guard model.importStatus.hasPrefix("Stopped") else {
                fail("the window did not say it had been stopped: \(model.importStatus)")
            }
            guard model.errorMessage == nil else {
                fail("Stop was reported as a failure: \(model.errorMessage ?? "")")
            }
            let partial = emptied("after Stop")
            guard partial < importProbeRows else {
                fail("Stop stopped nothing: all \(partial) rows were read")
            }
            fputs("import stopped: \(model.importStatus)\n", stderr)
            // The connection first. Both tables below have been browsed, and a
            // browse holds its cursor — and the lock under it — until something
            // closes it. This is what closes it.
            model.closeSession(0)
            dropped(table)
            dropped(made)
            try? FileManager.default.removeItem(at: path)
            exit(0)
        }
    }

    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fail("nothing arrived within the deadline, waiting in \(phase)")
        }
        // Twenty milliseconds, for the reason the transfer probe polls that
        // often: a sampler slower than the batches it samples reads one progress
        // line and concludes there was only ever one.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.02) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--transfer-probe`. Moves a table's rows from the connection in the
/// first tab into a table on the connection in the second, twice: once to the
/// end, and once with Stop pressed part way.
///
/// Two live connections, for the reason `--sessions-probe` needs them. A
/// transfer is one connection reading while another writes, and every rule about
/// which handle does which is invisible to a check holding one handle. That both
/// ends are PostgreSQL is not the interesting part and is not meant to be: what
/// is under test is the polled handle, the running total the window publishes
/// between batches, and whether Stop reaches a transfer already in flight.
///
/// It writes, and it drops what it wrote. Point `--conn` at a scratch database.
@MainActor
func probeTransfer(model: AppModel) {
    guard let conn = connArgument else {
        fputs("transfer: --transfer-probe needs --conn\n", stderr)
        exit(2)
    }
    let deadline = CFAbsoluteTimeGetCurrent() + 300
    let target = "transfer_probe_dst"
    // Answered yes without anybody being asked, as `--history-probe` does with
    // deletions: a probe that stopped on a modal would hang rather than fail.
    model.confirmProductionRun = { _ in true }

    /// Runs `sql` on a connection of its own and answers what the server
    /// counted, or nil if the statement could not be sent at all.
    ///
    /// Its own connection because both of the window's are spoken for — one is
    /// reading the source and the other is being written into — and a probe that
    /// borrowed either would be measuring its own interference.
    func ran(_ sql: String) -> Int? {
        guard let db = try? Database(connString: conn),
            let query = try? db.query(sql, batchRows: 1)
        else { return nil }
        while (try? query.nextBatch()) ?? nil != nil {}
        return query.rowsAffected
    }

    /// Empties the target and reports what was in it.
    ///
    /// A DELETE rather than a `count(*)` because it answers the question and
    /// clears the table for the next run in one statement — and because the
    /// count of what it removed comes back as a number rather than as an Arrow
    /// batch this would then have to decode.
    func emptied(_ what: String) -> Int {
        guard let rows = ran("DELETE FROM \(target)") else {
            fputs("transfer FAIL: could not read the target back\n", stderr)
            exit(1)
        }
        fputs("transfer \(what): \(rows) rows had arrived\n", stderr)
        return rows
    }

    /// Drops the fixture, after closing the window's connections.
    ///
    /// The closing is not tidiness, it is what makes the drop possible: a
    /// transfer reads its source through a server-side cursor, and a cursor open
    /// on a table holds a lock that `DROP TABLE` waits behind for as long as the
    /// connection lives. Closing the tabs releases both handles; a probe that
    /// dropped first would hang here instead of reporting what it found.
    func tidy() {
        while model.sessions.contains(where: { $0.db != nil }) {
            model.closeSession(0)
        }
        _ = ran("DROP TABLE IF EXISTS transfer_probe_src")
        _ = ran("DROP TABLE IF EXISTS \(target)")
    }

    /// Leaves the fixture where it is, unlike every other probe here. A failure
    /// can arrive with a transfer still in flight, and the tables it is reading
    /// cannot be dropped until that lets go — so a tidy on this path is a hang
    /// instead of a report. The next run drops them before it builds them.
    func fail(_ why: String) -> Never {
        fputs("transfer FAIL: \(why)\n", stderr)
        fputs(
            "  status=\(model.statusLine) error=\(model.errorMessage ?? "(none)")\n", stderr)
        exit(1)
    }

    /// A session that has answered and is not in the middle of anything.
    func settled(_ index: Int) -> Bool {
        guard model.sessions.indices.contains(index) else { return false }
        return model.sessions[index].db != nil && !model.sessions[index].isBusy
    }

    /// Every distinct progress line this run has shown, which is how the claim
    /// that the count moves *between* batches is checked: a transfer that
    /// reported only at the end would leave one line here.
    var progress: Set<String> = []

    enum Phase { case first, second, ran, moving, stopping, stopped }
    var phase = Phase.first

    func poll() {
        switch phase {
        case .first:
            guard settled(0), model.sessions.count == 1 else { return again() }
            model.connect(using: conn)
            phase = .second
            return again()

        case .second:
            guard model.sessions.count == 2, settled(1), model.activeSession == 1 else {
                return again()
            }
            // Back to the tab the rows are read from: what the model forwards
            // goes to whichever connection is in front, and the transfer is
            // asked for by the source.
            model.selectSession(0)
            model.activeTab = .query
            model.queryText = "select id, note from transfer_probe_src order by id"
            model.runCurrentQuery()
            phase = .ran
            return again()

        case .ran:
            // The target has to have settled too, and settling once is not
            // enough: a connection that has just answered goes back to work on
            // the relation it landed on, and a transfer will not send rows into
            // a tab that is in the middle of something.
            guard settled(0), settled(1), model.current.rowCount > 0 else { return again() }
            guard model.canTransfer else {
                let other = model.sessions[1]
                fail(
                    "a result of \(model.current.rowCount) rows with a second connection "
                        + "open is not transferable: statement="
                        + (model.current.statement.isEmpty ? "(empty)" : "kept")
                        + " targets=\(model.transferTargets.count)"
                        + " other=[open=\(other.db != nil) busy=\(other.isBusy)"
                        + " transferring=\(other.isTransferring)]")
            }
            model.transferCurrentResult(to: model.sessions[1], table: target)
            guard model.isTransferring else {
                fail("the transfer did not start: \(model.errorMessage ?? "no reason given")")
            }
            phase = .moving
            return again()

        case .moving:
            if model.isTransferring {
                progress.insert(model.transferStatus)
                // The status bar is the other half of the claim. A window that
                // shows nothing until the end is what the polled handle exists
                // to replace, and a progress line the status bar outranks is
                // the same thing with extra steps.
                guard model.statusLine == model.transferStatus else {
                    fail("the status bar is showing \(model.statusLine), not the transfer")
                }
                guard model.sessions[1].isBusy else {
                    fail("the target is taking rows and is not marked busy")
                }
                return again()
            }
            let moved = emptied("after the whole table")
            guard moved == transferProbeRows else {
                fail("\(transferProbeRows) rows were sent and \(moved) arrived")
            }
            guard !model.sessions[1].isBusy else { fail("the target was left busy") }
            guard progress.count > 1 else {
                fail("the transfer published no running total: \(progress.sorted())")
            }
            guard !model.status.hasPrefix("Stopped") else {
                fail("a transfer nobody stopped reported itself stopped: \(model.status)")
            }
            fputs("transfer finished: \(model.status)\n", stderr)

            progress.removeAll()
            model.transferCurrentResult(to: model.sessions[1], table: target)
            guard model.isTransferring else {
                fail(
                    "the second transfer did not start: "
                        + (model.errorMessage ?? "no reason given"))
            }
            phase = .stopping
            return again()

        case .stopping:
            // Stopped as soon as there is a handle to stop it by. Whatever had
            // already gone across stays there — that is what `Step::Stopped`
            // promises and the one thing a transfer cannot take back — so the
            // count this ends on is deliberately not pinned to a number.
            guard model.sessions[0].transferHandle != nil else { return again() }
            model.stopTransfer()
            phase = .stopped
            return again()

        case .stopped:
            guard !model.isTransferring else { return again() }
            let partial = emptied("after Stop")
            guard partial < transferProbeRows else {
                fail("Stop stopped nothing: all \(partial) rows went across")
            }
            guard model.status.hasPrefix("Stopped") else {
                fail("the window did not say it had been stopped: \(model.status)")
            }
            guard model.errorMessage == nil else {
                fail("Stop was reported as a failure: \(model.errorMessage ?? "")")
            }
            fputs("transfer stopped: \(model.status)\n", stderr)
            tidy()
            exit(0)
        }
    }

    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            let tabs = model.sessions.map { "open=\($0.db != nil) busy=\($0.isBusy)" }
            fail("nothing arrived within the deadline, waiting in \(phase): \(tabs)")
        }
        // Twenty milliseconds rather than the fifty every other probe here
        // polls at: one batch of a hundred thousand rows crosses in about
        // forty, and a sampler slower than the thing it samples would read one
        // progress line and conclude there was only ever one.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.02) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--delete-row`. Polls for the reason `openValueViewer` does: the rows
/// arrive through the model's own background pipeline and there is no completion
/// hook to hang this on.
///
/// Like that one it does not exit — the window has to stay up for the shutter —
/// so a span that names rows the result does not have has to be loud, or a
/// capture of an unmarked grid would read as the mark failing to draw.
@MainActor
func markRowsForDeletion(model: AppModel, spec: String) {
    let ends = spec.split(separator: "-", maxSplits: 1).compactMap { Int($0) }
    guard let first = ends.first, first >= 1 else {
        fputs("--delete-row counts from 1, and takes 2 or 2-4\n", stderr)
        exit(1)
    }
    let last = ends.count > 1 ? ends[1] : first
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    func poll() {
        let result = model.browseResult
        guard result.hasRun, !result.isLoading, !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("delete probe timed out waiting for the rows\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        guard last <= result.rowCount else {
            fputs(
                "--delete-row \(spec) names rows this result does not have "
                    + "(\(result.rowCount) fetched)\n", stderr)
            exit(1)
        }
        // Selected the way a user selects a span — cursor at one end, anchor at
        // the other — because that is what the command reads.
        result.selection = GridSelection(row: last - 1, column: 0, anchor: first - 1)
        model.toggleDeleteSelectedRows()
        guard model.hasPendingEdits else {
            fputs("the rows were selected and the mark did not take\n", stderr)
            exit(1)
        }
        fputs(
            "rows marked     \(first)…\(last) · \(model.deleteRowsTitle ?? "(no button)")\n", stderr
        )
    }
    poll()
}

/// Drives `--add-row`. Polls, and stays up rather than exiting, for the reasons
/// `markRowsForDeletion` does.
@MainActor
func addRows(model: AppModel, count: Int) {
    guard count >= 1 else {
        fputs("--add-row takes a count of 1 or more\n", stderr)
        exit(1)
    }
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    func poll() {
        guard model.browseResult.hasRun, !model.browseResult.isLoading, !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("add-row probe timed out waiting for the rows\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        guard model.canAddRow else {
            fputs("this result cannot take a new row; nothing to add\n", stderr)
            exit(1)
        }
        for _ in 0..<count { model.addDraftRow() }
        guard model.draftRows.count == count else {
            fputs(
                "asked for \(count) new rows and the grid holds \(model.draftRows.count)\n", stderr)
            exit(1)
        }
        fputs("rows added      \(count) · \(model.browseRowCount) drawn\n", stderr)
    }
    poll()
}

/// Drives `--preferences`: each of the three settings, off and then on, through
/// the window that reads them.
///
/// Polls between steps for the reason `markRowsForDeletion` does — a Save goes
/// out through the model's own background queue and comes back through a re-read
/// — and every step waits for the window to be idle before it acts, so a report
/// never describes a result that is still arriving.
///
/// Each pair of lines is the claim. A build that read a setting from the wrong
/// place, or wired a behaviour to the wrong side of one, prints the same thing
/// for both halves of a pair; only the differences here are evidence.
@MainActor
func probePreferences(model: AppModel) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    /// Whether Save put the question, and what it was answered.
    var asked: DeleteConfirmation?
    var answer = false
    model.confirmDeletion = { confirmation in
        asked = confirmation
        return answer
    }

    func report(_ what: String, _ said: String) {
        fputs("prefs  \(what.padding(toLength: 16, withPad: " ", startingAt: 0))\(said)\n", stderr)
    }

    /// The columns the grid is being told not to draw, by name — an index would
    /// say nothing about whether the right column was chosen.
    func hiddenNames() -> String {
        let names = model.hiddenBrowseColumns.sorted().map {
            model.browseResult.table.columns[$0].name
        }
        return names.isEmpty ? "(none)" : names.joined(separator: ", ")
    }

    /// Marks the nth row of the browse, counting from one.
    func mark(row: Int) {
        model.browseResult.selection = GridSelection(row: row - 1, column: 0)
        model.toggleDeleteSelectedRows()
    }

    /// One Save, described by what it was asked and what it left behind.
    ///
    /// The staged count matters as much as the row count: a Save answered "no"
    /// has to leave the marks alone, or saying no once would throw away the
    /// marking the user spent a minute doing.
    func saved(_ what: String) {
        let question = asked == nil ? "not asked" : "asked"
        let refusal = model.errorMessage.map { " · refused: \($0)" } ?? ""
        report(
            what,
            "\(question) · \(model.browseResult.rowCount) rows left · "
                + "\(model.staged.count) still staged\(refusal)")
        asked = nil
    }

    var steps: [() -> Void] = []
    var next = 0

    /// Where the cursor actually is, by column name. Read through the model's
    /// own clamp, which is what the grid and the inspector strip both read.
    func cursorName() -> String {
        guard let at = model.browseSelection?.column,
            at < model.browseResult.table.columns.count
        else { return "(none)" }
        return model.browseResult.table.columns[at].name
    }

    // Through the menu item rather than by calling the window's own opener, for
    // the reason `--reconnect` goes through Connect…: an item wired to nothing
    // would pass every check on the settings themselves and still leave them
    // unreachable. A preference that can only be changed with `defaults write`
    // is not a setting, it is a hidden key.
    steps.append {
        guard
            let item = NSApp.mainMenu?.items.first?.submenu?
                .items.first(where: { $0.title == "Settings…" })
        else {
            fputs("no Settings… item in the application menu\n", stderr)
            exit(1)
        }
        guard let action = item.action, item.target != nil else {
            fputs("the Settings… item has no target to send to\n", stderr)
            exit(1)
        }
        NSApp.sendAction(action, to: item.target, from: item)
        guard let panel = NSApp.windows.first(where: { $0.title == "Settings" }) else {
            fputs("the Settings… item did not open a window\n", stderr)
            exit(1)
        }
        report(
            "settings", "“\(item.title)” ⌘\(item.keyEquivalent) · \(Int(panel.frame.width))pt wide")
        // Closed again, so the shots and the steps below are of the session
        // window rather than of a panel sitting over it.
        panel.close()
    }

    // The evidence is gathered whichever way the setting is set, so both answers
    // below come from the one browse that has already happened.
    steps.append {
        model.preferences.hidesEmptyColumns = false
        // Parked on the empty column on purpose, so that turning the setting on
        // has a cursor to move. Found by reading the grid rather than by asking
        // the model which column it hid: aiming with the answer under test would
        // make the next line agree with itself.
        let grid = model.browseResult.table
        if let empty = grid.columns.indices.first(where: { column in
            grid.rowCount > 0
                && (0..<grid.rowCount).allSatisfy { grid.isNull(row: $0, column: column) }
        }) {
            model.browseResult.selection = GridSelection(row: 0, column: empty)
        }
        report("hide off", "\(hiddenNames()) · cursor \(cursorName())")
    }
    // Its own step rather than a second line in the one above, so a turn of the
    // run loop passes with the setting on: the grid then genuinely redraws
    // without the column, rather than only being told to and told back again
    // before it ever laid out.
    steps.append {
        model.preferences.hidesEmptyColumns = true
    }
    steps.append {
        report("hide on", "\(hiddenNames()) · cursor \(cursorName())")
        // Left off, so the rows below are addressed by the coordinates the grid
        // draws them at rather than by a shifted set.
        model.preferences.hidesEmptyColumns = false
    }

    steps.append {
        model.preferences.confirmsDeletions = false
        mark(row: 1)
        model.applyEdits()
    }
    steps.append { saved("delete off") }

    steps.append {
        model.preferences.confirmsDeletions = true
        answer = false
        mark(row: 1)
        model.applyEdits()
    }
    steps.append { saved("delete on, no") }

    // The same mark, answered the other way. Deliberately not re-marked: saying
    // no leaves the row staged, so pressing Save again is the whole of what a
    // user does next, and re-marking here would silently unmark it instead.
    steps.append {
        answer = true
        model.applyEdits()
    }
    steps.append { saved("delete on, yes") }

    steps.append {
        model.preferences.insertsRowOfDefaults = false
        model.addDraftRow()
        model.applyEdits()
    }
    steps.append { saved("empty row off") }

    steps.append {
        model.errorMessage = nil
        model.preferences.insertsRowOfDefaults = true
        model.applyEdits()
    }
    steps.append { saved("empty row on") }

    // A browse reads through a cursor, and a cursor is a transaction that stays
    // open for as long as somebody is looking at the rows — so `DROP TABLE
    // prefs_probe` from anywhere would wait on this window until it closed.
    // Moving the browse to another relation is what lets go of it; nothing else
    // reachable from here does. Smallest first, because the point is to stop
    // reading this table rather than to read another one.
    steps.append {
        let elsewhere =
            model.relations.values.flatMap { $0 }
            .filter { $0.name != "prefs_probe" }
            .min { ($0.estimatedRows ?? .max) < ($1.estimatedRows ?? .max) }
        guard let elsewhere else {
            fputs("preferences: no other relation to move the browse to\n", stderr)
            exit(1)
        }
        model.selected = elsewhere
        report("parked on", "\(elsewhere.schema).\(elsewhere.name)")
    }

    func finish() {
        // Read back through a connection of its own, because the claim is about
        // what is in the table rather than about what the window is showing:
        // a row of pure defaults has to have taken the schema's default, or the
        // statement inserted a row of NULLs and merely looked right.
        var defaulted = -1
        do {
            let db = try Database(connString: connArgument ?? "")
            func ran(_ sql: String) throws -> Int {
                let query = try db.query(sql, batchRows: 1000)
                while try query.nextBatch() != nil {}
                return query.rowsAffected ?? 0
            }
            defaulted = try ran(
                "SELECT id FROM prefs_probe WHERE note = 'from the schema' AND label IS NULL")
            _ = try ran("DROP TABLE prefs_probe")
        } catch {
            fputs("preferences: reading back failed: \(error)\n", stderr)
            exit(1)
        }
        report("defaulted rows", "\(defaulted)")
        exit(defaulted == 1 ? 0 : 1)
    }

    func pump() {
        guard model.browseResult.hasRun, !model.browseResult.isLoading, !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("preferences probe timed out waiting for the window\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(pump)
            }
            return
        }
        guard next < steps.count else {
            finish()
            return
        }
        let step = steps[next]
        next += 1
        step()
        // A step that only reports leaves the window idle, so the next one would
        // run in the same turn; a beat between them keeps each report on the
        // state its own step produced.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(pump)
        }
    }
    pump()
}

/// The SQL editor's text view, wherever SwiftUI put it.
@MainActor
func firstEditorTextView(_ view: NSView) -> EditorTextView? {
    if let editor = view as? EditorTextView { return editor }
    for sub in view.subviews {
        if let found = firstEditorTextView(sub) { return found }
    }
    return nil
}

/// Drives `--find-bar`: opens the bar over the editor the way ⌘F does, once
/// the connection is up. Waits for the connection although the bar itself
/// needs none, because the capture that wants the bar wants the window behind
/// it populated too.
@MainActor
func showFindBarWhenReady(model: AppModel) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    func poll() {
        guard !model.schemas.isEmpty, !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("find-bar probe timed out waiting for a connection\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        // Focused by hand for the reason `completeWhenReady` gives: on the
        // unattended machine a capture runs on, SwiftUI's focus never lands.
        let window = NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first(where: \.isVisible)
        guard let editor = window?.contentView.flatMap(firstEditorTextView) else {
            fputs("no editor text view to open the find bar over\n", stderr)
            exit(1)
        }
        window?.makeFirstResponder(editor)
        let board = NSPasteboard(name: .find)
        board.clearContents()
        board.setString("select", forType: .string)
        // The menu item's shape without the menu: the action reads the tag.
        let command = NSMenuItem()
        command.tag = Int(NSFindPanelAction.showFindPanel.rawValue)
        editor.performFindPanelAction(command)
    }
    poll()
}

/// Drives `--complete`: opens the list of names under the editor's caret, the
/// way ⌥⎋ does, once the connection is up.
///
/// Exists because the list appears in answer to a keystroke and a capture cannot
/// type one — synthetic events need accessibility permission this environment
/// does not grant. Without it the one thing that catches a popup drawn in the
/// wrong place, or behind the window, or empty, is a screenshot of the popup,
/// and there is no way to take one.
///
/// It goes through the menu item rather than reaching into the editor, so that
/// what is checked includes the item being there, being enabled, and being wired
/// to something. The window is left up, because the thing being verified is what
/// is on the screen.
@MainActor
func completeWhenReady(model: AppModel) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    var lastReport = CFAbsoluteTimeGetCurrent()

    func poll() {
        // The connection has landed once the navigator has something in it,
        // which is also when the core has a catalogue to complete from. Waiting
        // for the statement `--sql` ran as well: completion is skipped while the
        // one connection is busy, so firing during the run would leave nothing
        // on screen and nothing to say why.
        guard !model.schemas.isEmpty, !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("completion probe timed out waiting for a connection\n", stderr)
                exit(1)
            }
            // Said out loud rather than waited out in silence: a probe that
            // prints nothing for three minutes and then fails looks the same
            // whether the database is slow or the connection was refused, and
            // the window it would be read from is the one being photographed.
            if CFAbsoluteTimeGetCurrent() - lastReport > 2 {
                lastReport = CFAbsoluteTimeGetCurrent()
                fputs(
                    "waiting: \(model.schemas.count) schemas, "
                        + "busy \(model.isBusy), \(model.status)"
                        + (model.connectionError.map { " — \($0)" } ?? "") + "\n", stderr)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        let items = NSApp.mainMenu?.items.compactMap(\.submenu).flatMap(\.items) ?? []
        guard let item = items.first(where: { $0.action == #selector(NSResponder.complete(_:)) })
        else {
            fputs("no Complete item in the menu bar\n", stderr)
            exit(1)
        }
        // Sent to the focused view rather than through `sendAction(to: nil)`,
        // which dispatches down the *key* window's responder chain — and a
        // machine running this unattended, with the display asleep, has no key
        // window at all. What is being checked is the same either way: the item
        // carries the standard command, and whatever the editor puts in the
        // responder chain answers it.
        let window = NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first(where: \.isVisible)
        // Focused first, which a user does by clicking into the editor. An
        // application that is not the active one has no key window and SwiftUI's
        // focus never lands, so on an unattended machine — where a capture is
        // taken — the responder chain would otherwise start and end at the
        // window itself.
        if let editor = window?.contentView.flatMap(firstEditorTextView) {
            window?.makeFirstResponder(editor)
        }
        guard let responder = window?.firstResponder else {
            fputs("no first responder to complete in\n", stderr)
            exit(1)
        }
        fputs("menu item      “\(item.title)” → \(type(of: responder))\n", stderr)
        responder.tryToPerform(item.action!, with: item)
        report(within: CFAbsoluteTimeGetCurrent() + 5)
    }

    /// Says where the list landed, once it has.
    ///
    /// The answer comes back from the core on a background queue, so the list
    /// does not exist yet at the moment the command is sent. Reported rather
    /// than assumed: on a machine where a screenshot cannot be taken this line
    /// is the only evidence that anything appeared, and its frame is what says
    /// the list is under the caret rather than off the screen.
    func report(within deadline: CFAbsoluteTime) {
        if let panel = NSApp.windows.first(where: {
            $0.identifier == CompletionPopup.identifier && $0.isVisible
        }) {
            let f = panel.frame
            fputs(
                String(
                    format: "list shown    at %.0f,%.0f  %.0f×%.0f\n", f.minX, f.minY, f.width,
                    f.height), stderr)
            return
        }
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("no list appeared\n", stderr)
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated { report(within: deadline) }
        }
    }
    poll()
}

/// Drives `--refresh-after`. Polls, and reports to stderr, for the reasons
/// `exportWhenReady` does: there is no completion hook on the model's
/// background pipeline, and this process ends in `exit`, which loses stdout.
@MainActor
func refreshWhenReady(model: AppModel, after seconds: Double) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    func report(_ phase: String) {
        // Padded so the two reports line up in a terminal; the whole point of
        // printing twice is that the difference is read by eye.
        let tag = phase.padding(toLength: 6, withPad: " ", startingAt: 0)
        let objects = model.visibleSchemas
            .flatMap { model.relations[$0.name] ?? [] }
            .map(\.id).sorted()
        fputs("\(tag) objects  \(objects.joined(separator: ", "))\n", stderr)
        fputs(
            "\(tag) selected \(model.selected?.id ?? "(none)") · "
                + "\(AppModel.pluralized(model.columns.count, "column")) · "
                + "\(AppModel.pluralized(model.indexes.count, "index", "indexes")) · "
                + "\(AppModel.pluralized(model.triggers.count, "trigger"))\n", stderr)
        fputs("\(tag) expanded \(model.expanded.sorted().joined(separator: ", "))\n", stderr)
        // The Query tab's rows, which a refresh must leave alone. Reported
        // because the tempting wrong implementation — reconnecting — would
        // silently take them, and nothing else here would notice.
        fputs("\(tag) query    \(AppModel.pluralized(model.queryResult.rowCount, "row"))\n", stderr)
        fputs("\(tag) status   \(model.statusLine)\n", stderr)
        fputs("\(tag) filters  where=\(model.whereClause) order=\(model.orderClause)\n", stderr)
        fputs("\(tag) message  \(model.errorMessage ?? "(none)")\n", stderr)
    }

    /// Waits for the browse to land, then gives the Structure sections a beat.
    ///
    /// `isBusy` alone is not the signal: the model is briefly idle between the
    /// connection landing and the first browse being dispatched, and a report
    /// from inside that gap describes a window nobody ever sees. The extra beat
    /// is for the detail load, which rides behind the browse on the serial core
    /// queue deliberately and so has no completion of its own to wait on —
    /// giving it one would mean a second busy flag for the same state.
    func whenSettled(_ next: @escaping @MainActor () -> Void) {
        func poll() {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("refresh probe timed out waiting for the pane to settle\n", stderr)
                exit(1)
            }
            guard model.browseResult.hasRun, !model.isBusy, !model.browseResult.isLoading
            else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                    MainActor.assumeIsolated(poll)
                }
                return
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                MainActor.assumeIsolated(next)
            }
        }
        poll()
    }

    whenSettled {
        report("before")
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) {
            MainActor.assumeIsolated {
                model.refresh()
                whenSettled {
                    report("after")
                    exit(0)
                }
            }
        }
    }
}

/// Drives `--load-more`. Polls, and reports to stderr, for the reasons
/// `exportWhenReady` does.
@MainActor
func loadMoreWhenReady(model: AppModel, pages: Int) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    func report(_ tag: String) {
        fputs(
            "\(tag.padding(toLength: 7, withPad: " ", startingAt: 0)) "
                + "rows \(AppModel.formatted(model.browseResult.rowCount))"
                + " · capped \(model.browseResult.capped)"
                + " · more \(model.canLoadMore)"
                + " · obstacle \(model.pagingObstacle?.label ?? "(none)")"
                + " · generation \(model.browseResult.generation)\n", stderr)
    }

    func settled(_ next: @escaping @MainActor () -> Void) {
        func poll() {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("load-more probe timed out waiting for the pane to settle\n", stderr)
                exit(1)
            }
            // A failure with no browse behind it. The window shows this in its
            // error banner and goes quiet, so waiting out the deadline reports
            // "did not settle" for something that settled immediately and
            // badly. Found the hard way: a trigger the front end could not
            // decode took the browse down with it, and this probe spent three
            // minutes not saying so.
            if let failure = model.errorMessage, !model.browseResult.hasRun, !model.isBusy {
                fputs("nothing to browse: \(failure)\n", stderr)
                exit(1)
            }
            // The navigator has loaded and landed on nothing: no browse is
            // coming, and the three-minute deadline would only report that very
            // slowly. `connectionState` is what says the loading is over —
            // without it this fires in the moment between launch and the first
            // metadata call, when nothing is selected because nothing has
            // happened yet. Which of the two ways it happened is worth saying,
            // because they are fixed differently: a database with no tables in
            // it is the connection, a `--relation` that matched nothing is the
            // argument.
            if model.connectionState == .connected, model.selected == nil, !model.isBusy,
                !model.browseResult.isLoading
            {
                let empty = model.relations.values.allSatisfy(\.isEmpty)
                fputs(
                    empty
                        ? "nothing to browse: this connection reports no relations\n"
                        : "nothing to browse: no relation is selected — check --relation\n",
                    stderr)
                exit(1)
            }
            guard model.browseResult.hasRun, !model.isBusy, !model.browseResult.isLoading
            else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                    MainActor.assumeIsolated(poll)
                }
                return
            }
            next()
        }
        poll()
    }

    func page(_ remaining: Int) {
        guard remaining > 0 else { exit(0) }
        guard model.canLoadMore else {
            // Not a failure: a relation smaller than one page has nothing more
            // to give, and saying so beats reporting a page that never happened.
            fputs("no further page to ask for\n", stderr)
            exit(0)
        }
        model.loadMore()
        settled {
            report("page \(pages - remaining + 1)")
            page(remaining - 1)
        }
    }

    settled {
        report("first")
        page(pages)
    }
}

/// Drives `--run-script`. Polls, and reports to stderr, for the reasons
/// `exportWhenReady` does: the model's background pipeline has no completion
/// hook, and a capture switch does not justify inventing one.
@MainActor
func runScriptWhenReady(model: AppModel) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    /// The Query menu's Run Script item, found the way a user finds it.
    func menuItem() -> NSMenuItem? {
        NSApp.mainMenu?.items
            .compactMap(\.submenu)
            .first { $0.title == "Query" }?
            .items.first { $0.title == "Run Script" }
    }

    func poll() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("script probe timed out\n", stderr)
            exit(1)
        }
        guard let item = menuItem() else {
            fputs("script probe found no Run Script item in the Query menu\n", stderr)
            exit(1)
        }
        // The item's own target is asked, which is what AppKit asks when the
        // menu opens. `NSApp.validateMenuItem` answers for the application and
        // says yes to anything, so a probe that consulted it would fire the
        // command mid-connection and then wait forever for a run the disabled
        // command never started.
        guard let validator = item.target as? NSMenuItemValidation,
            validator.validateMenuItem(item), let action = item.action
        else {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        fputs("script item     \(item.title) · enabled\n", stderr)
        NSApp.sendAction(action, to: item.target, from: item)
        report()
    }

    func report() {
        func settled() {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("script probe timed out waiting for the run\n", stderr)
                exit(1)
            }
            guard !model.isBusy, !model.scriptSteps.isEmpty else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                    MainActor.assumeIsolated(settled)
                }
                return
            }
            for step in model.scriptSteps {
                fputs("script step \(step.id)   \(step.summary) · \(step.preview)\n", stderr)
            }
            fputs("script showing  \(model.selectedStep + 1)\n", stderr)
            fputs("script status   \(model.statusLine)\n", stderr)
            fputs("script message  \(model.errorMessage ?? "(none)")\n", stderr)
        }
        settled()
    }

    poll()
}

/// Drives `--switch-database`: waits for the first database to be open, reports
/// the window, moves the tab, and reports it again.
///
/// Polls rather than hooks, for the reason `reconnectWhenReady` gives: the
/// model's background pipeline has no completion hook and this process ends in
/// `exit`, which loses stdout.
@MainActor
func switchDatabaseWhenReady(model: AppModel, to name: String) {
    let deadline = CFAbsoluteTimeGetCurrent() + 120

    func report(_ phase: String) {
        let tag = phase.padding(toLength: 6, withPad: " ", startingAt: 0)
        let objects = model.visibleSchemas
            .flatMap { model.relations[$0.name] ?? [] }
            .map(\.id).sorted()
        let current = (model.databases ?? []).first(where: \.isCurrent)?.name ?? "(none)"
        fputs("\(tag) tabs     \(model.sessions.count)\n", stderr)
        fputs("\(tag) database \(current)\n", stderr)
        fputs("\(tag) objects  \(objects.joined(separator: ", "))\n", stderr)
        fputs("\(tag) editor   \(model.queryText.isEmpty ? "(empty)" : model.queryText)\n", stderr)
        fputs("\(tag) message  \(model.errorMessage ?? "(none)")\n", stderr)
    }

    func whenSettled(_ next: @escaping @MainActor () -> Void) {
        func poll() {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("switch probe timed out waiting for a session\n", stderr)
                exit(1)
            }
            let browsed =
                model.selected == nil || model.errorMessage != nil
                || (model.browseResult.hasRun && !model.browseResult.isLoading)
            guard !model.isShowingConnectionForm, !model.isBusy, browsed else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                    MainActor.assumeIsolated(poll)
                }
                return
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                MainActor.assumeIsolated(next)
            }
        }
        poll()
    }

    /// The switch, once the list of databases has the name in it.
    ///
    /// The refresh is not padding: on DuckDB the only way a second database
    /// exists is an `ATTACH` sent on this connection, so a capture that opens a
    /// file and attaches beside it — the one arrangement this flag is useful for
    /// — has a list read before the database it is aiming at was there. A person
    /// hits Refresh at that point, and so does this, once.
    func attempt(afterRefreshing refresh: Bool) {
        report(refresh ? "before" : "again")
        if model.canSwitchDatabase(to: name) {
            model.switchDatabase(to: name)
            whenSettled { report("after") }
            return
        }
        guard refresh, model.canRefresh else {
            fputs("switch  refused  \(name) is not somewhere this tab can go\n", stderr)
            return
        }
        model.refresh()
        whenSettled { attempt(afterRefreshing: false) }
    }

    whenSettled { attempt(afterRefreshing: true) }
}

/// Drives `--filter-objects`: the View menu's item, sent once the tree is there
/// to be filtered.
///
/// Deferred rather than sent at launch for the reason `--rename-buffer` records
/// — a field cannot take focus before the pane holding it is laid out — and
/// here also because the connection is still opening: the command is greyed out
/// until a schema has arrived, and this is the state a person would use it in.
@MainActor
func filterObjectsWhenReady(model: AppModel) {
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
        MainActor.assumeIsolated {
            model.focusNavigatorFilter()
            fputs("filter objects  rail: \(model.isSidebarCollapsed)\n", stderr)
        }
    }
}

/// Drives `--drop-connection`: waits for the connection to land, then tells the
/// window what a failed ping tells it.
///
/// Deferred rather than hooked, for the reason the flags around it are: the
/// model's opening pipeline has no completion hook, and marking a session that
/// has not opened yet would be marking the form.
@MainActor
func dropConnectionWhenReady(model: AppModel) {
    DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) {
        MainActor.assumeIsolated {
            let session = model.sessions[model.activeSession]
            model.recordHealth(false, of: session)
            fputs("drop  dot     \(session.connectionState.label)\n", stderr)
            fputs("drop  status  \(model.status)\n", stderr)
        }
    }
}

/// Drives `--transfer-picker`: opens a second connection beside the first and
/// puts the target picker up over the result the window is showing.
///
/// A capture flag rather than a probe. Whether the rules behind the sheet are
/// right is what `--verify-connection-form` asks and `--transfer-probe` proves
/// against two servers; whether the sentence at the top of it fits beside a menu
/// of connection names is a question only the shutter can answer.
///
/// Both connections are the one `--conn` names. Two tabs on the same server is
/// a real thing to have open — it is how a table is copied between schemas —
/// and it means the flag needs nothing a screenshot run does not already have.
@MainActor
func openTransferPicker(model: AppModel) {
    guard let conn = connArgument else {
        fputs("--transfer-picker needs --conn\n", stderr)
        exit(2)
    }
    let deadline = CFAbsoluteTimeGetCurrent() + 60
    var asked = false

    func settled(_ index: Int) -> Bool {
        guard model.sessions.indices.contains(index) else { return false }
        return model.sessions[index].db != nil && !model.sessions[index].isBusy
    }

    func poll() {
        // Rows as well as a settled connection, because the rows are what is
        // being sent: `--relation` lands the first tab on a table and the browse
        // that follows is not what `isBusy` describes, so a picker opened as
        // soon as the connection answered would find nothing to transfer.
        let rows = model.sessions.first?.browseResult
        guard settled(0), let rows, rows.rowCount > 0, !rows.isLoading else { return again() }
        guard asked else {
            model.connect(using: conn)
            asked = true
            return again()
        }
        guard model.sessions.count == 2, settled(1) else { return again() }
        // The rows are read from the tab that was there first, so that is the
        // tab the picker has to be opened over.
        model.selectSession(0)
        model.presentTransfer()
        // The row count and the statement as well as the answer, because a
        // picker that does not open is nearly always a result that is not
        // there — and a capture of a window with no sheet in it has no other
        // way to say which of the two it is.
        fputs(
            "transfer picker: open=\(model.isTransferPickerOpen) "
                + "targets=\(model.transferTargets.count) rows=\(model.current.rowCount) "
                + "statement=\(model.current.statement.isEmpty ? "(empty)" : "kept")\n", stderr)
    }

    /// No exit on the deadline, unlike the probes: the window is being
    /// photographed, and a run that gave up would be photographed too. It says
    /// so instead, which is what a capture with no sheet in it needs to explain
    /// itself.
    func again() {
        if CFAbsoluteTimeGetCurrent() > deadline {
            fputs("transfer picker: neither connection settled in time\n", stderr)
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            MainActor.assumeIsolated(poll)
        }
    }
    poll()
}

/// Drives `--rename-buffer`: puts the Query pane up and opens the name field on
/// the buffer the editor is in.
///
/// Deferred by a turn, and then some, for the reason `--edit-value` records: the
/// field seeds itself and takes focus from a `.task`, and asking for it in the
/// same turn as the pane it lives in has it drawn before the pane is laid out.
@MainActor
func openBufferNameField(model: AppModel) {
    model.activeTab = .query
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
        MainActor.assumeIsolated {
            guard model.queryBuffers.indices.contains(model.activeQueryBufferIndex) else {
                fputs("no buffer to rename\n", stderr)
                return
            }
            model.renamingQueryBuffer = model.queryBuffers[model.activeQueryBufferIndex].id
            fputs(
                "rename field   on “\(model.queryBuffers[model.activeQueryBufferIndex].name)”\n",
                stderr)
        }
    }
}

/// Drives `--history` and `--history-pick`. Polls, and reports to stderr, for
/// the reasons `exportWhenReady` does: the model's background pipeline has no
/// completion hook, and a capture switch does not justify inventing one.
///
/// Unlike the other probes this does not exit on success — the window has to
/// stay up for the shutter — so waiting for a history that never fills has to be
/// loud, or a capture of an empty panel would read as the panel failing to draw.
@MainActor
func driveHistory(model: AppModel, open: Bool, all: Bool, pick: Int?) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180
    // Counted from 1, like the list it indexes and like every other ordinal in
    // this window. Rejected rather than clamped: a probe that quietly picked a
    // different row than it was asked for would put the wrong statement in a
    // screenshot taken to prove which statement came back.
    if let pick, pick < 1 {
        fputs("--history-pick counts from 1\n", stderr)
        exit(1)
    }
    let wanted = pick ?? 1

    /// Opens the panel the way ⇧⌘H does rather than by setting the flag, for the
    /// reason `openValueViewer` walks the menu: the flag is one assignment and
    /// would prove nothing about the command behind it.
    func openThroughMenu() {
        let items = NSApp.mainMenu?.items.compactMap(\.submenu).flatMap(\.items) ?? []
        guard
            let item = items.first(where: {
                $0.action == #selector(QueryHistoryCommand.showQueryHistory(_:))
            }), let action = item.action
        else {
            fputs("no query-history item in the menu bar\n", stderr)
            exit(1)
        }
        guard (item.target as? NSMenuItemValidation)?.validateMenuItem(item) == true else {
            fputs("the query-history item is disabled on the Query tab\n", stderr)
            exit(1)
        }
        fputs("history item    “\(item.title)”\n", stderr)
        NSApp.sendAction(action, to: item.target, from: item)
    }

    func poll() {
        guard !model.isBusy, model.history.entries.count >= wanted else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs(
                    "history probe timed out waiting for \(wanted) recorded "
                        + "statement(s); the history holds "
                        + "\(model.history.entries.count)\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(poll)
            }
            return
        }
        for (i, entry) in model.history.entries.enumerated() {
            fputs("history \(i + 1)       \(entry.outcome.label) · \(entry.preview)\n", stderr)
        }
        if open { openThroughMenu() }
        // Set rather than clicked, unlike the panel itself. A checkbox in a
        // SwiftUI header has no menu item behind it to send, and what is being
        // proved here is what the list draws, not what the toggle is wired to.
        if all { model.showsAllStatements = true }
        if let pick {
            let entry = model.history.entries[pick - 1]
            model.recall(entry)
            fputs("history recalled \(entry.preview)\n", stderr)
        }
    }
    poll()
}

/// Drives `--stop-after`. Reports to stderr for the reasons `exportWhenReady`
/// does: this process ends in `exit`, which loses stdout.
@MainActor
func stopWhenRunning(model: AppModel, after seconds: Double) {
    func report(_ phase: String) {
        let tag = phase.padding(toLength: 6, withPad: " ", startingAt: 0)
        fputs("\(tag) busy     \(model.isBusy)\n", stderr)
        fputs("\(tag) status   \(model.statusLine)\n", stderr)
        // The banner is the point of the whole exercise: the server reports a
        // cancellation as an error, and this is where that would show up as one.
        fputs("\(tag) banner   \(model.errorMessage ?? "(none)")\n", stderr)
        fputs(
            "\(tag) steps    "
                + model.scriptSteps.map { "\($0.id):\($0.outcome.label)" }
                .joined(separator: ", ") + "\n", stderr)
        fputs(
            "\(tag) history  "
                + model.history.entries.map(\.outcome.label).joined(separator: ", ") + "\n",
            stderr)
    }

    // The cancel is a round trip of its own, and the statement it stops reports
    // through the same queue as everything else. Polled rather than slept on, so
    // a cancellation that never arrives fails loudly instead of being reported
    // as one that did.
    let deadline = CFAbsoluteTimeGetCurrent() + 30
    func settle() {
        guard !model.isBusy else {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("the statement was still running 30s after Stop\n", stderr)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                MainActor.assumeIsolated(settle)
            }
            return
        }
        report("after")
        exit(0)
    }

    func stopThroughMenu() {
        guard model.isBusy else {
            fputs("nothing was running \(seconds)s in; nothing to stop\n", stderr)
            exit(1)
        }
        report("before")

        guard
            let item = NSApp.mainMenu?.items.compactMap(\.submenu).flatMap(\.items)
                .first(where: { $0.action == #selector(StopCommand.stopRunningStatement(_:)) })
        else {
            fputs("no Stop item in the menu bar\n", stderr)
            exit(1)
        }
        guard (item.target as? NSMenuItemValidation)?.validateMenuItem(item) == true,
            let action = item.action
        else {
            fputs("the Stop item is disabled while a statement is running\n", stderr)
            exit(1)
        }
        fputs("menu   item     “\(item.title)” · enabled\n", stderr)
        NSApp.sendAction(action, to: item.target, from: item)
        settle()
    }

    DispatchQueue.main.asyncAfter(deadline: .now() + seconds) {
        MainActor.assumeIsolated(stopThroughMenu)
    }
}

/// Drives `--reconnect`. Polls, and reports to stderr, for the reasons
/// `exportWhenReady` does: the model's background pipeline has no completion
/// hook, and this process ends in `exit`, which loses stdout.
@MainActor
func reconnectWhenReady(model: AppModel, to connString: String) {
    let deadline = CFAbsoluteTimeGetCurrent() + 180

    func report(_ phase: String) {
        // Padded so the two reports line up in a terminal; the whole point of
        // printing twice is that the difference is read by eye.
        let tag = phase.padding(toLength: 6, withPad: " ", startingAt: 0)
        let objects = model.visibleSchemas
            .flatMap { model.relations[$0.name] ?? [] }
            .map(\.id).sorted()
        fputs("\(tag) label    \(model.connectionLabel)\n", stderr)
        fputs("\(tag) objects  \(objects.joined(separator: ", "))\n", stderr)
        fputs("\(tag) selected \(model.selected?.id ?? "(none)")\n", stderr)
        fputs(
            "\(tag) browse   \(AppModel.pluralized(model.browseResult.rowCount, "row"))\n", stderr)
        // The editor, the filters and the banner are the state most likely to be
        // carried across by accident: none of them is rebuilt from the new
        // catalogue, so nothing else would notice them surviving.
        fputs("\(tag) editor   \(model.queryText.isEmpty ? "(empty)" : model.queryText)\n", stderr)
        fputs("\(tag) filters  where=\(model.whereClause) order=\(model.orderClause)\n", stderr)
        fputs("\(tag) message  \(model.errorMessage ?? "(none)")\n", stderr)
    }

    /// Waits for a session to be up and done working.
    ///
    /// `isBusy` alone is not the signal: the model is briefly idle between the
    /// connection landing and the first browse being dispatched, and a report
    /// from inside that gap describes a window nobody ever sees. A database
    /// with nothing to browse never starts one at all, which is why the
    /// selection decides which of the two is being waited for — and a browse
    /// that failed never runs either, so a reported failure counts as settled
    /// rather than as something still to wait for.
    func whenSettled(_ next: @escaping @MainActor () -> Void) {
        func poll() {
            if CFAbsoluteTimeGetCurrent() > deadline {
                fputs("reconnect probe timed out waiting for a session\n", stderr)
                exit(1)
            }
            let browsed =
                model.selected == nil || model.errorMessage != nil
                || (model.browseResult.hasRun && !model.browseResult.isLoading)
            guard !model.isShowingConnectionForm, !model.isBusy, browsed else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                    MainActor.assumeIsolated(poll)
                }
                return
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                MainActor.assumeIsolated(next)
            }
        }
        poll()
    }

    /// Opens the form the way ⌘K does rather than by setting the flag.
    ///
    /// The flag is one assignment and would prove nothing about the command:
    /// this walks the menu bar for the item, checks validation lets it fire, and
    /// sends its action to its target, which is every link in the chain except
    /// AppKit's own key-equivalent dispatch.
    func presentThroughMenu() {
        guard
            let item = NSApp.mainMenu?.items
                .compactMap(\.submenu)
                .first(where: { $0.title == "File" })?
                .items.first(where: { $0.title == "Connect…" })
        else {
            fputs("no Connect… item in the File menu\n", stderr)
            exit(1)
        }
        guard let validator = item.target as? NSMenuItemValidation,
            validator.validateMenuItem(item), let action = item.action
        else {
            fputs("the Connect… item is disabled over a live connection\n", stderr)
            exit(1)
        }
        fputs("menu   item     “\(item.title)” · enabled\n", stderr)
        NSApp.sendAction(action, to: item.target, from: item)
    }

    whenSettled {
        report("before")
        presentThroughMenu()
        guard model.isShowingConnectionForm else {
            fputs("the Connect… item did not open the connection form\n", stderr)
            exit(1)
        }
        // The session is still behind the form, which is what makes Cancel a
        // way out rather than a button into an empty window.
        fputs("form   cancel   \(model.canCancelConnection)\n", stderr)
        // Through the path that does not remember, for the reason `--conn` does
        // not: a capture run must not change which database the next launch
        // opens. What this is checking is that the switch clears the window,
        // not that it writes to UserDefaults — and the first version of it did
        // write, which is how the probe left a database nobody had chosen as
        // the one this application would open next.
        model.connect(using: connString)
        whenSettled {
            report("after")
            exit(0)
        }
    }
}

/// Ends the process when the last window goes, and asks first when ending it
/// would lose work.
///
/// This application has exactly one window and no command that makes another:
/// no New Window, no document to reopen, and nothing that answers a click on the
/// Dock icon. Closing it therefore left a process running with a menu bar, no
/// window, and no way back to one — every item greyed out, ⌘Q the only thing
/// that still did anything. A single-window application quits when its window
/// closes, which is what Calculator and System Settings have always done and
/// what the close button on a window with nothing behind it promises.
///
/// Which makes both ⌘W and ⌘Q ways to end the process, and neither of them asked
/// anything: a grid holding twenty rows marked for deletion and a connection
/// holding an open transaction went the same way as an empty window. Not a new
/// defect — ⌘Q has always quit on the spot — but giving the window a working ⌘W
/// put a second one right next to it, one key away from Close in every muscle
/// memory on the platform.
///
/// Both paths are guarded here, with one decision and one dialog behind them:
/// `windowShouldClose` for ⌘W and the close button, `applicationShouldTerminate`
/// for ⌘Q, the Quit item, and a logout that asks the application first. Not a
/// setting — this is the last thing between a person and work that cannot be got
/// back, and a preference to turn it off is a preference to lose it silently.
///
/// Declared here rather than beside the menu targets because it is about the
/// process rather than about a command. `NSApplication.delegate` and
/// `NSWindow.delegate` are both weak references, so the top-level `let` below is
/// what keeps this alive.
final class AppLifecycle: NSObject, NSApplicationDelegate, NSWindowDelegate {
    /// The session, once there is one to ask about. `--bench` builds a window
    /// with no model behind it, and there is nothing in that one to lose.
    var model: AppModel?

    /// Whether the window has already put the question for the gesture that is
    /// ending the process.
    ///
    /// ⌘W arrives here twice: once as the window closing, and again as the
    /// termination that closing the last window causes. Without this the person
    /// who has just said "Discard and Quit" is asked the same thing a second time,
    /// over a window that has already gone.
    private var askedOnClose = false

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }

    /// Coming back to the front is the moment to find out whether the connection
    /// survived being left alone.
    ///
    /// This delegate method rather than a timer, because a timer would ask over
    /// and over while nobody was watching and would cost a round trip on every
    /// open connection for an answer nobody had asked for. It also fires once at
    /// launch, which is harmless: there is nothing open yet to ask about.
    func applicationDidBecomeActive(_ notification: Notification) {
        MainActor.assumeIsolated { model?.probeOpenConnections() }
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        MainActor.assumeIsolated {
            guard mayDiscardUnsavedWork() else { return false }
            askedOnClose = true
            return true
        }
    }

    /// Answers synchronously rather than with `.terminateLater`: the question is
    /// an `NSAlert` this thread runs itself, and there is nothing to save in the
    /// background — the choice is between sending the staged changes, which is
    /// Save's job and not a quit's, and losing them.
    func applicationShouldTerminate(_ app: NSApplication) -> NSApplication.TerminateReply {
        MainActor.assumeIsolated {
            if askedOnClose { return .terminateNow }
            return mayDiscardUnsavedWork() ? .terminateNow : .terminateCancel
        }
    }

    /// Puts the question, and answers whether the process may end.
    ///
    /// One dialog for both ways out, worded by `UnsavedWork` so that what it says
    /// can be checked without anybody at the keyboard — see `--verify-quitting`.
    /// A modal alert rather than the window's own error banner for the reason
    /// `AppModel.confirmDeletion` gives: a strip that can be ignored is not a
    /// question, and this one has to be answered before the process goes.
    @MainActor
    private func mayDiscardUnsavedWork() -> Bool {
        guard let work = model?.unsavedWork else { return true }
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = work.question
        alert.informativeText = work.detail
        // Quitting leads because it is what the keystroke asked for, and it says
        // what it costs rather than only where it goes. Cancel takes the escape
        // key, so dismissing the dialog without reading it keeps the work.
        alert.addButton(withTitle: "Discard and Quit")
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let lifecycle = AppLifecycle()
app.delegate = lifecycle

guard let device = MTLCreateSystemDefaultDevice() else {
    print("no metal device")
    exit(1)
}

let window = NSWindow(
    contentRect: NSRect(x: 0, y: 0, width: 1600, height: 1000),
    styleMask: [.titled, .closable, .resizable, .miniaturizable, .fullSizeContentView],
    backing: .buffered,
    defer: false)

if benchMode {
    // The bench has no window to ask in and no business guessing. A default
    // connection string would measure whatever was listening on that port and
    // report the number as if it meant something.
    guard
        let connString = connArgument
            // Read straight from the defaults rather than through a
            // `Preferences`: this path builds no window and is not on the main
            // actor, and where the connection was remembered is a question about
            // the user's defaults rather than about anything on screen.
            ?? ConnectionStore.load(from: Preferences.connectionStorage()).first.map({
                let id = $0.id
                return $0.settings.connectionString(
                    password: ConnectionKeychain.password(for: id) ?? "")
            })
    else {
        fputs(
            "--bench needs a database and has no window to ask in.\n"
                + "  Pass --conn \"postgres://user:password@host:port/database\",\n"
                + "  or connect once in the application so the connection is remembered.\n",
            stderr)
        exit(1)
    }

    window.title = "Phase 0 — 1M rows"

    guard let renderer = GridRenderer(device: device, scale: window.backingScaleFactor) else {
        print("renderer init failed")
        exit(1)
    }

    let view = GridView(frame: window.contentLayoutRect, device: device)
    view.colorPixelFormat = .bgra8Unorm
    view.clearColor = MTLClearColor(red: 0.08, green: 0.09, blue: 0.11, alpha: 1)
    view.renderer = renderer
    // The bench pumps frames itself; see `startBench(view:)`.
    view.isPaused = true
    view.enableSetNeedsDisplay = false

    let controller = GridViewController(
        renderer: renderer, connString: connString, sql: benchSQL,
        benchMode: true, benchFrames: benchFrames, verifyMode: verifyMode)
    view.delegate = controller
    window.contentView = view
    window.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)

    controller.loadInBackground {
        controller.startBench(view: view)
    }
} else {
    // Pinned before the window is shown, so nothing lays out in the wrong
    // appearance and flashes on the first frame.
    Theme.apply(to: app)

    // Transparent, so the unified titlebar and toolbar take the background set
    // on the line below rather than the system's own material. Opaque, that
    // strip is a neutral near-black running the full width of the window —
    // above the sidebar and the detail column alike — while every other
    // surface under it is the palette's blue-tinted background, and the seam
    // reads as two applications stacked. `.fullSizeContentView` is already in
    // the style mask and the toolbar still lays out beneath it, so nothing
    // moves; what changes is the fill.
    window.titlebarAppearsTransparent = true
    window.toolbarStyle = .unified
    window.backgroundColor = NSColor(Theme.background.color)
    // Below this the grid shows one column and the filter bar wraps; there is
    // no useful layout smaller, so the window is not allowed to reach it.
    window.minSize = NSSize(width: 940, height: 580)

    // Top-level code runs on the main thread but is not statically isolated in
    // Swift 5 mode; assert the isolation the model requires rather than hop.
    // Until a connection lands the window has no relation to name, and
    // `navigationTitle` has not run. A titleless window reads as one that failed
    // to finish launching.
    window.title = "DbClient"

    MainActor.assumeIsolated {
        let history: QueryHistory
        if let historyStore {
            // Emptied rather than merely kept apart: a suite is a persistent
            // defaults domain like any other, so a second capture would
            // otherwise open on the first one's entries.
            UserDefaults.standard.removePersistentDomain(forName: historyStore)
            guard let scratch = UserDefaults(suiteName: historyStore) else {
                fputs("--history-store \(historyStore) is not a usable suite name\n", stderr)
                exit(1)
            }
            history = QueryHistory(defaults: scratch)
        } else {
            history = QueryHistory()
        }
        let preferences: Preferences
        if capturePane != nil || mcpProbePort != nil {
            // One fixed suite, emptied on the way in, the way `--history-store`
            // does it: `ScratchDefaults` is for checks, which release what they
            // mint, and a window that runs until somebody kills it never
            // reaches a `defer` — so a suite per run would leave one plist per
            // capture behind for good.
            let name = "dev.dbclient.captureprefs"
            UserDefaults.standard.removePersistentDomain(forName: name)
            guard let store = UserDefaults(suiteName: name) else {
                fputs("could not open the capture defaults suite\n", stderr)
                exit(1)
            }
            if capturePane == .mcp || mcpProbePort != nil {
                store.set(true, forKey: "dev.dbclient.mcpServerEnabled")
            }
            if let mcpProbePort {
                store.set(mcpProbePort, forKey: "dev.dbclient.mcpServerPort")
            }
            preferences = Preferences(store: store)
        } else {
            preferences = Preferences()
        }
        let model = AppModel(
            history: history, favorites: QueryFavorites(), preferences: preferences,
            initialTab: initialTab, initialSQL: initialSQL,
            initialCaret: initialCaret, initialSQLIsScript: runScriptMode,
            initialWhere: initialWhere, initialOrder: initialOrder,
            initialStructureDetail: initialSection, initialRelation: initialRelation,
            initialFilter: initialFilter)
        // Installed here rather than before the window is built, because the
        // File menu sends to the model and there is no model until now.
        AppMenu.install(into: app, model: model)
        // The quit guard needs the model for the same reason and gets it here
        // too. Only this window is given the delegate: the Settings panel closes
        // with ⌘W and loses nothing, and a question in front of that would be a
        // question about nothing.
        lifecycle.model = model
        window.delegate = lifecycle
        // Here rather than in the model's own init, because only a window that
        // is about to run a run loop has any business owning a repeating
        // timer: the `--verify-*` suites build models by the dozen and exit.
        model.startKeepAliveTimer()
        // Beside the timer for the timer's reason: a server belongs to the
        // process that will run a run loop, not to every model a check builds.
        MCPCoordinator.shared.follow(
            preferences: model.preferences,
            connections: { [weak model] in model?.connections.connections ?? [] })
        window.contentView = NSHostingView(rootView: MainView(model: model))
        window.center()
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)

        // Nothing here opens the form: it is what the window shows until a
        // connection replaces it, so the last branch is simply not connecting.
        if let connArgument {
            model.connect(using: connArgument, marking: safetyMarks)
        }

        if let initialCell { openValueViewer(model: model, on: initialCell) }
        if let deleteRowSpec { markRowsForDeletion(model: model, spec: deleteRowSpec) }
        if let addRowCount { addRows(model: model, count: addRowCount) }
        if let reconnectTo { reconnectWhenReady(model: model, to: reconnectTo) }
        if let stopAfter { stopWhenRunning(model: model, after: stopAfter) }
        if let loadMorePages { loadMoreWhenReady(model: model, pages: loadMorePages) }
        if runScriptMode { runScriptWhenReady(model: model) }
        if completeMode { completeWhenReady(model: model) }
        if showHistory || historyAll || historyPick != nil {
            driveHistory(
                model: model, open: showHistory, all: historyAll, pick: historyPick)
        }
        if renameBuffer { openBufferNameField(model: model) }
        if let switchToDatabase { switchDatabaseWhenReady(model: model, to: switchToDatabase) }
        if dropConnection { dropConnectionWhenReady(model: model) }
        if transferPicker { openTransferPicker(model: model) }
        if collapseSidebar { model.wantsSidebarRail = true }
        if filterObjects { filterObjectsWhenReady(model: model) }
        if showFindBar { showFindBarWhenReady(model: model) }
        if mcpProbePort != nil {
            // After `follow`, which is where the server starts and the token is
            // minted. No token means it did not start, and the coordinator has
            // already said why on this same stream.
            if let token = MCPCoordinator.shared.token {
                fputs("mcp: http://127.0.0.1:\(preferences.mcpServerPort)/mcp\n", stderr)
                fputs("mcp: token \(token)\n", stderr)
            } else {
                fputs("mcp: not running\n", stderr)
            }
        }
        if let capturePane {
            // A local is enough to keep: `present` hands the panel to AppKit,
            // whose window list retains it, and nothing here switches panes
            // after the shutter.
            SettingsWindow().present(model.preferences, pane: capturePane)
            // Said out loud for the reason the probes say things: the capture
            // runs unattended, and a pane that failed to appear should name
            // itself in the log rather than in a screenshot of the wrong
            // window.
            for w in NSApp.windows where w.isVisible {
                fputs("capture: window \"\(w.title)\" \(w.frame)\n", stderr)
            }
        }
        if preferencesProbe { probePreferences(model: model) }
        if historyProbe { probeHistory(model: model) }
        if sessionsProbe { probeSessions(model: model) }
        if transferProbe { probeTransfer(model: model) }
        if importProbe { probeImport(model: model) }
        if let exportPath { exportWhenReady(model: model, to: exportPath) }
        if let importPath { importWhenReady(model: model, from: importPath) }
        if let mapImportPath { openImportSheet(model: model, from: mapImportPath) }
        if let fkJumpColumn { followForeignKey(model: model, from: fkJumpColumn) }
        if let findText {
            findInGrid(model: model, text: findText, column: findColumn)
        }
        if let createTablePath {
            openCreateTableSheet(model: model, from: createTablePath)
        }
        if let refreshAfter { refreshWhenReady(model: model, after: refreshAfter) }
    }
}

app.run()
