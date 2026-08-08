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

/// `--tab structure|content|query` opens straight to a pane. Screenshots are
/// how rendering defects get caught here, and a screenshot cannot click.
let initialTab = argument("--tab").flatMap { DetailTab(rawValue: $0.capitalized) } ?? .content

/// `--sql "SELECT …"` opens on the Query tab with that statement already run.
let initialSQL = argument("--sql")

/// `--where` and `--order` seed the browse filters, for the same reason `--tab`
/// exists: reproducing a particular view without clicking into it.
let initialWhere = argument("--where")
let initialOrder = argument("--order")

/// `--section triggers` opens the Structure tab on one of its lower sections.
/// Matched loosely so `foreignkeys`, `foreign-keys` and `Foreign keys` all work
/// — this is a capture switch, not a parser.
/// `--relation bench_wide` opens on a named table instead of the first one.
let initialRelation = argument("--relation")

/// `--export out.csv` writes the opened result to a file and exits.
///
/// Exists for the same reason `--tab` and `--relation` do, one step further on:
/// the export is otherwise reachable only through a save panel, and a script
/// cannot click one. Without it there is no way to check what actually lands in
/// a file. The format follows the extension.
let exportPath = argument("--export")

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

let initialSection = argument("--section").flatMap { requested in
    let wanted = requested.lowercased().filter { $0.isLetter }
    return StructureDetail.allCases.first {
        $0.rawValue.lowercased().filter(\.isLetter) == wanted
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
        let model = AppModel(
            connString: connString, initialTab: initialTab, initialSQL: initialSQL,
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
        if let exportPath { exportWhenReady(model: model, to: exportPath) }
    }
}

app.run()
