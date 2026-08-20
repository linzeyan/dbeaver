import Foundation

/// One column of one row, as the record view lists it.
struct RecordField: Identifiable {
    /// The column's index in the result, which is not its place in the list.
    ///
    /// The two differ the moment a column is hidden, and this is the one that
    /// goes back to `stageEdit`. A field that addressed itself by where it was
    /// drawn would write into the wrong column of the right row — the failure
    /// that looks exactly like working software.
    let column: Int
    let name: String
    /// The column's declared type, or empty where the relation does not name one.
    let type: String
    let value: String
    /// Drawn differently from the empty string, which is a different value and
    /// the whole reason this is carried rather than inferred from `value`.
    let isNull: Bool

    var id: Int { column }
}

/// The arithmetic the record view runs on.
///
/// Split out from the view for the reason `GridAccessibilitySource` is a
/// protocol: a value needs an Arrow table and an Arrow table needs a server,
/// while everything that can actually be wrong here — a column skipped, an
/// index that slid, a row stepped off the end — needs neither.
enum Record {
    /// The fields to list, in the order the grid draws its columns.
    ///
    /// Hidden columns are left out rather than listed empty, so that the record
    /// view and the grid are two readings of one row rather than two answers
    /// about what the row has in it.
    static func fields(
        count: Int, hidden: Set<Int>, describe: (Int) -> RecordField?
    ) -> [RecordField] {
        (0..<count).filter { !hidden.contains($0) }.compactMap(describe)
    }

    /// Where stepping `delta` rows from `row` lands, or nil with no rows to land
    /// on.
    ///
    /// Clamped rather than wrapped: someone holding the down arrow at the last
    /// row expects to stay on it, and a jump back to the first reads as the view
    /// having lost its place rather than as the end of the table.
    ///
    /// `row` is clamped too, not just the result of the step. A selection outlives
    /// the result it was made in — a filter narrowing a table to three rows leaves
    /// a cursor pointing at row nine hundred — and the first arrow key afterwards
    /// has to land somewhere real.
    static func row(_ row: Int, steppedBy delta: Int, rowCount: Int) -> Int? {
        guard rowCount > 0 else { return nil }
        return min(max(row + delta, 0), rowCount - 1)
    }
}
