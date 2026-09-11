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
        /// Whether the server declared this column NOT NULL.
        ///
        /// Not the same question as whether the field is nullable, which it
        /// always is: a driver may substitute NULL for a value Arrow has no
        /// shape for, and MySQL's `'0000-00-00'` is exactly that. So the
        /// declaration arrives beside the validity buffer instead of in it, and
        /// this is what tells a substituted NULL from one the data contains.
        let declaredNotNull: Bool
        /// The shape the column's text values are written in, or "" where the
        /// result made no claim.
        ///
        /// The result's claim rather than the relation's, and that is the whole
        /// point of it: a column a driver invented (MongoDB's `_extra`) or
        /// rendered on the way out (a DuckDB `STRUCT`) is in no catalogue, so
        /// the declared type a browse reads has nothing to say about it. Empty
        /// for nearly every column, which is why it is a string rather than an
        /// enum — the reader compares it and does not switch on it.
        let valueShape: String
        /// The type the server declared this column with, or "" where the driver
        /// had none to give.
        ///
        /// The result's own answer, which is the half a catalogue lookup cannot
        /// reach: a query pane's columns can be expressions and aliases across
        /// three relations, and no `information_schema` row describes them. Where
        /// there is a catalogue answer as well it is the better one, for the
        /// reason `GridRenderer.typeLabel` gives; this is what the header falls
        /// back to instead of to the name of the buffer the values arrived in.
        let declaredType: String
        /// Cached per-batch accessors, indexed by batch.
        fileprivate var batches: [ColumnBatch] = []
    }

    /// The scale a datetime column counts in.
    ///
    /// Arrow spells the unit into the format string and one column's unit is not
    /// another's — DuckDB alone sends all four for timestamps, because
    /// `TIMESTAMP_S`, `TIMESTAMP_MS`, `TIMESTAMP` and `TIMESTAMP_NS` are four
    /// types a person can declare. Measured, not assumed: a fixture holding one
    /// of each comes back as `Second`, `Millisecond`, `Microsecond` and
    /// `Nanosecond`, and `TIME_NS` comes back as `Time64(Nanosecond)`.
    ///
    /// This reader used to take microseconds for granted and send everything
    /// else to `unsupported`, which drew `<tsn:>` in every cell. Reading them as
    /// microseconds instead would have been worse: the same instant off by a
    /// factor of a thousand either way, and a plausible date is not something
    /// anyone checks.
    enum TimeUnit {
        case second, milli, micro, nano

        /// What Arrow puts between `ts` and the zone, and after `tt`.
        init?(arrow letter: Character) {
            switch letter {
            case "s": self = .second
            case "m": self = .milli
            case "u": self = .micro
            case "n": self = .nano
            default: return nil
            }
        }

        var perSecond: Int64 {
            switch self {
            case .second: return 1
            case .milli: return 1_000
            case .micro: return 1_000_000
            case .nano: return 1_000_000_000
            }
        }

        /// How many digits a fraction of this unit is written with, and zero
        /// where there is no fraction to write.
        var fractionDigits: Int {
            switch self {
            case .second: return 0
            case .milli: return 3
            case .micro: return 6
            case .nano: return 9
            }
        }
    }

    enum Kind {
        case bool, int8, int16, int32, int64, float32, float64
        /// The unsigned widths, which are not the signed ones read differently:
        /// a `BIGINT UNSIGNED` runs to twice `Int64.max`, and reading its top
        /// half as a signed integer prints a negative number for a column that
        /// cannot hold one.
        case uint8, uint16, uint32, uint64
        case utf8, binary
        /// A span of time rather than a reading on a clock — see `durationText`.
        case duration
        /// A column of nothing but NULL. Arrow's null type has no buffers at
        /// all, which is why `isNull` answers for it by its kind.
        case null
        case decimal128(precision: Int32, scale: Int32)
        case timestamp(tz: Bool, unit: TimeUnit)
        case date32
        case time64(unit: TimeUnit)
        /// A column whose values are not in its own buffers but in the children
        /// beside them. Three drivers send one — see `Nested`.
        indirect case nested(Nested)
        case unsupported(String)

        /// Whether values of this kind are compared by magnitude, and so should
        /// be right-aligned: digits only line up for scanning when the units
        /// column does.
        var isNumeric: Bool {
            switch self {
            case .int8, .int16, .int32, .int64, .float32, .float64, .decimal128:
                return true
            case .uint8, .uint16, .uint32, .uint64:
                return true
            // A duration is a magnitude, and it is still not one of these: it is
            // drawn as `838:59:59`, where lining up the last digits would line
            // up seconds against minutes.
            case .bool, .utf8, .binary, .timestamp, .date32, .time64, .duration, .null, .nested,
                .unsupported:
                return false
            }
        }

        /// The children this kind is read through, in Arrow's own order, and
        /// empty for every kind whose values are in its own buffers.
        ///
        /// One list rather than a case analysis at each use, because the two
        /// things that walk children — building the per-batch accessors and
        /// rendering a value — have to agree about how many there are and what
        /// order they come in. A batch whose children do not line up with this
        /// is refused rather than read off by one.
        var childFields: [Field] {
            guard case .nested(let nested) = self else { return [] }
            return nested.childFields
        }

        /// What to call this kind where no declared type is available.
        ///
        /// Deliberately the Arrow spelling rather than a SQL one: this is what
        /// arrived, and a column with nothing behind it — `count(*)`, `a || b` —
        /// has no declaration to borrow a name from. Calling it `int4` would be
        /// inventing a declaration that does not exist.
        var label: String {
            switch self {
            case .bool: return "bool"
            case .int8: return "int8"
            case .int16: return "int16"
            case .int32: return "int32"
            case .int64: return "int64"
            case .uint8: return "uint8"
            case .uint16: return "uint16"
            case .uint32: return "uint32"
            case .uint64: return "uint64"
            case .float32: return "float32"
            case .float64: return "float64"
            case .utf8: return "utf8"
            case .binary: return "binary"
            case .duration: return "duration"
            case .null: return "null"
            case .decimal128(let precision, let scale):
                // The normalized pair is the driver saying it could not read the
                // declared scale. Printing it would state a precision this
                // column was never declared with, which is the one failure a
                // type label must not have.
                return precision == ArrowTable.normalizedPrecision
                    && scale == ArrowTable.normalizedScale
                    ? "decimal" : "decimal(\(precision),\(scale))"
            // The unit is not in the label. It is in the values — a fraction is
            // written when there is one — and `dbclient.declared_type` carries
            // the name the database gave the column, which for the driver that
            // sends all four units says `TIMESTAMP_NS` outright.
            case .timestamp(let tz, _): return tz ? "timestamptz" : "timestamp"
            case .date32: return "date"
            case .time64: return "time"
            case .nested(let nested): return nested.label
            case .unsupported(let f): return "<\(f)>"
            }
        }
    }

    /// One field of a nested column: what it is called and what it holds.
    struct Field {
        let name: String
        let kind: Kind
    }

    /// The shape of a column whose values live in its children.
    ///
    /// Four, and they are exactly the four a server on this side of the FFI has
    /// been seen to send. Three drivers hand the server's own Arrow schema
    /// straight over without a type table — Flight SQL, BigQuery and Databricks
    /// each decode an IPC stream and keep what it describes — and the project's
    /// Flight SQL subject, which is DuckDB behind the protocol, answers
    /// `list<l: int32>`, `struct<qty: int32, unit: string>`, `map<string, int32>`
    /// and `fixed_size_list<: string>[2]` for the four DuckDB spellings. The
    /// twelve drivers that build Arrow a value at a time flatten their nesting to
    /// text before it gets here and are not affected either way.
    ///
    /// Not `LargeList`, not `Union`, not `RunEndEncoded`, not `Dictionary`:
    /// nothing has sent one, and a reader for a type with no producer is a claim
    /// about a column this build has never seen. Those keep reading as their
    /// format string, which says truthfully that this reader cannot follow them.
    ///
    /// `map` keeps its entries struct rather than flattening to a key and a
    /// value, because that is how Arrow lays the children out — a map *is* a list
    /// of `struct<key, value>` — and a model that disagreed with the layout would
    /// have to re-derive it at every step.
    indirect enum Nested {
        /// `+l`, one child holding the elements of every row's list.
        case list(Field)
        /// `+w:N`, the same with no offsets buffer: row *i* owns elements
        /// `i * N ..< (i + 1) * N`.
        case fixedList(Field, width: Int)
        /// `+s`, one child per field, each indexed by the parent's own row.
        case structure([Field])
        /// `+m`, a list whose element is a two-field struct.
        case map(entries: Field)

        var childFields: [Field] {
            switch self {
            case .list(let element): return [element]
            case .fixedList(let element, _): return [element]
            case .structure(let fields): return fields
            case .map(let entries): return [entries]
            }
        }

        /// What to call this shape, in Arrow's spelling for the reason
        /// `Kind.label` gives: nothing behind a nested column has a SQL
        /// declaration to borrow, and DuckDB's `STRUCT(qty INTEGER)` and
        /// ClickHouse's `Tuple(qty Int32)` are not the same words.
        var label: String {
            switch self {
            case .list(let element): return "list<\(element.kind.label)>"
            case .fixedList(let element, let width): return "list<\(element.kind.label)>[\(width)]"
            case .structure(let fields):
                let inner = fields.map { "\($0.name): \($0.kind.label)" }.joined(separator: ", ")
                return "struct<\(inner)>"
            case .map(let entries):
                let pair = entries.kind.childFields
                guard pair.count == 2 else { return "map" }
                return "map<\(pair[0].kind.label), \(pair[1].kind.label)>"
            }
        }
    }

    /// What the driver emits for a NUMERIC whose scale it could not read. Kept
    /// in step with `NUMERIC_PRECISION`/`NUMERIC_SCALE` in arrow_map.rs: the
    /// pair is how that side says "scale unknown", there being nowhere else in
    /// an Arrow schema to say it.
    fileprivate static let normalizedPrecision: Int32 = 38
    fileprivate static let normalizedScale: Int32 = 10

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
            let declared = Self.declarations(child.pointee.metadata)
            return Column(
                name: name, kind: Self.kind(of: child),
                declaredNotNull: declared.notNull, valueShape: declared.valueShape,
                declaredType: declared.declaredType)
        }
    }

    /// The keys the core writes its field declarations under.
    ///
    /// Spelled here as well as in `dbconn::DECLARED_NOT_NULL`, `VALUE_SHAPE` and
    /// `DECLARED_TYPE` because the C data interface carries no shared header for
    /// them — the string is the contract.
    static let declaredNotNullKey = "dbclient.declared_not_null"
    static let valueShapeKey = "dbclient.value_shape"
    static let declaredTypeKey = "dbclient.declared_type"

    /// The value `valueShapeKey` takes for a column of JSON documents. Matches
    /// `dbconn::SHAPE_JSON`.
    static let jsonShape = "json"

    /// What one field's metadata declares about its column.
    struct Declarations: Equatable {
        /// Whether the server declared the column NOT NULL.
        var notNull = false
        /// The shape its text values are written in, or "" for no claim.
        var valueShape = ""
        /// The type it was declared with, or "" for no claim.
        var declaredType = ""
    }

    /// Reads those declarations out of a field's metadata.
    ///
    /// The C data interface counts its lengths rather than terminating them, so
    /// the buffer may hold NUL bytes and reading it as a C string would stop at
    /// the first one: an int32 pair count, then per pair an int32 key length,
    /// the key, an int32 value length, the value. None of it is promised to be
    /// aligned, hence the unaligned loads.
    ///
    /// A count or length below zero answers the defaults instead of trapping —
    /// this is memory another language handed over, and a grid that crashes on
    /// it would be a worse failure than a column drawn as though it were
    /// nullable. A buffer that lies about a length in the other direction cannot
    /// be caught here, because the format carries no total size to check it
    /// against. What has been read so far is kept rather than discarded: a
    /// truncated buffer is corrupt whichever way it is read, and the entries
    /// before the damage were whole.
    ///
    /// One walk for all three keys rather than one per key. The buffer is walked
    /// with unaligned loads over counted strings, and a second reader of it is a
    /// second chance to mis-step by four bytes and answer a plausible default.
    static func declarations(_ metadata: UnsafePointer<CChar>?) -> Declarations {
        var found = Declarations()
        guard let metadata else { return found }
        var cursor = UnsafeRawPointer(metadata)
        let pairs = cursor.loadUnaligned(as: Int32.self)
        guard pairs > 0 else { return found }
        cursor += MemoryLayout<Int32>.size
        var seenNotNull = false
        var seenShape = false
        var seenType = false
        for _ in 0..<pairs {
            guard let key = takeString(&cursor), let value = takeString(&cursor) else {
                return found
            }
            // Decided at the first match per key: a second entry under the same
            // key is not something the writer can produce.
            switch key {
            case declaredNotNullKey where !seenNotNull:
                found.notNull = value == "1"
                seenNotNull = true
            case valueShapeKey where !seenShape:
                found.valueShape = value
                seenShape = true
            case declaredTypeKey where !seenType:
                found.declaredType = value
                seenType = true
            default:
                continue
            }
        }
        return found
    }

    /// One length-prefixed string, advancing the cursor past it.
    private static func takeString(_ cursor: inout UnsafeRawPointer) -> String? {
        let length = cursor.loadUnaligned(as: Int32.self)
        guard length >= 0 else { return nil }
        cursor += MemoryLayout<Int32>.size
        let bytes = UnsafeRawBufferPointer(start: cursor, count: Int(length))
        cursor += Int(length)
        return String(decoding: bytes, as: UTF8.self)
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
        guard
            let (batchIdx, localRow) = Self.locate(
                row: row, batchStarts: batchStarts, columns: columns)
        else { return "" }
        return columns[column].batches[batchIdx].text(at: localRow)
    }

    /// Whether a cell is SQL NULL.
    ///
    /// `text` renders NULL and an empty string identically, and in a database
    /// client those are different values — the caller has to be able to tell
    /// them apart before deciding what to draw.
    func isNull(row: Int, column: Int) -> Bool {
        guard column < columns.count, row < rowCount else { return false }
        guard
            let (batchIdx, localRow) = Self.locate(
                row: row, batchStarts: batchStarts, columns: columns)
        else { return false }
        return columns[column].batches[batchIdx].isNull(localRow)
    }

    /// A binary cell's bytes, or nil where the column is not binary or the cell
    /// is NULL.
    ///
    /// `text` renders a binary cell as a byte count, which is all that fits in a
    /// grid cell and tells a reader nothing about what is in it. Copied out
    /// rather than handed over as a pointer into the Arrow buffer: the caller
    /// holds the value past the frame it asked in, and a `reset()` in between
    /// would return that buffer to Rust underneath it.
    func bytes(row: Int, column: Int) -> [UInt8]? {
        guard column < columns.count, row < rowCount else { return nil }
        guard
            let (batchIdx, localRow) = Self.locate(
                row: row, batchStarts: batchStarts, columns: columns)
        else { return nil }
        return columns[column].batches[batchIdx].bytes(at: localRow)
    }

    /// How much of a nested value one grid cell spells out.
    ///
    /// `text(row:column:)` runs for every visible cell on every frame, and a
    /// `LIST` column holding ten thousand elements would build a megabyte of
    /// string per cell per frame to fill two hundred points of grid. So the cell
    /// is a preview — enough to recognise the value by, not enough to read it —
    /// and the pane under the grid is where the rest goes. The same trade the
    /// binary columns already make with `0x… (12 B)`.
    static let cellBudget = 256

    /// And how much of one the value viewer builds.
    ///
    /// `RenderedValue` cuts its own rendering at this number, so a document
    /// longer than it is one the pane would not draw anyway. Read from there
    /// rather than written down again: two constants would be two chances to
    /// raise one and leave the viewer laying out a value the reader had already
    /// cut. Built once per selection change instead of once per frame, which is
    /// what makes the larger number affordable at all.
    fileprivate static var documentBudget: Int { RenderedValue.characterCap }

    /// A nested cell as a whole JSON document, for the viewer.
    ///
    /// Nil where the column is not nested, so the caller keeps the single
    /// rendering path it already had for everything else. Read here rather than
    /// on demand for the reason `bytes(row:column:)` gives: the walk has to
    /// happen while the batch is alive, and the pane is drawn after the model has
    /// finished with it.
    func json(row: Int, column: Int) -> String? {
        guard column < columns.count, row < rowCount, case .nested = columns[column].kind,
            let (batchIdx, localRow) = Self.locate(
                row: row, batchStarts: batchStarts, columns: columns)
        else { return nil }
        return columns[column].batches[batchIdx].json(at: localRow, budget: Self.documentBudget)
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
        var lo = 0
        var hi = batchStarts.count - 1
        while lo <= hi {
            let mid = (lo + hi) / 2
            let start = batchStarts[mid]
            let end = start + (columns.first?.batches[mid].length ?? 0)
            if row < start {
                hi = mid - 1
            } else if row >= end {
                lo = mid + 1
            } else {
                return (mid, row - start)
            }
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

    /// How deep the reader will follow a schema into its children.
    ///
    /// The walk is recursive and the schema is another process's memory: a
    /// server that described a thousand levels of list would take the stack down
    /// with it before a single row arrived. Real nesting is two or three deep — a
    /// list of structs holding a list — and past this the column reads as its
    /// format string, which is what every nested column read as before this file
    /// could follow one at all.
    static let maxNesting = 16

    /// One field's kind, following its children where it has any.
    ///
    /// Takes the schema node rather than its format string, because the format
    /// string is only half of a nested type: `+s` says "struct" and says nothing
    /// about which fields, and those are in `children`.
    static func kind(of schema: UnsafePointer<ArrowSchema>, depth: Int = 0) -> Kind {
        let format = String(cString: schema.pointee.format)
        if let nested = nested(format, of: schema, depth: depth) { return .nested(nested) }
        return kind(fromFormat: format)
    }

    /// The nested shape a node describes, or nil where it describes none.
    ///
    /// Nil is also the answer for a node that claims a nested format and does not
    /// carry the children for it — a `+m` whose entries are not a pair, a `+l`
    /// with two children. The caller then falls through to `unsupported`, so the
    /// column reads as `<+m>`: this reader could not follow it, said in the one
    /// way that cannot be mistaken for a value.
    private static func nested(
        _ format: String, of schema: UnsafePointer<ArrowSchema>, depth: Int
    ) -> Nested? {
        guard depth < maxNesting, let children = schema.pointee.children else { return nil }
        let count = Int(schema.pointee.n_children)

        func field(_ index: Int) -> Field? {
            guard index < count, let child = children[index] else { return nil }
            return Field(
                name: child.pointee.name.map { String(cString: $0) } ?? "",
                kind: kind(of: child, depth: depth + 1))
        }

        switch format {
        case "+l":
            guard count == 1, let element = field(0) else { return nil }
            return .list(element)
        case "+s":
            let fields = (0..<count).compactMap(field)
            guard fields.count == count else { return nil }
            return .structure(fields)
        case "+m":
            // A map's child is `struct<key, value>`. Anything else under `+m` is
            // a node this reader has no rule for, and guessing which child was
            // the key would be reading a layout rather than being told it.
            guard count == 1, let entries = field(0), entries.kind.childFields.count == 2
            else { return nil }
            return .map(entries: entries)
        default:
            guard format.hasPrefix("+w:"), let width = Int(format.dropFirst(3)), width > 0,
                count == 1, let element = field(0)
            else { return nil }
            return .fixedList(element, width: width)
        }
    }

    private static func kind(fromFormat f: String) -> Kind {
        switch f {
        case "b": return .bool
        case "c": return .int8
        case "s": return .int16
        case "i": return .int32
        case "l": return .int64
        // Arrow spells the unsigned widths as the capitals of the signed ones.
        case "C": return .uint8
        case "S": return .uint16
        case "I": return .uint32
        case "L": return .uint64
        case "f": return .float32
        case "g": return .float64
        case "u", "U": return .utf8
        case "z", "Z": return .binary
        case "tdD": return .date32
        // Only the two 64-bit spellings. Arrow's `tts` and `ttm` are `Time32`,
        // which is the same unit letter at half the stride — reading one of
        // those as an `Int64` takes two values for one and prints a number that
        // is neither. No driver here sends a `Time32`; if one starts, it belongs
        // in a case of its own rather than in this one.
        case "ttu": return .time64(unit: .micro)
        case "ttn": return .time64(unit: .nano)
        // Microseconds only, which is the one unit any driver here sends. The
        // other three spellings reach `unsupported` and say so, rather than
        // being read as microseconds and drawn a thousandfold wrong.
        case "tDu": return .duration
        case "n": return .null
        default:
            if f.hasPrefix("d:") {
                // "d:precision,scale" — or "d:precision,scale,bitWidth", which
                // is the one that matters. The width is optional and defaults
                // to 128, so an earlier version read the first two numbers and
                // ignored the rest: a Decimal256 column matched this case,
                // reported itself as decimal128, and the reader then took 16
                // bytes out of every 32. That is not an unreadable column, it
                // is a column of plausible wrong numbers, which is the worst
                // thing this file could produce.
                let parts = f.dropFirst(2).split(separator: ",")
                let precision = parts.count > 0 ? Int32(parts[0]) ?? 0 : 0
                let scale = parts.count > 1 ? Int32(parts[1]) ?? 0 : 0
                let bits = parts.count > 2 ? Int32(parts[2]) ?? 0 : 128
                guard bits == 128 else { return .unsupported(f) }
                return .decimal128(precision: precision, scale: scale)
            }
            // `ts<unit>:<zone>`, where the zone is optional and its presence is
            // the whole difference between a wall clock and an instant. The
            // colon is checked rather than assumed: `ts` is also the start of
            // nothing else Arrow spells, but a format this reader half-matched
            // would be read at the wrong scale instead of being named.
            let letters = Array(f)
            if letters.count >= 4, letters[0] == "t", letters[1] == "s", letters[3] == ":",
                let unit = TimeUnit(arrow: letters[2])
            {
                return .timestamp(tz: letters.count > 4, unit: unit)
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
    /// The child columns, in the order `Kind.childFields` names them, and empty
    /// for every column whose values are in its own buffers.
    ///
    /// Pointers like everything else here. The children are already inside the
    /// batch this table retains, so following them costs one small value per
    /// child per batch and copies nothing — which is the whole reason a nested
    /// value can be read on demand rather than materialised into the model.
    let children: [ColumnBatch]

    init(array: UnsafeMutablePointer<ArrowArray>, kind: ArrowTable.Kind, length: Int) {
        self.kind = kind
        self.length = Int(array.pointee.length)
        self.offset = Int(array.pointee.offset)
        let buffers = array.pointee.buffers
        let n = Int(array.pointee.n_buffers)
        self.validity = n > 0 ? buffers?[0]?.assumingMemoryBound(to: UInt8.self) : nil
        self.buffer1 = n > 1 ? buffers?[1].map { UnsafeRawPointer($0) } : nil
        self.buffer2 = n > 2 ? buffers?[2].map { UnsafeRawPointer($0) } : nil

        // All of the children or none of them. A list one short of what the
        // schema named would put a struct's second field under its first name,
        // which is a wrong value that reads as a right one; an empty list is
        // refused by every reader below and shows as `<+s>`.
        let fields = kind.childFields
        var children: [ColumnBatch] = []
        if !fields.isEmpty, Int(array.pointee.n_children) == fields.count,
            let kids = array.pointee.children
        {
            children.reserveCapacity(fields.count)
            for (at, field) in fields.enumerated() {
                guard let kid = kids[at] else {
                    children.removeAll()
                    break
                }
                children.append(
                    ColumnBatch(array: kid, kind: field.kind, length: Int(kid.pointee.length)))
            }
        }
        self.children = children
    }

    /// Address of the column's primary data buffer, for the zero-copy probe.
    var dataBufferAddress: UnsafeMutableRawPointer? {
        buffer1.map { UnsafeMutableRawPointer(mutating: $0) }
    }

    func isNull(_ i: Int) -> Bool {
        // A null column holds nothing else, and it carries no validity buffer to
        // say so: Arrow's null type has no buffers at all. Read by the rule
        // below it would report every row as present and draw a blank cell,
        // which is a value — an empty string — rather than the absence of one.
        if case .null = kind { return true }
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
        case .int8:
            return String(load(Int8.self, idx))
        case .int16:
            return String(load(Int16.self, idx))
        case .uint8:
            return String(load(UInt8.self, idx))
        case .uint16:
            return String(load(UInt16.self, idx))
        case .uint32:
            return String(load(UInt32.self, idx))
        case .uint64:
            return String(load(UInt64.self, idx))
        case .duration:
            return Self.durationText(micros: load(Int64.self, idx))
        case .null:
            // Unreachable: every row of a null column is null and `isNull`
            // above has already answered. Here so that a kind added to the
            // enum cannot quietly fall into another case's reading.
            return ""
        case .int32, .date32:
            let v = load(Int32.self, idx)
            if case .date32 = kind { return Self.dateText(days: v) }
            return String(v)
        case .int64, .time64:
            let v = load(Int64.self, idx)
            if case .time64(let unit) = kind { return Self.timeText(v, unit: unit) }
            return String(v)
        case .float32:
            return String(load(Float.self, idx))
        case .float64:
            return String(load(Double.self, idx))
        case .timestamp(_, let unit):
            return Self.timestampText(load(Int64.self, idx), unit: unit)
        case .decimal128(let precision, let scale):
            return Self.decimalText(
                load(Int128Bits.self, idx), precision: precision, scale: scale)
        case .utf8:
            guard let offsets = buffer1?.assumingMemoryBound(to: Int32.self),
                let data = buffer2?.assumingMemoryBound(to: UInt8.self)
            else { return "" }
            let start = Int(offsets[idx])
            let end = Int(offsets[idx + 1])
            guard end > start else { return "" }
            return String(
                decoding: UnsafeBufferPointer(start: data + start, count: end - start),
                as: UTF8.self)
        case .binary:
            guard let offsets = buffer1?.assumingMemoryBound(to: Int32.self) else { return "" }
            let start = Int(offsets[idx])
            let end = Int(offsets[idx + 1])
            return "0x… (\(end - start) B)"
        case .nested:
            return json(at: i, budget: ArrowTable.cellBudget)
        case .unsupported(let f):
            return "<\(f)>"
        }
    }

    // MARK: - Nested values

    /// One nested cell as JSON, spending at most `budget` characters on it.
    func json(at i: Int, budget: Int) -> String {
        var sink = JSONSink(budget: budget)
        writeJSON(at: i, to: &sink)
        return sink.text
    }

    /// What a nested cell reads as when the batch disagrees with the schema it
    /// arrived with — a struct carrying fewer children than the fields it was
    /// described by, a list whose offsets point outside its values.
    ///
    /// A JSON string rather than `null`, which would say the database holds
    /// nothing there, and rather than nothing at all, which would leave a
    /// document that does not parse. What has gone wrong by then is memory, and
    /// the recoverable end of that is a cell that says so.
    private static let unreadable = "\"<unreadable>\""

    /// Appends this cell's value to `sink`, following children where there are
    /// any.
    ///
    /// Every index handed to a child is a *logical* one, as the C data interface
    /// defines it: the child adds its own `offset` and the caller adds the
    /// parent's. Getting that wrong is the failure this whole file is careful
    /// about — a batch read four bytes off answers a plausible value from the
    /// neighbouring row rather than failing.
    private func writeJSON(at i: Int, to sink: inout JSONSink) {
        // A run the walk cannot afford is not entered at all, and that is a claim
        // about the mark as much as about the cost. `JSONSink.text` reports the
        // run the walk was in when it stopped; a child stepped into on a spent
        // budget would report *its* length instead, so a document cut on a field's
        // name would claim that field's contents are the only thing missing while
        // saying nothing about the fields after it.
        guard !sink.isFull else { return }
        guard i >= 0, i < length else {
            sink.write(Self.unreadable)
            return
        }
        guard case .nested(let nested) = kind else {
            sink.write(isNull(i) ? "null" : scalarJSON(at: i))
            return
        }
        if isNull(i) {
            sink.write("null")
            return
        }
        let idx = offset + i
        switch nested {
        case .list:
            guard children.count == 1, let offsets = buffer1?.assumingMemoryBound(to: Int32.self)
            else {
                sink.write(Self.unreadable)
                return
            }
            writeElements(
                children[0], from: Int(offsets[idx]), to: Int(offsets[idx + 1]), into: &sink)

        case .fixedList(_, let width):
            // Multiplied with the overflow reported rather than trapped, for the
            // reason the bounds below are checked at all: the width is a number
            // parsed out of a format string another process wrote, and `+w:` and
            // a very large integer is a string a server can send.
            let (start, wide) = idx.multipliedReportingOverflow(by: width)
            let (end, past) = start.addingReportingOverflow(width)
            guard children.count == 1, !wide, !past else {
                sink.write(Self.unreadable)
                return
            }
            writeElements(children[0], from: start, to: end, into: &sink)

        case .structure(let fields):
            guard children.count == fields.count else {
                sink.write(Self.unreadable)
                return
            }
            sink.write("{")
            for (at, field) in fields.enumerated() {
                guard !sink.isFull else {
                    sink.abandon(fields.count - at)
                    break
                }
                if at > 0 { sink.write(",") }
                sink.write(Self.quoted(field.name))
                sink.write(":")
                // The parent's own row index, not the field's position: a
                // struct's children are one array each, indexed by the row.
                children[at].writeJSON(at: idx, to: &sink)
            }
            sink.write("}")

        case .map:
            guard children.count == 1, children[0].children.count == 2,
                let offsets = buffer1?.assumingMemoryBound(to: Int32.self)
            else {
                sink.write(Self.unreadable)
                return
            }
            writeEntries(
                children[0], from: Int(offsets[idx]), to: Int(offsets[idx + 1]), into: &sink)
        }
    }

    /// Whether `start..<end` is a run this child actually holds.
    ///
    /// Asked before the range is built, and that is the whole reason it is a
    /// function rather than a guard inside the loops below. `a..<b` traps when
    /// `b < a`, so a `Range` assembled from two offsets and *then* checked is a
    /// check that never runs: the offsets buffer is memory another process wrote,
    /// and a pair that goes backwards would take the window down before anything
    /// could refuse it. Every other bound in this file is read the same way for
    /// the same reason.
    private static func holds(_ child: ColumnBatch, from start: Int, to end: Int) -> Bool {
        start >= 0 && end >= start && end <= child.length
    }

    /// A run of a child's values as a JSON array.
    private func writeElements(
        _ element: ColumnBatch, from start: Int, to end: Int, into sink: inout JSONSink
    ) {
        guard Self.holds(element, from: start, to: end) else {
            sink.write(Self.unreadable)
            return
        }
        sink.write("[")
        for (at, index) in (start..<end).enumerated() {
            // Asked before the element is written rather than after it, so that
            // the loop still knows what it is leaving behind: `end - index` is the
            // run this walk turned back from, and it is the number the cell
            // reports. Checked after the write, the budget would still hold — the
            // sink refuses what it cannot afford either way — but the walk would
            // have visited every element of a ten-thousand-element list to write
            // nothing, and the mark would have no number to give.
            guard !sink.isFull else {
                sink.abandon(end - index)
                break
            }
            if at > 0 { sink.write(",") }
            element.writeJSON(at: index, to: &sink)
        }
        sink.write("]")
    }

    /// A run of a map's entries as a JSON object.
    ///
    /// The entries struct is stepped over rather than written out: a map spelled
    /// as `[{"key":"x","value":1}]` would be this reader describing Arrow's
    /// layout, where `{"x":1}` is the value the database holds. A key that is not
    /// a string is quoted anyway, because a JSON key has to be one — the same
    /// answer Cassandra's driver gives for a map keyed by a number.
    private func writeEntries(
        _ entries: ColumnBatch, from start: Int, to end: Int, into sink: inout JSONSink
    ) {
        guard Self.holds(entries, from: start, to: end) else {
            sink.write(Self.unreadable)
            return
        }
        let keys = entries.children[0]
        let values = entries.children[1]
        sink.write("{")
        for (at, index) in (start..<end).enumerated() {
            guard !sink.isFull else {
                sink.abandon(end - index)
                break
            }
            if at > 0 { sink.write(",") }
            // The entries array's own offset, because these two are its
            // children and the walk is stepping over it rather than through it.
            let logical = entries.offset + index
            sink.write(Self.quoted(keys.text(at: logical)))
            sink.write(":")
            values.writeJSON(at: logical, to: &sink)
        }
        sink.write("}")
    }

    /// One value that is not nested, as a JSON literal.
    ///
    /// Numbers and booleans bare; everything else quoted, because JSON has no
    /// spelling of its own for a timestamp, a date or a blob and the string is
    /// the same text the grid draws for a column of them. A decimal is quoted
    /// too, and that is a decision rather than an omission: JSON's number grammar
    /// would take it, and every reader on the other side would parse it through a
    /// double and lose the exactness the column was declared for.
    private func scalarJSON(at i: Int) -> String {
        switch kind {
        case .bool, .int8, .int16, .int32, .int64:
            return text(at: i)
        // Bare like the signed widths. A `uint64` past `Int64.max` is written
        // out in full, which is a number JSON's grammar takes and a reader
        // parsing into a signed 64-bit integer will refuse — the honest failure,
        // against a quoted string that every reader would take as text.
        case .uint8, .uint16, .uint32, .uint64:
            return text(at: i)
        case .float32, .float64:
            let written = text(at: i)
            // JSON has no infinity and no NaN, and a document holding the bare
            // word would not parse. Strings, which is what every JSON encoder
            // does with the three of them.
            return ["inf", "-inf", "nan", "-nan"].contains(written)
                ? Self.quoted(written) : written
        default:
            return Self.quoted(text(at: i))
        }
    }

    /// One string as a JSON literal.
    ///
    /// Written out rather than handed to `JSONSerialization`, which allocates an
    /// object per call and, before its fragment option, would not encode a bare
    /// string at all. The control characters are the ones that matter: a `text`
    /// column holding a newline inside a struct would otherwise end the line and
    /// leave a document the viewer refuses to lay out.
    private static func quoted(_ text: String) -> String {
        var out = "\""
        out.reserveCapacity(text.utf8.count + 2)
        for scalar in text.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if scalar.value < 0x20 {
                    out += String(format: "\\u%04x", scalar.value)
                } else {
                    out.unicodeScalars.append(scalar)
                }
            }
        }
        return out + "\""
    }

    /// See `ArrowTable.bytes(row:column:)`.
    func bytes(at i: Int) -> [UInt8]? {
        guard case .binary = kind, !isNull(i) else { return nil }
        let idx = offset + i
        guard let offsets = buffer1?.assumingMemoryBound(to: Int32.self),
            let data = buffer2?.assumingMemoryBound(to: UInt8.self)
        else { return nil }
        let start = Int(offsets[idx])
        let end = Int(offsets[idx + 1])
        guard end > start else { return [] }
        return Array(UnsafeBufferPointer(start: data + start, count: end - start))
    }

    private func load<T>(_ type: T.Type, _ idx: Int) -> T {
        guard let b = buffer1 else {
            return withUnsafeTemporaryAllocation(of: T.self, capacity: 1) { $0[0] }
        }
        return b.load(fromByteOffset: idx * MemoryLayout<T>.stride, as: T.self)
    }

    // Formatting kept minimal for Phase 0: correct enough to eyeball, cheap
    // enough not to distort the frame-time measurement.

    private static func dateText(days: Int32) -> String {
        let secs = TimeInterval(days) * 86400
        return isoDate.string(from: Date(timeIntervalSince1970: secs))
    }

    private static func timeText(_ value: Int64, unit: ArrowTable.TimeUnit) -> String {
        let (seconds, fraction) = Self.split(value, unit: unit)
        let clock = String(
            format: "%02lld:%02lld:%02lld", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
        return clock + Self.fractionText(fraction, unit: unit)
    }

    /// A span of time, which is not a reading on a clock.
    ///
    /// MySQL's `TIME` is what reaches here: a signed interval running to
    /// ±838:59:59 — `TIMEDIFF()` returns one — which is why the driver maps it
    /// to a duration and not to `time64`, elapsed time since midnight, with no
    /// sign and nothing past 24 hours. So the hours are neither taken modulo a
    /// day nor capped at two digits: the third digit is the value, and the minus
    /// sign is a direction rather than a defect.
    ///
    /// The fraction is written only when there is one. A `TIME(6)` column of
    /// whole seconds would otherwise be six columns of zeros in every row,
    /// pushing the part that differs off the width the values need.
    private static func durationText(micros: Int64) -> String {
        // Magnitude rather than `abs`, which traps on `Int64.min` — a value no
        // server sends and every buffer can hold.
        let total = micros.magnitude
        let seconds = total / 1_000_000
        let span =
            (micros < 0 ? "-" : "")
            + String(
                format: "%02llu:%02llu:%02llu",
                seconds / 3600, (seconds % 3600) / 60, seconds % 60)
        let fraction = total % 1_000_000
        return fraction == 0 ? span : span + String(format: ".%06llu", fraction)
    }

    private static func timestampText(_ value: Int64, unit: ArrowTable.TimeUnit) -> String {
        let (seconds, fraction) = Self.split(value, unit: unit)
        let clock = isoDateTime.string(from: Date(timeIntervalSince1970: TimeInterval(seconds)))
        return clock + Self.fractionText(fraction, unit: unit)
    }

    /// One datetime as whole seconds and the fraction left over, both exact.
    ///
    /// The division happens in integers and before anything becomes a
    /// `TimeInterval`, which is a `Double` with 53 bits of mantissa: a
    /// nanosecond count in this century needs 61, so converting first and
    /// dividing after rounds the value before the formatter has seen it. At
    /// microseconds that was already within a hair of showing.
    ///
    /// Floored rather than truncated, so that an instant before 1970 keeps
    /// counting forward from the second below it. `-1_500_000` microseconds is
    /// half a second before `23:59:59`, not half a second after it, and
    /// truncation toward zero would draw it a whole second late.
    private static func split(_ value: Int64, unit: ArrowTable.TimeUnit) -> (
        seconds: Int64, fraction: Int64
    ) {
        let per = unit.perSecond
        var seconds = value / per
        var fraction = value % per
        if fraction < 0 {
            seconds -= 1
            fraction += per
        }
        return (seconds, fraction)
    }

    /// The fraction of a second, written only when there is one.
    ///
    /// The same rule `durationText` states: a `TIMESTAMP_NS` column of whole
    /// seconds would otherwise be nine columns of zeros in every row, pushing
    /// the part that differs off the width the values need. When there is a
    /// fraction it is written to the column's full width rather than trimmed —
    /// the declared unit is part of what the column means, and `.5` and
    /// `.500000` are the same number in two different columns.
    private static func fractionText(_ fraction: Int64, unit: ArrowTable.TimeUnit) -> String {
        guard fraction != 0 else { return "" }
        return "." + String(format: "%0*lld", unit.fractionDigits, fraction)
    }

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
        if precision == ArrowTable.normalizedPrecision && scale == ArrowTable.normalizedScale {
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

/// A JSON document being written under a character budget.
///
/// The budget is the memory rule of this whole path, not tidiness. Nothing here
/// holds a child array or a decoded value: a nested cell is walked when
/// somebody looks at it and the string is thrown away with the frame. What that
/// leaves exposed is the walk itself — a `LIST` of ten thousand elements is ten
/// thousand appends whether or not anyone can read the result — so the walk
/// carries its own stopping condition rather than trusting the caller's cell to
/// be narrow.
///
/// A struct rather than a `String` and an `Int` threaded by hand, because the
/// check for "have I written enough" has to happen at every level of the
/// recursion and `String.count` is a walk of its own.
private struct JSONSink {
    private var written = ""
    private var budget: Int
    /// Whether the walk stopped short. Read at the top of every loop above, so
    /// that a list past its budget is turned back from rather than visited to the
    /// end writing nothing.
    private(set) var isFull = false
    /// Values the walk turned back from, summed over the runs it unwound through.
    private(set) var abandoned = 0

    init(budget: Int) {
        self.budget = max(budget, 0)
    }

    /// There is deliberately no `isFull` fast path here. Once the budget is spent
    /// it stays at zero, so the guard below takes `prefix(0)` and appends nothing,
    /// and a build with the fast path and a build without it produce the same
    /// string for every input. A branch nothing can tell apart is a branch this
    /// file would rather not carry.
    mutating func write(_ piece: String) {
        let count = piece.count
        guard count <= budget else {
            written += piece.prefix(budget)
            budget = 0
            isFull = true
            return
        }
        written += piece
        budget -= count
    }

    /// Records a run the walk decided not to visit.
    mutating func abandon(_ count: Int) {
        abandoned += max(count, 0)
    }

    /// What was written, and how much of the value is not in it.
    ///
    /// The ellipsis is load bearing: a truncated document does not parse, and
    /// the value viewer's fallback for one that does not parse is to show it as
    /// stored. Without a mark, a cut list would read as a whole one.
    ///
    /// The count beside it is the same obligation `RenderedValue.clipped` meets
    /// when it writes "first 128,000 of 500,000 characters", and the reason the
    /// loops above check their budget before each element rather than after:
    /// `[0,1,2,3,4…` cannot tell a five-element list from a five-thousand-element
    /// one, and the wrong reading is the expensive one. Zero is spelled as a bare
    /// ellipsis, for the cut that landed on a field's name rather than inside a
    /// run — there is nothing counted to report, and "(0 more)" would be a lie
    /// about a value that has more.
    var text: String {
        guard isFull else { return written }
        return abandoned > 0 ? written + "… (\(abandoned) more)" : written + "…"
    }
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
