import SwiftUI

/// A query plan, as the tree the server described.
///
/// It replaces the grid rather than sitting beside it. The rows a plan arrives in
/// are one JSON document in one cell on PostgreSQL and four columns of ids on
/// SQLite — neither is something a person reads out of a grid, which is why this
/// exists — but they are still what the server said, so the pane keeps a way back
/// to them.
///
/// The cost bars are drawn from each step's *own* cost, not the cumulative number
/// the server prints: see `Plan::self_cost` in the core. A bar drawn from the
/// printed number says the root is the root, which every reader already knows.
struct PlanTree: View {
    let plan: QueryPlan

    var body: some View {
        VStack(spacing: 0) {
            // Two numbers a row with no word on them is two numbers nobody can
            // read. The captions carry the column widths of the rows below, so
            // that a change to one has to be made to the other.
            if plan.widestCost != nil {
                HStack(spacing: Theme.Space.sm) {
                    Spacer(minLength: 0)
                    caption("rows")
                    caption("cost")
                    // Over the bars, which are the same measure drawn twice.
                    Color.clear.frame(width: PlanStepRow.barWidth, height: 1)
                }
                .padding(.horizontal, Theme.Space.sm)
                .padding(.bottom, 2)
                .accessibilityHidden(true)
            }
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(plan.rows) { row in
                        PlanStepRow(row: row, share: plan.share(of: row))
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.bottom, Theme.Space.xs)
            }
        }
        .padding(.top, Theme.Space.xs)
        .background(Theme.Surface.canvas.color)
        .accessibilityLabel("Query plan")
    }

    private func caption(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.micro)
            .foregroundStyle(Theme.Text.tertiary.color)
            .frame(width: PlanStepRow.numberWidth, alignment: .trailing)
    }
}

/// One step of a plan: what it does, what it says about itself, what it costs.
struct PlanStepRow: View {
    let row: QueryPlan.Row
    /// How much of the widest step's cost this one adds, from 0 to 1. Zero draws
    /// no bar, which is also the answer where the server costed nothing at all.
    let share: Double

    /// One indent per level, wide enough for the guide to read as a column and
    /// narrow enough that a plan eight deep still has room for its labels.
    private static let indent: CGFloat = 14
    /// Shared with the captions above the tree, so that a column and the word
    /// over it cannot drift apart.
    static let barWidth: CGFloat = 56
    /// Wide enough for a cost in the millions, which is what a scan of a large
    /// table costs and therefore exactly the plan somebody is reading.
    static let numberWidth: CGFloat = 78

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            // A guide per level this step sits under, drawn for every ancestor
            // rather than only for the last one: the shape being read is "how far
            // in is this", and a single line at the current depth leaves two
            // siblings four levels apart looking equally nested.
            ForEach(0..<row.depth, id: \.self) { _ in
                Rectangle()
                    .fill(Theme.Border.hairline.color)
                    .frame(width: 1)
                    .frame(width: Self.indent, alignment: .leading)
            }
            VStack(alignment: .leading, spacing: 1) {
                Text(row.label)
                    .font(Theme.Typography.bodyEmphasis)
                    .foregroundStyle(Theme.Text.primary.color)
                if !row.detail.isEmpty {
                    Text(row.detail)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.Text.dataMuted.color)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: Theme.Space.sm)
            estimate
        }
        .padding(.horizontal, Theme.Space.sm)
        .padding(.vertical, Theme.Space.xs)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }

    /// What the planner expects of this step, right-aligned so that the numbers
    /// of steps at different depths line up with each other rather than with
    /// their own labels.
    private var estimate: some View {
        HStack(spacing: Theme.Space.sm) {
            if let rows = row.node.rows {
                Text(QueryPlan.number(rows))
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .frame(width: Self.numberWidth, alignment: .trailing)
                    .help("Rows the planner expects out of this step")
            }
            if let cost = row.node.selfCost {
                Text(QueryPlan.cost(cost))
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .frame(width: Self.numberWidth, alignment: .trailing)
                    .help("What this step adds to the cost of the steps feeding it")
                bar
            }
        }
    }

    /// The share as a bar, on a track that stays put so the bars line up.
    ///
    /// Warning-coloured rather than accent-coloured: the step this points at is
    /// the one to look at when a query is slow, and the accent is already what
    /// this window means by "selected".
    private var bar: some View {
        ZStack(alignment: .leading) {
            RoundedRectangle(cornerRadius: 1.5)
                .fill(Theme.Border.hairline.color)
                .frame(width: Self.barWidth, height: 3)
            RoundedRectangle(cornerRadius: 1.5)
                .fill(Theme.Semantic.warning.color)
                .frame(width: max(Self.barWidth * share, share > 0 ? 1 : 0), height: 3)
        }
        .frame(width: Self.barWidth, alignment: .leading)
    }

    /// The row as one sentence, for a reader who cannot see the indentation.
    ///
    /// The depth is spoken because it is the only place the tree's shape lives:
    /// a list of labels read out in order says nothing about what feeds what.
    private var spoken: String {
        var said = row.depth == 0 ? row.label : "\(row.label), level \(row.depth + 1)"
        if let rows = row.node.rows {
            said += ", \(AppModel.pluralized(Int(rows.rounded()), "row"))"
        }
        if let cost = row.node.selfCost {
            said += ", cost \(QueryPlan.cost(cost))"
        }
        if !row.detail.isEmpty {
            said += ". \(row.detail)"
        }
        return said
    }
}

/// The switch between the tree and the rows it was read from.
///
/// Two words rather than an icon: this is not a view mode somebody sets once, it
/// is a question asked in the moment — "is that really what the server said" —
/// and the answer has to be one click away and obviously there.
struct PlanSwitch: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Picker("", selection: $model.showsPlanTree) {
                Text("Plan").tag(true)
                Text("Rows").tag(false)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 120)
            .accessibilityLabel("Show the plan as a tree or as the rows it came in")
            Spacer()
        }
        .padding(.horizontal, Theme.Space.sm)
        .padding(.vertical, Theme.Space.xs)
        .background(Theme.Surface.raised.color)
    }
}
