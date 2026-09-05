import Foundation

/// Executable checks for what a run keeps, run by `--verify-script-run`.
///
/// The ceiling this pins cannot be reached by a check that runs a script: the
/// statements go out over a live connection, and the run loop that spends the
/// budget is inside a closure on the core queue. So the decision is a pure
/// function of two numbers and lives in `ScriptRetention`, and this is where it
/// is held still — a rule reachable only through a database is a rule nobody
/// checks after the day it was written.
///
/// The failure it guards against is invisible: a build that kept everything
/// looks identical on screen to one that does not, right up to the script that
/// makes the window unusable. Nothing about a screenshot, a status line or an
/// outcome row would say which build is which.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum ScriptRunChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        MainActor.assumeIsolated {
            checkOneStatementKeepsEverythingItReturned()
            checkARunOfSeveralStopsKeepingAtTheBudget()
            checkTheBudgetIsSpentInWholeStatements()
            checkAReleasedResultIsItsOwnAnswer()
            checkTheHistoryRecordsWhatTheServerSent()
        }
        if failures == 0 {
            fputs("script-run: all checks passed\n", stderr)
        } else {
            fputs("script-run: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The ceiling

    /// A run of one statement has no ceiling at all.
    ///
    /// This is the load-bearing half. ⌘R and ⌥⌘E each send one statement, and
    /// the Query pane's standing promise is that what is on screen is the whole
    /// of what the statement produced — no LIMIT, no cap, nothing quietly left
    /// behind. A budget that applied to them would break that promise for every
    /// large SELECT anybody has ever run in this pane, and it would do it
    /// silently.
    private static func checkOneStatementKeepsEverythingItReturned() {
        expect(
            ScriptRetention.keepsRows(havingKept: 0, of: 1), true,
            "a lone statement is kept")
        expect(
            ScriptRetention.keepsRows(havingKept: ScriptRetention.rowBudget * 10, of: 1), true,
            "and is still kept at ten times the budget, because the budget is not its business")
    }

    /// A run of several stops keeping once it has kept the budget.
    ///
    /// The boundary is checked from both sides. `<` and `<=` differ by one row
    /// out of two hundred thousand, which is a difference no run will ever
    /// notice — and that is exactly why it has to be written down rather than
    /// left to whoever next reads the line.
    private static func checkARunOfSeveralStopsKeepingAtTheBudget() {
        let budget = ScriptRetention.rowBudget
        expect(
            ScriptRetention.keepsRows(havingKept: 0, of: 2), true,
            "the first statement of a run is always kept")
        expect(
            ScriptRetention.keepsRows(havingKept: budget - 1, of: 2), true,
            "and so is one that begins a row short of the budget")
        expect(
            ScriptRetention.keepsRows(havingKept: budget, of: 2), false,
            "at the budget the run stops keeping")
        expect(
            ScriptRetention.keepsRows(havingKept: budget + 1, of: 2), false,
            "and past it")
    }

    /// The budget is spent in whole statements, so the one that crosses it is
    /// kept entire and the next is refused entire.
    ///
    /// Written as the run's own loop, because the rule is about a sequence and
    /// no single call to it shows that. A ceiling applied per batch instead
    /// would put the first part of a result in the grid with nothing on screen
    /// saying the rest is missing — a row count that means "some of them" is the
    /// lie this whole pane is built to refuse.
    private static func checkTheBudgetIsSpentInWholeStatements() {
        let budget = ScriptRetention.rowBudget
        var kept = 0
        var verdicts: [Bool] = []
        for rows in [budget - 1, 10, 10] {
            let keeping = ScriptRetention.keepsRows(havingKept: kept, of: 3)
            verdicts.append(keeping)
            if keeping { kept += rows }
        }
        expect(
            verdicts, [true, true, false],
            "the statement that crossed the line is kept, and the one after it is not")
        expect(
            kept, budget + 9,
            "kept whole rather than cut at the budget — the run holds it plus one statement")
    }

    // MARK: - What the pane says about it

    /// A released result is neither an empty one nor a failure.
    ///
    /// Three separate claims, because collapsing any of them into an existing
    /// case is the shortcut this outcome exists to prevent: no grid is drawn
    /// (the columns are there and the rows are not, so a grid would be an empty
    /// table under a statement that returned thousands), the row still names the
    /// count the server sent, and the pane's sentence says how to get them back.
    @MainActor private static func checkAReleasedResultIsItsOwnAnswer() {
        let released = StatementOutcome.released(rows: 12_345)
        expect(released.hasGrid, false, "there are no rows here to draw")
        expect(StatementOutcome.rows(12_345).hasGrid, true, "unlike a result that was kept")
        expect(released.label, "12,345 rows not kept", "the count is the server's, and it is said")
        expect(
            StatementOutcome.rows(0).label != released.label, true,
            "and it is not the sentence an empty result gets")

        let step = ScriptStep(
            id: 1, sql: "SELECT * FROM orders", range: 0..<20, summary: "",
            outcome: released, result: ResultSet())
        expect(
            step.note.contains("Run this statement on its own"), true,
            "the note names the way back, not only the limit")
        expect(
            step.note.contains("12,345 rows"), true,
            "and how many rows are on the other side of it")
    }

    /// The history records the rows the server sent, not the ones the pane kept.
    ///
    /// "What did this application run on my database" is a question about the
    /// database. How much of the answer this window had room for is a fact about
    /// this window, and a history that recorded zero rows for a statement that
    /// returned a million would be answering the wrong question in the one place
    /// somebody goes to check what happened.
    @MainActor private static func checkTheHistoryRecordsWhatTheServerSent() {
        expect(
            QueryHistoryOutcome(.released(rows: 900)), .rows(900),
            "recorded as the rows it returned")
        expect(
            QueryHistoryOutcome(.notRun) == nil, true,
            "while a statement the server never saw is still recorded as nothing at all")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("script-run FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
