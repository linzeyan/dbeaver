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
    AppMenu.install(into: app)

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
        window.contentView = NSHostingView(rootView: MainView(model: model))
        window.center()
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)
        model.connect()
    }
}

app.run()
