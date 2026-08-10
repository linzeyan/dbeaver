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
// Without it, the full application shell starts.

let connString = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench"
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

// `--verify-splitter` runs the statement splitter's checks and exits with their
// verdict. It needs no window and no database, so it runs before either exists.
if CommandLine.arguments.contains("--verify-splitter") {
    exit(SQLScriptChecks.run() ? 0 : 1)
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

/// `--where` and `--order` seed the browse filters, for the same reason `--tab`
/// exists: reproducing a particular view without clicking into it.
let initialWhere = argument("--where")
let initialOrder = argument("--order")

/// `--relation bench_wide` opens on a named table instead of the first one.
/// Accepts `schema.name` to reach a schema other than the one that opens by
/// default.
let initialRelation = argument("--relation")

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
    let format = DelimitedFormat(pathExtension: url.pathExtension) ?? .csv
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
            fputs("export name     \(model.exportFilename(format))\n", stderr)
            fputs("export message  \(model.exportMessage)\n", stderr)
            model.exportCurrentResult(to: url, format: format)
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
            connString: connString, history: history,
            initialTab: initialTab, initialSQL: initialSQL,
            initialCaret: initialCaret, initialSQLIsScript: runScriptMode,
            initialWhere: initialWhere, initialOrder: initialOrder,
            initialStructureDetail: initialSection, initialRelation: initialRelation)
        // Installed here rather than before the window is built, because the
        // File menu sends to the model and there is no model until now.
        AppMenu.install(into: app, model: model)
        window.contentView = NSHostingView(rootView: MainView(model: model))
        window.center()
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)
        model.connect()
        if let initialCell { openValueViewer(model: model, on: initialCell) }
        if runScriptMode { runScriptWhenReady(model: model) }
        if showHistory || historyPick != nil {
            driveHistory(model: model, open: showHistory, pick: historyPick)
        }
        if let exportPath { exportWhenReady(model: model, to: exportPath) }
        if let refreshAfter { refreshWhenReady(model: model, after: refreshAfter) }
    }
}

app.run()
