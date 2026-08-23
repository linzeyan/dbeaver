// Launches a GUI binary, waits for its main window, captures just that window,
// and shuts it down.
//
// Exists because rendering correctness cannot be inferred from frame times. The
// first version of the grid hit 1165 fps while drawing visibly wrong text.
//
// Usage: swift capture-window.swift <output.png> <executable> [args...]

import Foundation
import CoreGraphics

guard CommandLine.arguments.count >= 3 else {
    print("usage: capture-window.swift <output.png> <executable> [args...]")
    exit(1)
}

let output = CommandLine.arguments[1]
let exePath = CommandLine.arguments[2]
let args = Array(CommandLine.arguments.dropFirst(3))
let timeout = 60.0
/// Extra settle time after the window appears, so the first frame and any
/// asynchronous data load have landed before the shutter.
let settle: UInt32 = 2_500_000

let proc = Process()
proc.executableURL = URL(fileURLWithPath: exePath)
proc.arguments = args

// Output goes to a file, not a Pipe. Reading a pipe to EOF blocks until the
// child exits, and the child is still running by design at that point.
let logPath = NSTemporaryDirectory() + "capture-window-\(getpid()).log"
FileManager.default.createFile(atPath: logPath, contents: nil)
let logHandle = FileHandle(forWritingAtPath: logPath)
proc.standardOutput = logHandle
proc.standardError = logHandle

do {
    try proc.run()
} catch {
    print("launch failed: \(error)")
    exit(1)
}
let pid = proc.processIdentifier

// The size floor keeps palettes and popups from being mistaken for the window,
// and 600×400 is the main window's. The Settings panel is 460 wide and takes
// its height from whichever pane is showing, so a capture of it says so
// through the environment rather than by loosening the floor for everything.
let minWidth = ProcessInfo.processInfo.environment["CAPTURE_MIN_WIDTH"].flatMap(Double.init) ?? 600
let minHeight =
    ProcessInfo.processInfo.environment["CAPTURE_MIN_HEIGHT"].flatMap(Double.init) ?? 400
// Naming the window beats sizing it when the app has two: the poll below runs
// from launch, and whichever window the server registers first wins a race
// the caller never meant to enter.
let wantedTitle = ProcessInfo.processInfo.environment["CAPTURE_WINDOW_TITLE"]

func mainWindowID() -> CGWindowID? {
    guard let list = CGWindowListCopyWindowInfo(
        [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]]
    else { return nil }

    for w in list {
        guard let owner = w[kCGWindowOwnerPID as String] as? Int32, owner == pid,
              let layer = w[kCGWindowLayer as String] as? Int, layer == 0,
              let num = w[kCGWindowNumber as String] as? CGWindowID,
              let b = w[kCGWindowBounds as String] as? [String: Any],
              let h = b["Height"] as? Double, let width = b["Width"] as? Double,
              h >= minHeight, width >= minWidth
        else { continue }
        if let wantedTitle, (w[kCGWindowName as String] as? String) != wantedTitle { continue }
        return num
    }
    return nil
}

func shutdown(_ code: Int32) -> Never {
    proc.terminate()
    usleep(200_000)
    if proc.isRunning { kill(pid, SIGKILL) }
    exit(code)
}

let start = CFAbsoluteTimeGetCurrent()
var windowID: CGWindowID?
while CFAbsoluteTimeGetCurrent() - start < timeout {
    if let id = mainWindowID() { windowID = id; break }
    usleep(20_000)
}

guard let windowID else {
    print("no window appeared within \(timeout)s")
    shutdown(2)
}

usleep(settle)

let capture = Process()
capture.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
capture.arguments = ["-x", "-o", "-l", String(windowID), output]
try? capture.run()
capture.waitUntilExit()

// Surface whatever the app printed; it usually carries the load statistics.
try? logHandle?.close()
if let s = try? String(contentsOfFile: logPath, encoding: .utf8), !s.isEmpty {
    print(s.trimmingCharacters(in: .whitespacesAndNewlines))
}
try? FileManager.default.removeItem(atPath: logPath)

if capture.terminationStatus == 0,
   let attrs = try? FileManager.default.attributesOfItem(atPath: output),
   let size = attrs[.size] as? Int, size > 0 {
    print("captured \(output) (\(size) bytes)")
    shutdown(0)
} else {
    print("capture failed (status \(capture.terminationStatus))")
    shutdown(3)
}
