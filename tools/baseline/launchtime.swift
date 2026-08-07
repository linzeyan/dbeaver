// Measures time from exec to the first substantial on-screen window owned by
// the launched process. This is the number a user experiences as "startup",
// which is not the same as process exit or main() being reached.
//
// Usage: swift launchtime.swift <timeout_s> <executable> [args...]

import Foundation
import CoreGraphics

let timeout = Double(CommandLine.arguments[1]) ?? 60
let exePath = CommandLine.arguments[2]
let args = Array(CommandLine.arguments.dropFirst(3))

let t0 = CFAbsoluteTimeGetCurrent()
let proc = Process()
proc.executableURL = URL(fileURLWithPath: exePath)
proc.arguments = args
proc.standardOutput = FileHandle.nullDevice
proc.standardError = FileHandle.nullDevice

do {
    try proc.run()
} catch {
    print("launch_failed \(error)")
    exit(1)
}

let pid = proc.processIdentifier

// A window counts once it is on screen and large enough to be the main window
// rather than a splash or a menu-bar artifact.
func mainWindowVisible() -> Bool {
    guard let list = CGWindowListCopyWindowInfo(
        [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]]
    else { return false }

    for w in list {
        guard let owner = w[kCGWindowOwnerPID as String] as? Int32, owner == pid,
              let layer = w[kCGWindowLayer as String] as? Int, layer == 0,
              let b = w[kCGWindowBounds as String] as? [String: Any],
              let h = b["Height"] as? Double, let width = b["Width"] as? Double
        else { continue }
        if h >= 400 && width >= 600 { return true }
    }
    return false
}

while CFAbsoluteTimeGetCurrent() - t0 < timeout {
    if mainWindowVisible() {
        print(String(format: "window_ms %.1f", (CFAbsoluteTimeGetCurrent() - t0) * 1000))
        proc.terminate()
        // Give it a moment to exit cleanly before the harness moves on.
        usleep(200_000)
        if proc.isRunning { kill(pid, SIGKILL) }
        exit(0)
    }
    usleep(5_000)
}

print("timeout after \(timeout)s")
proc.terminate()
if proc.isRunning { kill(pid, SIGKILL) }
exit(2)
