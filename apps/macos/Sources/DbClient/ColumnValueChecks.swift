import CDbFfi
import Foundation

/// Executable checks for the values in a column's own buffers, run by
/// `--verify-column-values`.
///
/// The other half of what `NestedValueChecks` asks about, and it shares that
/// file's `Arena`: there the reader walks into children, here it reads a
/// buffer at a stride the format string decided.
///
/// Seven formats reach this reader that it used to have no rule for, and the
/// symptom was the same for all of them — the cell drew `<L>`, the format
/// string itself, which is what `unsupported` renders. None of the seven is
/// exotic in a MySQL schema: `TINYINT`, every `UNSIGNED` column, `BIT(2..64)`
/// and `TIME` all land here, so a `BIGINT UNSIGNED` id column showed its type
/// in the header and `<L>` in every row.
///
/// The cases below are about the two ways that could have been fixed wrongly.
/// A width read at the wrong stride reads a plausible number out of the middle
/// of two values; an unsigned width read as signed prints a negative id. Both
/// are worse than `<L>`, which at least could not be mistaken for the data.
enum ColumnValueChecks {
    private static var failures = 0

    static func run() -> Bool {
        MainActor.assumeIsolated { cases() }
        if failures == 0 {
            fputs("column-values: all checks passed\n", stderr)
        } else {
            fputs("column-values: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    @MainActor private static func cases() {
        failures = 0
        checkEachFormatIsReadAsTheTypeArrowSpells()
        checkASignedByteKeepsItsSign()
        checkAnUnsignedWidthIsNotReadAsSigned()
        checkTheWidestUnsignedValueSurvives()
        checkEveryIntegerWidthIsLinedUpForScanning()
        checkADurationIsWrittenAsASpanAndNotAClock()
        checkAColumnOfNothingButNullSaysSoWithoutABitmap()
        // After the cases and not inside them, for the reason `keep` gives.
        for arena in arenas { arena.release() }
        arenas.removeAll()
    }

    // MARK: - Cases

    /// The format strings, which are where a typo hides in plain sight: Arrow
    /// spells the unsigned widths as the capitals of the signed ones, so `L` and
    /// `l` differ by a shift key and by the top half of a 64-bit range.
    @MainActor private static func checkEachFormatIsReadAsTheTypeArrowSpells() {
        let expected = [
            ("c", "int8"), ("s", "int16"), ("i", "int32"), ("l", "int64"),
            ("C", "uint8"), ("S", "uint16"), ("I", "uint32"), ("L", "uint64"),
            ("tDu", "duration"), ("n", "null")
        ]
        for (format, label) in expected {
            let table = read(format: format, values: [UInt8]())
            expect(table.columns[0].kind.label, label, "the type \(format) is read as")
        }
    }

    @MainActor private static func checkASignedByteKeepsItsSign() {
        let table = read(format: "c", values: [Int8.min, -1, 0, Int8.max])
        expect(table.text(row: 0, column: 0), "-128", "the lowest signed byte")
        expect(table.text(row: 1, column: 0), "-1", "a negative byte")
        expect(table.text(row: 3, column: 0), "127", "the highest signed byte")
    }

    /// The failure this is really about: every one of these values has its top
    /// bit set, and read at its own width as a signed integer each would print a
    /// negative number for a column that cannot hold one.
    @MainActor private static func checkAnUnsignedWidthIsNotReadAsSigned() {
        let bytes = read(format: "C", values: [UInt8.max, 128])
        expect(bytes.text(row: 0, column: 0), "255", "the widest byte")
        expect(bytes.text(row: 1, column: 0), "128", "a byte past the signed half")

        let shorts = read(format: "S", values: [UInt16.max, 32_768])
        expect(shorts.text(row: 0, column: 0), "65535", "the widest short")
        expect(shorts.text(row: 1, column: 0), "32768", "a short past the signed half")

        let ints = read(format: "I", values: [UInt32.max, 2_147_483_648])
        expect(ints.text(row: 0, column: 0), "4294967295", "the widest int")
        expect(ints.text(row: 1, column: 0), "2147483648", "an int past the signed half")
    }

    /// `BIGINT UNSIGNED` is an ordinary auto-increment id in MySQL, and its
    /// range is twice `Int64`'s. This is the value the driver widens the column
    /// for; a reader that took it back to `Int64` would undo that at the last
    /// step and print -1.
    @MainActor private static func checkTheWidestUnsignedValueSurvives() {
        let table = read(format: "L", values: [UInt64.max, UInt64(Int64.max) + 1, 7])
        expect(table.text(row: 0, column: 0), "18446744073709551615", "the widest unsigned value")
        expect(
            table.text(row: 1, column: 0), "9223372036854775808",
            "the first value past what Int64 holds")
        expect(table.text(row: 2, column: 0), "7", "an ordinary id in the same column")
    }

    /// And they are all right-aligned, because a column of numbers is read by
    /// comparing digits in the same place.
    @MainActor private static func checkEveryIntegerWidthIsLinedUpForScanning() {
        for format in ["c", "C", "S", "I", "L"] {
            let table = read(format: format, values: [UInt8]())
            expect(table.columns[0].kind.isNumeric, true, "\(format) is compared by magnitude")
        }
    }

    /// A duration is a span, not a reading on a clock.
    ///
    /// MySQL's `TIME` runs to ±838:59:59, which is why the driver maps it to a
    /// duration rather than to `time64`. Every value here is one `time64` would
    /// have drawn wrongly: the sign dropped, the hours wrapped into a day, or
    /// the fraction lost.
    @MainActor private static func checkADurationIsWrittenAsASpanAndNotAClock() {
        let spans: [Int64] = [
            -span(838, 59, 59),
            span(13, 45, 56, micros: 123_456),
            0,
            span(25, 0, 0),
            -500_000
        ]
        let table = read(format: "tDu", values: spans)
        expect(table.text(row: 0, column: 0), "-838:59:59", "MySQL's largest negative TIME")
        expect(table.text(row: 1, column: 0), "13:45:56.123456", "a span with a fraction")
        // No fraction where there is none: six columns of zeros in every row
        // would push the part that differs out of the width the values need.
        expect(table.text(row: 2, column: 0), "00:00:00", "a span of no time at all")
        expect(table.text(row: 3, column: 0), "25:00:00", "a span longer than a day")
        expect(table.text(row: 4, column: 0), "-00:00:00.500000", "less than a second, backwards")
    }

    /// A null column has no buffers at all, not even the bitmap that says its
    /// values are absent — so the reader has to answer from the type.
    ///
    /// Drawn as a blank instead, the column would be saying every row holds an
    /// empty string, which is a value. `SELECT NULL` is where this arrives.
    @MainActor private static func checkAColumnOfNothingButNullSaysSoWithoutABitmap() {
        let table = read(format: "n", rows: 3)
        for row in 0..<3 {
            expect(table.isNull(row: row, column: 0), true, "row \(row) of a null column")
        }
        // And the column beside it is not swept up in that: the fixture's second
        // column is a plain int32 with no bitmap, which is Arrow for "all
        // present".
        expect(table.isNull(row: 0, column: 1), false, "an ordinary column beside it")
    }

    /// A span written the way it is read — `838:59:59`, and a fraction where
    /// there is one — in the microseconds a duration column counts.
    ///
    /// A function rather than the arithmetic inline, because an array literal of
    /// those products cannot be type-checked in reasonable time by the compiler
    /// on the CI runner: the local toolchain compiles it and the gate does not,
    /// which is the failure this project keeps CI for. It reads better besides —
    /// the sum is what the case is about, and nobody should have to do it.
    private static func span(_ hours: Int64, _ minutes: Int64, _ seconds: Int64, micros: Int64 = 0)
        -> Int64
    {
        (hours * 3600 + minutes * 60 + seconds) * 1_000_000 + micros
    }

    // MARK: - Harness

    /// One column of `values` laid out as Arrow lays them out, with a plain
    /// `int32` beside it.
    ///
    /// The constraint is what keeps the fixture honest. An unconstrained `T`
    /// lets a literal whose element type nobody wrote down settle on `[Any]`,
    /// and the buffer then holds 32-byte existential boxes rather than numbers:
    /// row 0 still reads, because its payload is stored inline at the front of
    /// the box, and every row after it is whatever the heap held next. That is
    /// a fixture lying about the data, which is the one failure a check cannot
    /// report.
    @MainActor private static func read<T: FixedWidthInteger>(format: String, values: [T])
        -> ArrowTable
    {
        let arena = keep()
        return arena.table(
            schema: arena.schema(format: format, name: "value"),
            array: arena.array(length: values.count, buffers: [nil, arena.buffer(values)]))
    }

    /// A column with no buffers of its own, which only the null type has.
    @MainActor private static func read(format: String, rows: Int) -> ArrowTable {
        let arena = keep()
        return arena.table(
            schema: arena.schema(format: format, name: "value"),
            array: arena.array(length: rows, buffers: []))
    }

    /// An arena that outlives the table built from it.
    ///
    /// The batch reads through these buffers for as long as the table is alive,
    /// so an arena released at the end of the case that built it would leave the
    /// reader pointing at returned memory — and a check that reads freed memory
    /// can pass on any given run.
    @MainActor private static func keep() -> Arena {
        let arena = Arena()
        arenas.append(arena)
        return arena
    }

    @MainActor private static var arenas: [Arena] = []

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("column-values FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
