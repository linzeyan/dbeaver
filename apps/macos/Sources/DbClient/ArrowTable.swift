import CDbFfi
import Foundation

/// Read-only view over Arrow batches received from the core.
///
/// Nothing here copies buffer contents. A column holds the pointers Arrow
/// handed us and reads through them; only `text(row:)` allocates, and only for
/// the handful of cells actually on screen.
final class ArrowTable {
    struct Column {
        let name: String
        let kind: Kind
        /// Cached per-batch accessors, indexed by batch.
        fileprivate var batches: [ColumnBatch] = []
    }

    enum Kind {
        case bool, int16, int32, int64, float32, float64
        case utf8, binary
        case decimal128(precision: Int32, scale: Int32)
        case timestamp(tz: Bool), date32, time64
        case unsupported(String)

        /// Whether values of this kind are compared by magnitude, and so should
        /// be right-aligned: digits only line up for scanning when the units
        /// column does.
        var isNumeric: Bool {
            switch self {
            case .int16, .int32, .int64, .float32, .float64, .decimal128:
                return true
            case .bool, .utf8, .binary, .timestamp, .date32, .time64, .unsupported:
                return false
            }
        }
    }

    private(set) var columns: [Column] = []
    private(set) var rowCount: Int = 0
    /// Row offset at which each batch starts, for locating a global row index.
    private var batchStarts: [Int] = []
    private var retained: [RetainedBatch] = []

    // MARK: - Ingest

    /// Drops all batches and columns.
    ///
    /// Releasing the retained batches here is what returns the Rust-side
    /// buffers; without it, switching tables would accumulate every result ever
    /// loaded.
    func reset() {
        columns.removeAll()
        batchStarts.removeAll()
        retained.removeAll()
        rowCount = 0
    }

    func setSchema(_ schema: UnsafeMutablePointer<ArrowSchema>) {
        columns = (0..<Int(schema.pointee.n_children)).map { i in
            let child = schema.pointee.children![i]!
            let name = child.pointee.name.map { String(cString: $0) } ?? "col\(i)"
            return Column(name: name, kind: Self.kind(fromFormat: String(cString: child.pointee.format)))
        }
    }

    func append(batch: UnsafeMutablePointer<ArrowArray>) {
        let retainedBatch = RetainedBatch(array: batch)
        batchStarts.append(rowCount)
        let length = Int(batch.pointee.length)

        for i in columns.indices {
            let child = batch.pointee.children![i]!
            columns[i].batches.append(
                ColumnBatch(array: child, kind: columns[i].kind, length: length)
            )
        }

        retained.append(retainedBatch)
        rowCount += length
    }

    // MARK: - Access

    /// Displayable text for a cell. The only allocating path, called once per
    /// visible cell per frame.
    func text(row: Int, column: Int) -> String {
        guard column < columns.count, row < rowCount else { return "" }
        guard let (batchIdx, localRow) = Self.locate(
            row: row, batchStarts: batchStarts, columns: columns) else { return "" }
        return columns[column].batches[batchIdx].text(at: localRow)
    }

    /// Whether a cell is SQL NULL.
    ///
    /// `text` renders NULL and an empty string identically, and in a database
    /// client those are different values — the caller has to be able to tell
    /// them apart before deciding what to draw.
    func isNull(row: Int, column: Int) -> Bool {
        guard column < columns.count, row < rowCount else { return false }
        guard let (batchIdx, localRow) = Self.locate(
            row: row, batchStarts: batchStarts, columns: columns) else { return false }
        return columns[column].batches[batchIdx].isNull(localRow)
    }

    /// Which batch holds a global row index, and where inside it.
    ///
    /// Takes the state it searches rather than reading it off `self`, so a
    /// `Snapshot` can run the same search over its own copy of it.
    fileprivate static func locate(
        row: Int, batchStarts: [Int], columns: [Column]
    ) -> (Int, Int)? {
        // Batches are uniform except the last, but binary search keeps this
        // correct if that ever stops being true.
        var lo = 0, hi = batchStarts.count - 1
        while lo <= hi {
            let mid = (lo + hi) / 2
            let start = batchStarts[mid]
            let end = start + (columns.first?.batches[mid].length ?? 0)
            if row < start { hi = mid - 1 }
            else if row >= end { lo = mid + 1 }
            else { return (mid, row - start) }
        }
        return nil
    }

    // MARK: - Snapshot

    /// The rows the table holds right now, as a value another thread may read.
    ///
    /// Taking one costs four array retains. A batch never changes once it has
    /// been appended, and holding `retained` here owns those batches
    /// independently of the table they came from — so an export can walk a
    /// million rows on a background queue while the main thread resets the same
    /// table and loads something else into it. Without that retain, `reset()`
    /// would hand the Arrow buffers back to Rust underneath the reader. The
    /// unchecked conformance is that argument, not a shortcut.
    struct Snapshot: @unchecked Sendable {
        let columns: [Column]
        let rowCount: Int
        fileprivate let batchStarts: [Int]
        /// Never read. Owning the batches for as long as this value lives is
        /// the entire job of this property.
        private let retained: [RetainedBatch]

