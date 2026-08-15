import Foundation

/// Which columns of a browse have been null in every row of it so far.
///
/// It exists to answer the cost of a decision made elsewhere: every MongoDB
/// result carries an `_extra` column, so that a document which does not fit the
/// schema sampled from the first thousand still has somewhere to land. A
/// collection whose documents all have the same shape therefore shows a column
/// that is null in every row, and a grid spends real width drawing it.
///
/// The difficulty is that "every row of it" moves. The rows a browse holds grow
/// as the user scrolls, so a column empty in the first page can hold something
/// in the ninth — and a column that *appears* half a million rows down is worse
/// than one that was there from the start, because the grid the reader has been
/// scanning changes shape underneath them. So this only ever takes columns away,
/// and stops looking after `evidencePages`: the answer is settled early, from
/// enough rows to mean something, and then left alone.
///
/// What that costs is stated plainly because it is the reason the setting reading
/// this is off by default: a value arriving past those pages lands in a column
/// nothing draws. Nothing is lost — the column is still in the result, so Copy
/// and Export carry it — but it is off screen, and only re-reading the relation
/// brings it back.
///
/// A type of its own rather than three fields on the model, so the rule can be
/// checked against a table of literals with no database anywhere near it; see
/// `--verify-preferences`.
struct EmptyColumns {
    /// Pages that get a say. Three, because the question is whether a column is
    /// empty in this relation rather than in this screenful, and one page here
    /// is already a hundred thousand rows — a column still empty after three of
    /// them is empty in every sense a reader cares about.
    static let evidencePages = 3

    /// The columns concluded empty, by index into the result.
    private(set) var columns: Set<Int> = []

    /// Pages weighed so far. Only pages that brought rows count: a fetch that
    /// returned nothing is not evidence about anything.
    private(set) var pagesWeighed = 0

    /// Whether any further page would change the answer.
    var isSettled: Bool { pagesWeighed >= Self.evidencePages }

    /// Takes one page of rows into account.
    ///
    /// The first page with rows in it proposes every column and each page after
    /// it can only take columns away. A result with no rows proposes nothing: a
    /// column no row has contradicted is not a column that is empty, it is a
    /// column nobody has seen — and hiding every column of an empty table would
    /// leave a grid with not even a header to say what the table holds.
    ///
    /// `isNull` is asked per cell rather than handed a table, so that this file
    /// knows nothing about Arrow and the rule can be checked against literals.
    /// It is cheap in the case that dominates: the walk stops at the first row
    /// where a column holds something, which for nearly every column is the
    /// first row of the page. The columns walked in full are the ones this
    /// exists to find.
    mutating func weigh(
        rows: Range<Int>, columnCount: Int, isNull: (_ row: Int, _ column: Int) -> Bool
    ) {
        guard !isSettled, !rows.isEmpty else { return }
        pagesWeighed += 1
        if pagesWeighed == 1 { columns = Set(0..<columnCount) }
        columns = columns.filter { column in
            rows.allSatisfy { isNull($0, column) }
        }
    }

    /// Forgets everything, for a result that has been replaced.
    ///
    /// A new read is new evidence and the columns may not even be the same ones:
    /// a filter changes which rows there are, and another relation changes the
    /// schema. Nothing the previous result concluded applies here.
    mutating func reset() {
        columns = []
        pagesWeighed = 0
    }
}
