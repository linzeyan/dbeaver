import SwiftUI

/// The Import sheet: which column of the file goes into which column of the
/// table, and the two things about an import that cannot be taken back.
///
/// A list rather than a palette, which is where this parts company with
/// `TransferSheet`. That one asks for one answer out of hundreds and typing
/// beats reading; this one asks for a decision per column of the file, and every
/// one of them is already answered — what the sheet is for is showing the
/// answers before they are acted on.
///
/// The rows are the file's columns and not the table's. A file column with
/// nowhere to go is a decision somebody has to make; a table column nothing
/// feeds is not — it keeps its default, or takes a NULL, and the server has the
/// last word on whether that is allowed.
struct ImportSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.ImportPlan? { model.importPlan }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            list
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        .frame(width: 540, height: 420)
        .background(Theme.Surface.raised.color)
        // Escape closes it, as it does the transfer picker. Closing is the safe
        // move here — nothing has been read yet — so it needs no confirmation.
        .onExitCommand { model.importPlan = nil }
    }

    /// What is being read, into what, and the two facts the window cannot show
    /// afterwards. They were on the open panel before this sheet existed, and
    /// they belong on the last screen before the rows land rather than on the
    /// first one after the menu.
    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            HStack(spacing: Theme.Space.sm) {
                Text(plan?.url.lastPathComponent ?? "")
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Image(systemName: "arrow.right")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.Text.tertiary.color)
                Text(plan?.table ?? "")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
                Spacer(minLength: 0)
            }
            Text(
                "The table must already exist — no column is added. An import stopped or failed "
                    + "part way leaves behind the rows it had already written."
            )
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.secondary.color)
            .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private var list: some View {
        if let plan {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(plan.fileColumns.indices, id: \.self) { index in
                        row(plan, at: index)
                    }
                }
            }
        } else {
            Color.clear
        }
    }

    private func row(_ plan: AppModel.ImportPlan, at index: Int) -> some View {
        let target = plan.mapping[index]
        return HStack(spacing: Theme.Space.sm) {
            // The dot rather than red text on the name: the name is the file's
            // and there is nothing wrong with it. What is being marked is that
            // this column has nowhere to go, which is a property of the row.
            Circle()
                .fill(target == nil ? Theme.Semantic.warning.color : Color.clear)
                .frame(width: 5, height: 5)
            Text(plan.fileColumns[index])
                .font(Theme.Typography.mono)
                .foregroundStyle(
                    target == nil ? Theme.Text.tertiary.color : Theme.Text.primary.color
                )
                .lineLimit(1)
                .frame(width: 170, alignment: .leading)
            Image(systemName: "arrow.right")
                .font(.system(size: 9))
                .foregroundStyle(Theme.Text.tertiary.color)
            Picker("", selection: binding(at: index)) {
                Text("Skip").tag(String?.none)
                Divider()
                ForEach(plan.tableColumns) { column in
                    Text("\(column.name) · \(column.dataType)").tag(String?.some(column.name))
                }
            }
            .labelsHidden()
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityLabel("Where \(plan.fileColumns[index]) goes")
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs)
    }

    /// Written through the model rather than into the plan, because pointing one
    /// row at a column has to take it off whichever row had it — see
    /// `setImportTarget`.
    private func binding(at index: Int) -> Binding<String?> {
        Binding(
            get: { model.importPlan?.mapping[index] ?? nil },
            set: { model.setImportTarget($0, forFileColumn: index) })
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(summary)
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.importPlanObstacle == nil
                        ? Theme.Text.tertiary.color : Theme.Semantic.warning.color
                )
                .lineLimit(1)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.importPlan = nil }
                .keyboardShortcut(.cancelAction)
            Button("Import") { model.startPlannedImport() }
                .keyboardShortcut(.defaultAction)
                .disabled(model.importPlanObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }

    /// The count, or the reason the button is off. One line rather than two: the
    /// obstacle replaces the count because it is the same fact stated usefully —
    /// "0 of 3 columns" and "every column is skipped" are one sentence.
    private var summary: String {
        if let obstacle = model.importPlanObstacle { return obstacle }
        guard let plan else { return "" }
        let skipped = plan.fileColumns.count - plan.mapped
        let read = "\(AppModel.pluralized(plan.mapped, "column")) read"
        return skipped == 0 ? read : "\(read) · \(skipped) skipped"
    }
}
