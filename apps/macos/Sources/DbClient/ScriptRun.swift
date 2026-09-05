import Foundation

/// How much of a script's answer this window holds on to.
///
/// A run of several statements keeps every one of their results at once — that
/// is what the outcome list is for — and nothing closes them until the next run
/// replaces them. Forty SELECTs of a hundred thousand rows is forty result sets
/// alive in one tab, in every tab somebody left a script in, and the ceiling on
/// that has to be a number this file names rather than the size of the database.
///
/// The budget belongs to the run rather than to any statement, and only a run of
/// several ever spends it: ⌘R over one statement — and ⌥⌘E, which is also one —
/// keeps its rows whole, exactly as the Query pane has always promised. Nothing
/// about the single-statement path changes, by construction rather than by care.
///
/// Whole statements, not whole rows. Whether a statement's rows are kept is
/// decided from what the run has kept *before* it is pulled, so a statement is
/// either in the grid entire or not in it at all — half a result set under a grid
/// that does not say so is the class of lie the row count exists to refuse.
/// Deciding first is also what bounds the peak: the batches of a statement being
/// refused are released as they arrive rather than gathered and then dropped. So
/// the most a run holds is the budget plus one statement, and one statement's
/// worth is what ⌘R holds anyway.
enum ScriptRetention {
    /// Rows one run keeps across all of its statements.
    ///
    /// A fifth of the million rows the grid is built to scroll. A script is read
    /// as a list of outcomes with one of its grids open at a time, so what this
    /// number buys is the results kept *behind* the one being read — and past
    /// this it is buying rows nobody scrolls back to.
    static let rowBudget = 200_000

    /// Whether a run of `statements` that has kept `kept` rows keeps the rows of
    /// the statement about to go out.
    static func keepsRows(havingKept kept: Int, of statements: Int) -> Bool {
        statements == 1 || kept < rowBudget
    }
}

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
    /// Returned this many rows, which the run let go instead of keeping.
    ///
    /// Neither an absence nor a failure: the statement ran and the server sent
    /// its answer, and what is gone is this window's copy of it. Its own case
    /// because the alternatives are both lies — `rows(0)` says the server
    /// returned nothing, and `rows(n)` beside an empty grid says this window has
    /// them.
    case released(rows: Int)

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
        case .released(let n): return "\(AppModel.pluralized(n, "row")) not kept"
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
    /// The plan in those rows, where this step was a run that asked for one and
    /// the core could read the answer.
    ///
    /// On the step rather than on the model, for the reason `result` is: a run of
    /// several statements can hold a plan for one of them and rows for the rest,
    /// and moving between them must not need anything re-read.
    let plan: QueryPlan?

    init(
        id: Int, sql: String, range: Range<Int>, summary: String,
        outcome: StatementOutcome, result: ResultSet, plan: QueryPlan? = nil
    ) {
        self.id = id
        self.sql = sql
        self.range = range
        self.summary = summary
        self.outcome = outcome
        self.result = result
        self.plan = plan
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
        case .released(let n):
            // Names the way back rather than only the limit. The rows are one
            // ⌘R away — the statement is still in the buffer, on the connection
            // that answered it — and a note that only said what was refused
            // would leave somebody looking for a setting there isn't one of.
            return "This statement returned \(AppModel.pluralized(n, "row")). "
                + "The run had already kept "
                + "\(AppModel.pluralized(ScriptRetention.rowBudget, "row")) from the "
                + "statements above it, so these were let go rather than held in "
                + "memory. Run this statement on its own to see them."
        }
    }
}
