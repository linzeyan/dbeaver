import Foundation

/// Executable checks for the query plan, run by `--verify-plan`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link. That link is the point here —
/// the tree these checks read is built by the core and crosses the FFI boundary,
/// and half of what can go wrong is on the way across.
@MainActor
enum PlanChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkADocumentCrossesTheBoundaryAsTheTreeTheServerDescribed()
        checkTheTreeFlattensInTheOrderItHangs()
        checkTheBarIsDrawnFromTheCostAStepAdds()
        checkAServerThatCostsNothingDrawsNoBars()
        checkEveryTopLevelStepIsKept()
        checkRowsThatAreNotAPlanLeaveTheGridAlone()
        checkAProductThatRidesAnotherDriverStillGetsAProsePlan()
        if failures == 0 {
            fputs("plan: all checks passed\n", stderr)
        } else {
            fputs("plan: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The whole path, from the cell a grid is holding to the rows a pane draws.
    ///
    /// The document is PostgreSQL 17's own answer for a grouped join, trimmed of
    /// nothing: what this has to survive is the shape a server actually sends,
    /// and a fixture written to suit the reader would stop testing that the day
    /// the reader changed.
    private static func checkADocumentCrossesTheBoundaryAsTheTreeTheServerDescribed() {
        guard let plan = postgres() else { return }
        expect(plan.rows.count, 4, "four steps came back")
        expect(plan.rows.map(\.label), ["Limit", "Sort", "Aggregate", "Seq Scan"], "in order")
        expect(plan.rows[0].node.rows, 10, "the planner's row estimate survived the boundary")
        expect(plan.rows[0].node.cost, 1051.94, "and its cost")
        // The details are what the server said about the step and the reason a
        // plan is worth drawing at all: a Seq Scan with a filter on it is the
        // answer to why a query is slow.
        expect(
            plan.rows[3].detail.contains("Relation Name: bench_child"), true,
            "the scan says what it reads: \(plan.rows[3].detail)")
        expect(
            plan.rows[3].detail.contains("Filter: (int_val > 100)"), true,
            "and what it filters by: \(plan.rows[3].detail)")
        // Lines saying no about an option that was never in question are what a
        // reader would be scrolling past to reach the two above.
        expect(
            plan.rows[3].detail.contains("false"), false,
            "and nothing it did not do: \(plan.rows[3].detail)")
    }

    /// Depth is the only place the tree's shape survives the flattening, so a
    /// list of the right labels in the right order can still be the wrong plan.
    private static func checkTheTreeFlattensInTheOrderItHangs() {
        guard let plan = postgres() else { return }
        expect(plan.rows.map(\.depth), [0, 1, 2, 3], "each step sits under the one before it")

        // A step with two children, which the document above has none of: a
        // flattening that walked breadth-first or that indented by position
        // would agree with that plan and not with this one.
        guard
            let forked = tree(
                """
                [{"Plan": {"Node Type": "Hash Join", "Total Cost": 9,
                  "Plans": [{"Node Type": "Seq Scan", "Total Cost": 4},
                            {"Node Type": "Hash", "Total Cost": 3,
                             "Plans": [{"Node Type": "Index Scan", "Total Cost": 1}]}]}}]
                """)
        else { return }
        expect(
            forked.rows.map(\.label), ["Hash Join", "Seq Scan", "Hash", "Index Scan"],
            "a child's own children come before its sibling")
        expect(forked.rows.map(\.depth), [0, 1, 1, 2], "and the sibling goes back out one level")
    }

    /// What the bars mean. The number PostgreSQL prints includes everything
    /// below, so bars drawn from it grow monotonically towards the root and point
    /// at the step every reader already knew about.
    private static func checkTheBarIsDrawnFromTheCostAStepAdds() {
        guard let plan = postgres() else { return }
        // 1051.94 at the root, 1051.92 below it: the Limit adds almost nothing,
        // and a bar drawn from the printed cost would make it the tallest.
        //
        // Compared as the pane writes them rather than as doubles: 943.87 less
        // 100.00 is not 843.87 in binary floating point, and the number this has
        // to be right about is the one somebody reads.
        expect(cost(plan.rows[0]), "0.02", "the root adds two hundredths")
        expect(cost(plan.rows[2]), "843.87", "the aggregate is where the cost is")
        expect(plan.widestCost.map(QueryPlan.cost), "843.87", "so the scale is the aggregate's")
        expect(plan.share(of: plan.rows[2]), 1, "which fills the bar")
        expect(plan.share(of: plan.rows[0]) < 0.01, true, "and the root's is invisible")
        // Every share is a fraction of the widest, so none can exceed it.
        for row in plan.rows {
            expect(plan.share(of: row) <= 1, true, "\(row.label) is on the scale")
        }
    }

    /// SQLite publishes no estimates at all. Bars all the same length would be a
    /// claim about cost that nothing said, so there are none.
    private static func checkAServerThatCostsNothingDrawsNoBars() {
        guard let plan = sqlite() else { return }
        expect(plan.widestCost, nil, "nothing to draw a scale from")
        for row in plan.rows {
            expect(plan.share(of: row), 0, "\(row.label) draws no bar")
            expect(row.node.rows, nil, "and claims no row count")
        }
    }

    /// SQLite answers an ordinary statement with several steps at the top level,
    /// and a reader who saw only the first would be reading a different plan.
    private static func checkEveryTopLevelStepIsKept() {
        guard let plan = sqlite() else { return }
        expect(plan.rows.count, 3, "three steps")
        expect(plan.rows.map(\.depth), [0, 0, 0], "none of them under another")
        expect(
            plan.rows[2].label, "USE TEMP B-TREE FOR ORDER BY",
            "including the last, which is the one a truncating reader would lose")
    }

    /// Nothing to draw is not a failure. The rows are still what the server sent
    /// and they are still on screen — what must not happen is a pane that clears
    /// the grid and shows an empty tree.
    private static func checkRowsThatAreNotAPlanLeaveTheGridAlone() {
        expect(Database.plan(product: "PostgreSQL", rows: []), nil, "no rows, no plan")
        expect(
            Database.plan(product: "PostgreSQL", rows: [["ERROR"]]), nil,
            "a cell that is not a document")
        expect(Database.plan(product: "PostgreSQL", rows: [["[]"]]), nil, "an empty document")
        expect(
            Database.plan(product: "CockroachDB", rows: [[Self.document]]), nil,
            "a product with no such form, whatever the rows hold")
        expect(
            Database.plan(product: "SQLite", rows: [["one cell"]]), nil, "rows of the wrong width")
    }

    /// The reason the plan is keyed by product and the prose `EXPLAIN` is keyed
    /// by scheme.
    ///
    /// CockroachDB arrives through the PostgreSQL driver and rejects
    /// `EXPLAIN (FORMAT JSON)` outright. Offering it there would replace a plan
    /// somebody has today with a syntax error — so the menu item stays, sends the
    /// word every product of that dialect takes, and the pane shows the rows.
    private static func checkAProductThatRidesAnotherDriverStillGetsAProsePlan() {
        expect(Database.planPrefix(for: "PostgreSQL"), "EXPLAIN (FORMAT JSON)", "PostgreSQL asks")
        expect(Database.planPrefix(for: "SQLite"), "EXPLAIN QUERY PLAN", "SQLite asks")
        for product in ["CockroachDB", "GreptimeDB", "TiDB", "MySQL", ""] {
            expect(Database.planPrefix(for: product), nil, "\(product) is not asked")
        }

        let model = makeModel(scheme: "postgres", product: "CockroachDB")
        let request = model.explainRequest
        expect(request?.prefix, "EXPLAIN", "so it is sent the word its dialect takes")
        expect(request?.drawable, false, "and nothing tries to draw the answer")
        expect(model.canExplainStatement, true, "the command is still offered")

        let postgres = makeModel(scheme: "postgres", product: "PostgreSQL")
        expect(
            postgres.explainRequest?.prefix, "EXPLAIN (FORMAT JSON)",
            "while PostgreSQL is asked the way that can be drawn")
        expect(postgres.explainRequest?.drawable, true, "and the answer is drawn")

        // A database with no prefix at all: SQL Server asks with a session
        // setting either side of the statement, which no prefix can be.
        let mssql = makeModel(scheme: "sqlserver", product: "SQL Server")
        expect(mssql.explainRequest?.prefix, nil, "SQL Server has no request to make")
        expect(mssql.canExplainStatement, false, "so the command is not offered")
    }

    // MARK: - Fixtures

    /// PostgreSQL 17's answer for a grouped join over the benchmark tables.
    private static let document = """
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 1051.92,
              "Total Cost": 1051.94,
              "Plan Rows": 10,
              "Plan Width": 18,
              "Plans": [
                {
                  "Node Type": "Sort",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Startup Cost": 1051.92,
                  "Total Cost": 1051.92,
                  "Plan Rows": 5000,
                  "Plan Width": 18,
                  "Sort Key": ["(count(*)) DESC"],
                  "Plans": [
                    {
                      "Node Type": "Aggregate",
                      "Strategy": "Hashed",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Startup Cost": 893.87,
                      "Total Cost": 943.87,
                      "Plan Rows": 5000,
                      "Plan Width": 18,
                      "Group Key": ["w.name"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Relation Name": "bench_child",
                          "Alias": "c",
                          "Startup Cost": 0.00,
                          "Total Cost": 100.00,
                          "Plan Rows": 5000,
                          "Plan Width": 4,
                          "Filter": "(int_val > 100)"
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
        """

    private static func postgres() -> QueryPlan? { tree(document) }

    /// A step's own cost as the pane writes it, or a word that is not a number
    /// where there is none — so that a missing cost fails a comparison rather
    /// than matching some other step's.
    private static func cost(_ row: QueryPlan.Row) -> String {
        row.node.selfCost.map(QueryPlan.cost) ?? "none"
    }

    private static func tree(_ document: String) -> QueryPlan? {
        guard let nodes = Database.plan(product: "PostgreSQL", rows: [[document]]) else {
            failures += 1
            fputs("plan FAIL: the core read no plan out of the document\n", stderr)
            return nil
        }
        return QueryPlan(nodes)
    }

    /// SQLite's answer for a grouped join, as its four columns arrive.
    private static func sqlite() -> QueryPlan? {
        let rows = [
            ["9", "0", "210", "SCAN c USING COVERING INDEX c_parent"],
            ["11", "0", "45", "SEARCH w USING INTEGER PRIMARY KEY (rowid=?)"],
            ["16", "0", "0", "USE TEMP B-TREE FOR ORDER BY"]
        ]
        guard let nodes = Database.plan(product: "SQLite", rows: rows) else {
            failures += 1
            fputs("plan FAIL: the core read no plan out of SQLite's rows\n", stderr)
            return nil
        }
        return QueryPlan(nodes)
    }

    /// A window whose connection is the product named, with something in the
    /// editor for ⌥⌘E to be about.
    private static func makeModel(scheme: String, product: String) -> AppModel {
        let store = ScratchDefaults.store("verify-plan")
        let model = AppModel(
            history: QueryHistory(defaults: store), favorites: QueryFavorites(defaults: store),
            preferences: Preferences(store: store))
        model.sessions[0].connString = "\(scheme)://someone@example.test/db"
        model.sessions[0].serverProduct = product
        model.activeTab = .query
        model.queryText = "SELECT 1"
        return model
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("plan FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
