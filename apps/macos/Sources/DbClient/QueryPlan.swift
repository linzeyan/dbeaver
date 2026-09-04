import Foundation

/// One step of a query plan, as the core read it out of what the server sent.
///
/// A mirror of `dbsql::plan::Plan`, with the field names serde writes so that a
/// rename on either side fails to decode rather than quietly producing an empty
/// tree.
struct PlanNode: Decodable, Hashable {
    /// What this step does, in the server's own words.
    let label: String
    /// The rest of what the server said about it, already written as `Key: value`.
    let detail: [String]
    /// Rows the planner expects out of this step, where it said.
    let rows: Double?
    /// What the step is expected to cost including everything below it.
    let cost: Double?
    /// What this step adds to the cost of the steps feeding it. The core does the
    /// subtraction — see `Plan::self_cost` for why the server's own number is not
    /// the one to draw a bar from.
    let selfCost: Double?
    let children: [PlanNode]

    private enum CodingKeys: String, CodingKey {
        case label, detail, rows, cost, children
        case selfCost = "self_cost"
    }
}

/// A plan, flattened into the rows a pane draws.
///
/// A forest rather than a tree: SQLite answers an ordinary statement with several
/// top-level steps, and the core hands them over as it found them rather than
/// inventing a parent to hold them.
///
/// Flattened once, here, rather than recursed through in the view. A `ForEach`
/// over a nested type either recurses — which costs a view identity per level and
/// makes selection state a per-node problem — or walks the tree again on every
/// redraw. The rows below carry their own depth, which is the whole of what the
/// drawing needs.
struct QueryPlan: Hashable {
    /// One step, and where it sits.
    struct Row: Identifiable, Hashable {
        /// Position in the flattened list, which is also the order the rows draw
        /// in. Stable for as long as the plan is, which is as long as the step
        /// that produced it.
        let id: Int
        /// How many steps this one sits under, which is the whole of what the
        /// drawing needs: the pane indents by it and draws a guide per level, the
        /// way the server's own text output does.
        let depth: Int
        let node: PlanNode

        var label: String { node.label }
        /// What the server said about this step, on one line. Joined rather than
        /// stacked because a step is a row, and a row that grows to eight lines
        /// buries the shape of the tree it belongs to.
        var detail: String { node.detail.joined(separator: " · ") }
    }

    let rows: [Row]

    /// The largest cost any one step adds, which is what a bar is drawn against.
    ///
    /// Nil where the server costed nothing — SQLite publishes no estimates at all
    /// — and the pane draws no bars rather than bars that all mean the same thing.
    let widestCost: Double?

    init(_ roots: [PlanNode]) {
        var rows: [Row] = []
        func walk(_ node: PlanNode, depth: Int) {
            rows.append(Row(id: rows.count, depth: depth, node: node))
            for child in node.children {
                walk(child, depth: depth + 1)
            }
        }
        for root in roots {
            walk(root, depth: 0)
        }
        self.rows = rows
        // Zero is not a scale. A plan where every step costs nothing — which is
        // what a server that costs in whole units says about a trivial statement
        // — would otherwise divide by it.
        let widest = rows.compactMap { $0.node.selfCost }.max() ?? 0
        widestCost = widest > 0 ? widest : nil
    }

    /// How much of the widest bar this step's own cost fills, from 0 to 1.
    ///
    /// Zero where there is nothing to compare against, which is the same answer
    /// as a step that costs nothing — and both draw no bar, so the two cases do
    /// not need telling apart here.
    func share(of row: Row) -> Double {
        guard let widest = widestCost, let cost = row.node.selfCost else { return 0 }
        return min(max(cost / widest, 0), 1)
    }

    /// A count for a step's row, grouped the way this application writes every
    /// other row count.
    @MainActor
    static func number(_ value: Double) -> String {
        AppModel.formatted(Int(value.rounded()))
    }

    /// A cost, which is not a count: PostgreSQL's units are fractional and the
    /// two decimals are the difference between two steps that look alike.
    static func cost(_ value: Double) -> String {
        String(format: "%.2f", value)
    }
}
