import Foundation

/// What one statement of a run did.
///
/// Four outcomes rather than a flag and a number. A run that stops partway has
/// to distinguish the statement that failed from the ones that never got the
/// chance: a list where the last three look like the first two, under a banner
/// reading "failed", claims work happened that did not — the same class of lie
/// as a row count quietly meaning "the first hundred thousand".
@MainActor
enum StatementOutcome: Equatable {
    /// Returned a result set holding this many rows. Zero is a real result set
    /// with no rows in it, which is not the same answer as having none.
    case rows(Int)
    /// Returned no result set. The count is what the server said it affected —
    /// an UPDATE's matched rows, and zero for a CREATE, which affected nothing
    /// and happened all the same.
    case completed(affected: Int)
    /// Failed, and stopped the run here.
    case failed(String)
    /// Stopped on request, and ended the run here.
    ///
    /// Separate from `failed` because it is not one. The server's wording for a
    /// cancellation is an error message, and a row reading "failed" in red for
    /// the button the user just pressed sends them looking for a fault in their
    /// SQL.
    case cancelled
    /// Never ran, because a statement before it failed.
    case notRun

    /// What the outcome column reads. Short on purpose: it shares a row with the
    /// statement itself, which is the part being scanned.
    var label: String {
        switch self {
        case .rows(let n): return AppModel.pluralized(n, "row")
        case .completed(let n):
            return n == 0 ? "no rows" : "\(AppModel.pluralized(n, "row")) affected"
        case .failed: return "failed"
        case .cancelled: return "cancelled"
        case .notRun: return "not run"
        }
    }

    /// Whether this step has rows for the grid to show. False for a statement
    /// that returned none, which needs a sentence instead — an empty grid with
    /// no columns reads as a query that broke rather than as an UPDATE.
    var hasGrid: Bool {
        if case .rows = self { return true }
        return false
    }
}

/// One statement of a run: what was sent, and what came back.
///
/// A run of one is what ⌘R makes, a run of five is what ⌥⌘R makes, and the same
/// type describes both — so a single statement keeps the pane it has always had
/// and a script gets somewhere for its other four results to go.
@MainActor
final class ScriptStep: Identifiable {
    /// Position in the run, counted from 1 the way the editor's corner counts.
    let id: Int
    /// The statement as sent.
    let sql: String
    /// Where the statement sits in the buffer, so a failure can move the caret.
    let range: Range<Int>
    /// What the status bar reads while this step is the selected one.
    let summary: String
    let outcome: StatementOutcome
    /// This step's rows. Its own result rather than a share of one: choosing
    /// another step out of the list must not re-run anything, so each step goes
    /// on owning the Arrow batches it was handed.
    let result: ResultSet

    init(
        id: Int, sql: String, range: Range<Int>, summary: String,
        outcome: StatementOutcome, result: ResultSet
    ) {
        self.id = id
        self.sql = sql
        self.range = range
        self.summary = summary
        self.outcome = outcome
        self.result = result
    }

    /// The statement on one line, for the list.
    ///
    /// Whitespace is collapsed rather than trusted to `lineLimit`, which shows
    /// the first line and hides the rest — and the first line of a formatted
    /// statement is often just `SELECT`. Collapsed, the row leads with the part
    /// that tells one statement from another.
    var preview: String {
        sql.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }

    /// What the pane says in place of a grid, for a step that has no rows to
    /// show. Spelled out rather than left to the list's short label, because
    /// this is the whole answer for that statement and it has a pane to fill.
    var note: String {
        switch outcome {
        case .rows: return ""
        case .completed(let n):
            return n == 0
                ? "This statement returned no rows."
                : "This statement returned no rows, and affected "
                    + "\(AppModel.pluralized(n, "row"))."
        case .failed(let message): return message
        case .cancelled:
            // Says what happened to the database, because that is the question a
            // cancel leaves behind. A statement stopped mid-flight is rolled
            // back by the server; the ones the run had already sent are not, and
            // nothing here wraps a script in a transaction that would undo them.
            return "This statement was stopped before it finished, and the server "
                + "rolled it back. Statements the run had already sent still happened."
        case .notRun:
            // "Stopped above" rather than "failed above": a run also stops when
            // it is cancelled, and a row that names the wrong reason for its own
            // silence is the kind of small lie this list exists to avoid.
            return "This statement did not run — the run stopped at the "
                + "statement above."
        }
    }
}