        fileprivate init(
            columns: [Column], rowCount: Int, batchStarts: [Int], retained: [RetainedBatch]
        ) {
            self.columns = columns
            self.rowCount = rowCount
            self.batchStarts = batchStarts
            self.retained = retained
        }

        /// The cell's value, or nil where it is SQL NULL.
        ///
        /// One lookup rather than `isNull` followed by `text`, because an
        /// export asks this of every cell in the result and the batch search is
        /// not free.
        func value(row: Int, column: Int) -> String? {
            guard column < columns.count, row < rowCount,
                  let (batchIdx, localRow) = ArrowTable.locate(
                      row: row, batchStarts: batchStarts, columns: columns)
            else { return nil }
            let batch = columns[column].batches[batchIdx]
            return batch.isNull(localRow) ? nil : batch.text(at: localRow)
        }
    }

    func snapshot() -> Snapshot {
        Snapshot(
            columns: columns, rowCount: rowCount, batchStarts: batchStarts, retained: retained)
    }

    // MARK: - Zero-copy verification

    struct BufferProbe {
        let column: String
        let address: UInt
        /// Allocation size as reported by the allocator that owns this pointer.
        /// A non-zero value means the pointer refers to a live heap block we did
        /// not allocate — i.e. Rust's Arrow buffer, read in place.
        let mallocSize: Int
        let rows: Int
    }

    /// Reports where each column's data buffer actually lives for a given batch.
    ///
    /// This exists to make the zero-copy claim falsifiable: if Swift were
    /// copying, these addresses would move into Swift-owned allocations and the
    /// process RSS would carry a second full copy of the result.
    func probe(batch: Int) -> [BufferProbe] {
        columns.compactMap { col in
            guard batch < col.batches.count else { return nil }
            let cb = col.batches[batch]
            guard let ptr = cb.dataBufferAddress else { return nil }
            return BufferProbe(
                column: col.name,
                address: UInt(bitPattern: ptr),
                mallocSize: malloc_size(ptr),
                rows: cb.length)
        }
    }

    private static func kind(fromFormat f: String) -> Kind {
        switch f {
        case "b": return .bool
        case "s": return .int16
        case "i": return .int32
        case "l": return .int64
        case "f": return .float32
        case "g": return .float64
        case "u", "U": return .utf8
        case "z", "Z": return .binary
        case "tdD": return .date32
        case "ttu": return .time64
        default:
            if f.hasPrefix("d:") {
                // "d:precision,scale"
                let parts = f.dropFirst(2).split(separator: ",")
                let precision = parts.count > 0 ? Int32(parts[0]) ?? 0 : 0
                let scale = parts.count > 1 ? Int32(parts[1]) ?? 0 : 0
                return .decimal128(precision: precision, scale: scale)
            }
            if f.hasPrefix("tsu:") {
                return .timestamp(tz: f.count > 4)
            }
            return .unsupported(f)
        }
    }
}

/// Owns an ArrowArray's lifetime. Releasing is what returns the Rust-side
/// buffers, so it must happen exactly once, at deinit.
private final class RetainedBatch {
    private let array: UnsafeMutablePointer<ArrowArray>

    init(array: UnsafeMutablePointer<ArrowArray>) {
        self.array = array
    }

    deinit {
        if let release = array.pointee.release {
            release(array)
        }
        array.deallocate()
    }
}

/// One column within one batch: raw pointers into Arrow buffers.
private struct ColumnBatch {
    let kind: ArrowTable.Kind
    let length: Int
    let offset: Int
    let validity: UnsafePointer<UInt8>?
    let buffer1: UnsafeRawPointer?
    let buffer2: UnsafeRawPointer?

    init(array: UnsafeMutablePointer<ArrowArray>, kind: ArrowTable.Kind, length: Int) {
        self.kind = kind
        self.length = Int(array.pointee.length)
        self.offset = Int(array.pointee.offset)
        let buffers = array.pointee.buffers
        let n = Int(array.pointee.n_buffers)
        self.validity = n > 0 ? buffers?[0]?.assumingMemoryBound(to: UInt8.self) : nil
        self.buffer1 = n > 1 ? buffers?[1].map { UnsafeRawPointer($0) } : nil
        self.buffer2 = n > 2 ? buffers?[2].map { UnsafeRawPointer($0) } : nil
    }

    /// Address of the column's primary data buffer, for the zero-copy probe.
    var dataBufferAddress: UnsafeMutableRawPointer? {
        buffer1.map { UnsafeMutableRawPointer(mutating: $0) }
    }

    func isNull(_ i: Int) -> Bool {
        guard let validity else { return false }
        let bit = offset + i
        return (validity[bit / 8] >> UInt8(bit % 8)) & 1 == 0
    }

