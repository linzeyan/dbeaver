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
// Without it, the full application shell starts — on the connection form, or
// straight into a session when `--conn` or a remembered connection says which
// database. There is no built-in one: a default would silently connect to
// whatever happened to be listening on a port, which is the one thing a
// database client must never do.

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

/// `--connect-form` opens the connection form even when a connection was
/// remembered.
///
/// Exists for the reason `--tab` does: a screenshot is how a layout defect here
/// gets caught, and a screenshot can neither press Connect… nor know what a
/// previous run happened to leave in UserDefaults.
let forceConnectForm = CommandLine.arguments.contains("--connect-form")

/// `--reconnect "postgres://…/other"` opens a second database once the first
/// connection has landed, through the File menu's own Connect… item, printing
/// what the window holds before and after.
///
/// Exists for the reason `--refresh-after` does: Connect… is reachable only
/// from a menu item, and the form's own Connect button only from a click —
/// synthetic events need accessibility permission this environment does not
/// grant. The two reports are the whole claim: opening a second database has to
/// leave nothing of the first on screen, and launching straight into that
/// database would show the same window while proving nothing about switching.
/// Pointed at a connection that fails, it also leaves the form up over a live
/// session, which is the one state a capture cannot otherwise reach.
let reconnectTo = argument("--reconnect")

// `--verify-splitter`, `--verify-connection`, `--verify-completion`,
// `--verify-transaction`, `--verify-editing`, `--verify-metadata`,
// `--verify-schema-metadata` and `--verify-preferences` run the checks for the
// pieces of pure logic in the front-end and exit with their verdict. None needs
// a window or a database, so they run before either exists.
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
if CommandLine.arguments.contains("--verify-metadata") {
    exit(MetadataChecks.run() ? 0 : 1)
}
if CommandLine.arguments.contains("--verify-schema-metadata") {
    exit(SchemaMetadataChecks.run() ? 0 : 1)
}
// The only one of these that has to state its isolation. `Preferences` is
// main-actor isolated because the window reads it, and top-level code runs on
// the main thread without being statically known to.
if CommandLine.arguments.contains("--verify-preferences") {
    exit(MainActor.assumeIsolated { PreferencesChecks.run() } ? 0 : 1)
}

/// `--tab structure|content|query` opens straight to a pane. Screenshots are
/// how rendering defects get caught here, and a screenshot cannot click.
let initialTab = argument("--tab").flatMap { DetailTab(rawValue: $0.capitalized) } ?? .content

/// `--sql "SELECT …"` opens on the Query tab with that statement already run.
let initialSQL = argument("--sql")

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
let initialCell = argument("--cell")

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

    func poll() {
        let result = model.current
        if let index = result.table.columns.firstIndex(where: { $0.name == column }),
            result.hasRun, !result.isLoading, result.rowCount > 0
        {
            result.selection = GridSelection(
                row: min(max(0, row - 1), result.rowCount - 1), column: index)
            openViewerThroughMenu()
            fputs("cell selected   \(column) (column \(index)) row \(row)\n", stderr)
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
        let objects = model.schemas
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

/// Drives `--history` and `--history-pick`. Polls, and reports to stderr, for
/// the reasons `exportWhenReady` does: the model's background pipeline has no
/// completion hook, and a capture switch does not justify inventing one.
///
/// Unlike the other probes this does not exit on success — the window has to
/// stay up for the shutter — so waiting for a history that never fills has to be
/// loud, or a capture of an empty panel would read as the panel failing to draw.
@MainActor
func driveHistory(model: AppModel, open: Bool, pick: Int?) {
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
        let objects = model.schemas
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
            guard !model.isPresentingConnection, !model.isBusy, browsed else {
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
        guard model.isPresentingConnection else {
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

let app = NSApplication.shared
app.setActivationPolicy(.regular)

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
            ?? ConnectionStore.remembered().map({
                $0.settings.connectionString(password: $0.password)
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

    window.titlebarAppearsTransparent = false
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
        let model = AppModel(
            history: history, preferences: Preferences(),
            initialTab: initialTab, initialSQL: initialSQL,
            initialCaret: initialCaret, initialSQLIsScript: runScriptMode,
            initialWhere: initialWhere, initialOrder: initialOrder,
            initialStructureDetail: initialSection, initialRelation: initialRelation,
            initialFilter: initialFilter)
        // Installed here rather than before the window is built, because the
        // File menu sends to the model and there is no model until now.
        AppMenu.install(into: app, model: model)
        window.contentView = NSHostingView(rootView: RootView(model: model))
        window.center()
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)

        // Nothing here opens the form: it is what the window shows until a
        // connection replaces it, so the last branch is simply not connecting.
        if let connArgument {
            model.connect(using: connArgument)
        } else if !forceConnectForm, let remembered = ConnectionStore.remembered() {
            model.connect(to: remembered.settings, password: remembered.password)
        }

        if let initialCell { openValueViewer(model: model, on: initialCell) }
        if let deleteRowSpec { markRowsForDeletion(model: model, spec: deleteRowSpec) }
        if let addRowCount { addRows(model: model, count: addRowCount) }
        if let reconnectTo { reconnectWhenReady(model: model, to: reconnectTo) }
        if let stopAfter { stopWhenRunning(model: model, after: stopAfter) }
        if let loadMorePages { loadMoreWhenReady(model: model, pages: loadMorePages) }
        if runScriptMode { runScriptWhenReady(model: model) }
        if completeMode { completeWhenReady(model: model) }
        if showHistory || historyPick != nil {
            driveHistory(model: model, open: showHistory, pick: historyPick)
        }
        if preferencesProbe { probePreferences(model: model) }
        if let exportPath { exportWhenReady(model: model, to: exportPath) }
        if let refreshAfter { refreshWhenReady(model: model, after: refreshAfter) }
    }
}

app.run()
