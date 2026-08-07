import AppKit
import MetalKit

// Phase 0 harness. `--bench` runs a scripted scroll and prints frame-time
// statistics, because scroll smoothness has to be measured rather than
// eyeballed. Without it the window is interactive, for manual inspection.

let connString = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench"
let sql = "SELECT * FROM bench_wide"
let benchMode = CommandLine.arguments.contains("--bench")
let verifyMode = CommandLine.arguments.contains("--verify")
let benchFrames = 600

let app = NSApplication.shared
app.setActivationPolicy(.regular)

guard let device = MTLCreateSystemDefaultDevice() else {
    print("no metal device")
    exit(1)
}

let window = NSWindow(
    contentRect: NSRect(x: 0, y: 0, width: 1600, height: 1000),
    styleMask: [.titled, .closable, .resizable, .miniaturizable],
    backing: .buffered,
    defer: false)
window.title = "Phase 0 — 1M rows"

guard let renderer = GridRenderer(device: device, scale: window.backingScaleFactor) else {
    print("renderer init failed")
    exit(1)
}

let view = GridView(frame: window.contentLayoutRect, device: device)
view.colorPixelFormat = .bgra8Unorm
view.clearColor = MTLClearColor(red: 0.08, green: 0.09, blue: 0.11, alpha: 1)
view.renderer = renderer
// Benchmarking needs continuous frames; interactive use redraws on input only.
view.isPaused = !benchMode
view.enableSetNeedsDisplay = !benchMode
view.preferredFramesPerSecond = 120

let controller = GridViewController(
    renderer: renderer, connString: connString, sql: sql,
    benchMode: benchMode, benchFrames: benchFrames, verifyMode: verifyMode)
view.delegate = controller
window.contentView = view
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

controller.loadInBackground {
    if benchMode {
        controller.startBench()
    } else {
        view.needsDisplay = true
    }
}

app.run()