    func text(at i: Int) -> String {
        if isNull(i) { return "" }
        let idx = offset + i
        switch kind {
        case .bool:
            guard let b = buffer1?.assumingMemoryBound(to: UInt8.self) else { return "" }
            return (b[idx / 8] >> UInt8(idx % 8)) & 1 == 1 ? "true" : "false"
        case .int16:
            return String(load(Int16.self, idx))
        case .int32, .date32:
            let v = load(Int32.self, idx)
            if case .date32 = kind { return Self.dateText(days: v) }
            return String(v)
        case .int64, .time64:
            let v = load(Int64.self, idx)
            if case .time64 = kind { return Self.timeText(micros: v) }
            return String(v)
        case .float32:
            return String(load(Float.self, idx))
        case .float64:
            return String(load(Double.self, idx))
        case .timestamp:
            return Self.timestampText(micros: load(Int64.self, idx))
        case .decimal128(let precision, let scale):
            return Self.decimalText(
                load(Int128Bits.self, idx), precision: precision, scale: scale)
        case .utf8:
            guard let offsets = buffer1?.assumingMemoryBound(to: Int32.self),
                  let data = buffer2?.assumingMemoryBound(to: UInt8.self) else { return "" }
            let start = Int(offsets[idx]), end = Int(offsets[idx + 1])
            guard end > start else { return "" }
            return String(decoding: UnsafeBufferPointer(start: data + start, count: end - start),
                          as: UTF8.self)
        case .binary:
            guard let offsets = buffer1?.assumingMemoryBound(to: Int32.self) else { return "" }
            let start = Int(offsets[idx]), end = Int(offsets[idx + 1])
            return "0x… (\(end - start) B)"
        case .unsupported(let f):
            return "<\(f)>"
        }
    }

    private func load<T>(_ type: T.Type, _ idx: Int) -> T {
        guard let b = buffer1 else { return withUnsafeTemporaryAllocation(of: T.self, capacity: 1) { $0[0] } }
        return b.load(fromByteOffset: idx * MemoryLayout<T>.stride, as: T.self)
    }

    // Formatting kept minimal for Phase 0: correct enough to eyeball, cheap
    // enough not to distort the frame-time measurement.

    private static func dateText(days: Int32) -> String {
        let secs = TimeInterval(days) * 86400
        return isoDate.string(from: Date(timeIntervalSince1970: secs))
    }

    private static func timeText(micros: Int64) -> String {
        let s = micros / 1_000_000
        return String(format: "%02d:%02d:%02d", s / 3600, (s % 3600) / 60, s % 60)
    }

    private static func timestampText(micros: Int64) -> String {
        isoDateTime.string(from: Date(timeIntervalSince1970: TimeInterval(micros) / 1e6))
    }

    /// What the driver emits for a NUMERIC whose scale it could not read. Kept
    /// in step with `NUMERIC_PRECISION`/`NUMERIC_SCALE` in arrow_map.rs: the
    /// pair is how that side says "scale unknown", there being nowhere else in
    /// an Arrow schema to say it.
    private static let normalizedPrecision: Int32 = 38
    private static let normalizedScale: Int32 = 10

    /// Formats a decimal exactly, by placing a point in the integer's digits.
    ///
    /// Going through `Double` loses precision past 2^53 and renders a
    /// `numeric(18,4)` value as eighteen digits of noise. Decimal columns are
    /// usually money, so being visibly wrong here is not acceptable even in a
    /// preview.
    private static func decimalText(
        _ bits: Int128Bits, precision: Int32, scale: Int32
    ) -> String {
        let v = bits.value
        var digits = String(v.magnitude)
        let sign = v < 0 ? "-" : ""

        guard scale > 0 else { return sign + digits }

        // Left-pad so there is at least one integer digit before the point.
        while digits.count <= Int(scale) {
            digits = "0" + digits
        }
        let split = digits.index(digits.endIndex, offsetBy: -Int(scale))
        let whole = String(digits[..<split])
        var fraction = String(digits[split...])

        // A declared scale is part of what the column means: numeric(12,2) is
        // money, and 1000.00 trimmed to 1000 reads as a different column. The
        // driver's fallback pair is the one case where the scale was unknown
        // and normalized, so its padding really is noise.
        if precision == normalizedPrecision && scale == normalizedScale {
            while fraction.hasSuffix("0") { fraction.removeLast() }
        }

        return fraction.isEmpty ? sign + whole : sign + whole + "." + fraction
    }

    private static let isoDate: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()

    private static let isoDateTime: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd HH:mm:ss"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()
}

/// Arrow decimal128 is a 16-byte little-endian two's-complement integer.
///
/// Loaded as two halves and recombined rather than read as `Int128` directly,
/// because the Arrow buffer is only guaranteed 8-byte aligned.
private struct Int128Bits {
    let lo: UInt64
    let hi: Int64

    var value: Int128 {
        Int128(hi) << 64 | Int128(lo)
    }
}
