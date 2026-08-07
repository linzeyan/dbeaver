import AppKit
import MetalKit
import CDbFfi

// Kept out of main.swift deliberately: types declared in a file with top-level
// code inherit @MainActor isolation, which is wrong for a controller whose
// whole job is to move work off the main thread.

/// Raw Arrow pointers handed from the loading thread to the main thread.
/// Ownership transfers with the value and the receiver is the only reader, so
/// the unchecked conformance is a statement about the protocol, not a shortcut.
struct ArrowHandoff<T>: @unchecked Sendable {
    let pointer: UnsafeMutablePointer<T>
}

final class GridViewController: NSObject, MTKViewDelegate {
    let renderer: GridRenderer
    let table = ArrowTable()

    private let connString: String
    private let sql: String
    private let benchMode: Bool
    private let benchFrames: Int
    let verifyMode: Bool

    private var firstBatchMs: Double = .nan
    private var loadCompleteMs: Double = .nan
    private var benchFrameCount = 0
    private var benchStarted = false

    init(renderer: GridRenderer, connString: String, sql: String,
         benchMode: Bool, benchFrames: Int, verifyMode: Bool) {
        self.renderer = renderer
        self.connString = connString
        self.sql = sql
        self.benchMode = benchMode
        self.benchFrames = benchFrames
        self.verifyMode = verifyMode
        super.init()
        renderer.table = table
    }

    /// Prints where the data Swift reads actually lives.
    ///
    /// Buffers are reported for two separate batches: if a copy were happening,
    /// the addresses would land in Swift-owned allocations and RSS would carry a
    /// second copy of the entire result.
    private func printZeroCopyProbe() {
        for batchIdx in [0, max(0, 1)] where batchIdx < 2 {
            let probes = table.probe(batch: batchIdx)
            guard !probes.isEmpty else { continue }
            let totalMalloc = probes.reduce(0) { $0 + $1.mallocSize }
            print("probe_batch      \(batchIdx)")
            print("  columns        \(probes.count)")
            print("  rows           \(probes.first?.rows ?? 0)")
            print("  buffer_bytes   \(totalMalloc)")
            for p in probes.prefix(4) {
                print(String(format: "  %-14@ @0x%llx  alloc=%d",
                             p.column as NSString, UInt64(p.address), p.mallocSize))
            }
        }
    }

    /// `onReady` is `@MainActor` because it is only ever invoked from the main
    /// queue below, and callers legitimately want to touch the view from it.
    /// A plain `@Sendable` closure would compile but forbid exactly that.
    func loadInBackground(onReady: @escaping @MainActor () -> Void) {
        let connString = self.connString
        let sql = self.sql
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            do {
                let loadStart = CFAbsoluteTimeGetCurrent()
                let db = try Database(connString: connString)
                let query = try db.query(sql, batchRows: 8192)

                let schema = ArrowHandoff(pointer: try query.schema())
                DispatchQueue.main.sync {
                    table.setSchema(schema.pointer)
                    if let release = schema.pointer.pointee.release {
                        release(schema.pointer)
                    }
                    schema.pointer.deallocate()
                }

                var isFirst = true
                while let raw = try query.nextBatch() {
                    if isFirst {
                        firstBatchMs = (CFAbsoluteTimeGetCurrent() - loadStart) * 1000
                        isFirst = false
                    }
                    // Serialized onto the main thread because the renderer reads
                    // the same table. Phase 1 hands across an immutable snapshot
                    // instead; here the cost shows up in load time and nowhere
                    // else, which is acceptable for a measurement harness.
                    let batch = ArrowHandoff(pointer: raw)
                    DispatchQueue.main.sync {
                        table.append(batch: batch.pointer)
                    }
                }
                loadCompleteMs = (CFAbsoluteTimeGetCurrent() - loadStart) * 1000

                let rows = table.rowCount
                let cols = table.columns.count
                let first = firstBatchMs
                let total = loadCompleteMs
                let wantProbe = self.verifyMode
                DispatchQueue.main.async {
                    print("rows             \(rows)")
                    print("columns          \(cols)")
                    print("first_batch_ms   \(String(format: "%.1f", first))")
                    print("load_total_ms    \(String(format: "%.1f", total))")
                    if wantProbe { self.printZeroCopyProbe() }
                    // Already on the main queue; assert that rather than hop
                    // again, which would delay the first frame by a runloop turn.
                    MainActor.assumeIsolated { onReady() }
                }
            } catch {
                print("load failed: \(error)")
                exit(1)
            }
        }
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        if benchMode && benchStarted {
            // Advance across the whole result so the measurement covers cold
            // cells rather than repeatedly re-reading one page.
            let step = Double(max(1, table.rowCount / benchFrames))
            renderer.scrollRow += step
            if renderer.scrollRow > Double(table.rowCount) { renderer.scrollRow = 0 }
        }

        renderer.draw(in: view)

        if benchMode && benchStarted {
            benchFrameCount += 1
            if benchFrameCount >= benchFrames {
                report()
                exit(0)
            }
        }
    }

    func startBench() {
        // Skip the first frames: pipeline warm-up and the initial texture
        // upload are not steady-state scrolling.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [self] in
            renderer.resetStats()
            benchStarted = true
        }
    }

    private func report() {
        let s = renderer.frameSamples.sorted()
        guard !s.isEmpty else { return }
        func pct(_ p: Double) -> Double { s[min(s.count - 1, Int(Double(s.count) * p))] }
        let mean = s.reduce(0, +) / Double(s.count)
        print("frames           \(s.count)")
        print("frame_mean_ms    \(String(format: "%.3f", mean))")
        print("frame_p50_ms     \(String(format: "%.3f", pct(0.50)))")
        print("frame_p95_ms     \(String(format: "%.3f", pct(0.95)))")
        print("frame_p99_ms     \(String(format: "%.3f", pct(0.99)))")
        print("frame_max_ms     \(String(format: "%.3f", s.last!))")
        print("implied_fps      \(String(format: "%.0f", 1000.0 / mean))")
    }
}

/// MTKView subclass that turns scroll events into row offsets.
final class GridView: MTKView {
    weak var renderer: GridRenderer?

    override var acceptsFirstResponder: Bool { true }

    override func scrollWheel(with event: NSEvent) {
        guard let renderer, let table = renderer.table else { return }

        renderer.scrollRow = max(0, min(
            Double(table.rowCount - 1),
            renderer.scrollRow - Double(event.scrollingDeltaY) / Double(renderer.rowHeight) * 3))

        // Clamp so the last column cannot be scrolled off the right edge.
        let maxX = max(0, renderer.contentWidth(columns: table.columns.count)
            - Float(bounds.width))
        renderer.scrollX = max(0, min(maxX, renderer.scrollX - Float(event.scrollingDeltaX)))

        needsDisplay = true
    }
}
