import SwiftUI

/// What the server is configured with.
///
/// The same table as `ProcessesSheet` with everything dangerous taken out, and
/// the differences between them are all consequences of one fact: these rows do
/// not change while you read them. So there is no timer and no auto-refresh
/// picker, no selection — nothing acts on one row — and the sheet is opened to
/// answer a question rather than to watch something go wrong.
///
/// What it does have is the filter, which is not a convenience here. A default
/// PostgreSQL reports about 360 settings and a default MySQL about 650; the only
/// way anybody finds `innodb_flush_log_at_trx_commit` is by typing part of it.
/// The field takes focus when the sheet opens for that reason.
struct VariablesSheet: View {
    @Bindable var model: AppModel
    @FocusState private var typing: Bool

    private var rows: [ServerVariable] { model.visibleVariables }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            table
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        .frame(width: 720, height: 420)
        .background(Theme.Surface.raised.color)
        .onExitCommand { model.closeVariables() }
        .onAppear { typing = true }
    }

    /// The filter and Refresh.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "magnifyingglass")
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
            TextField("Filter", text: $model.variableFilter)
                .textFieldStyle(.plain)
                .font(Theme.Typography.body)
                .focused($typing)
                .frame(width: 240)
                .accessibilityLabel("Filter settings")

            Spacer(minLength: Theme.Space.sm)

            Button("Refresh") { model.loadVariables() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Accent.selection.color)
                .disabled(model.isReadingVariables)
        }
        .padding(Theme.Space.md)
    }

    @ViewBuilder
    private var table: some View {
        if rows.isEmpty {
            // Which of the two empties this is, as in `ProcessesSheet` — except
            // that here the first case means the read has not landed yet, since
            // a server with no settings at all is not a thing.
            Text(
                model.variableFilter.isEmpty
                    ? "Nothing read from this server yet."
                    : "No setting matches \(model.variableFilter)."
            )
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.tertiary.color)
            .padding(Theme.Space.md)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        } else {
            VStack(spacing: 0) {
                columnHeadings
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(rows) { variable in
                            row(variable)
                        }
                    }
                }
            }
        }
    }

    private var columnHeadings: some View {
        HStack(spacing: Theme.Space.sm) {
            heading("Name", width: 260)
            heading("Scope", width: 70)
            Text("Value")
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 20)
        .accessibilityHidden(true)
    }

    private func heading(_ text: String, width: CGFloat) -> some View {
        Text(text)
            .font(Theme.Typography.micro)
            .foregroundStyle(Theme.Text.tertiary.color)
            .frame(width: width, alignment: .leading)
    }

    /// One setting. Not a button, because there is nothing to select it for.
    private func row(_ variable: ServerVariable) -> some View {
        HStack(spacing: Theme.Space.sm) {
            Text(variable.name)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(width: 260, alignment: .leading)
            // Dimmer for the server's, brighter for this connection's, which is
            // the opposite of how a table usually greys a column: the server's
            // is the ordinary case on nearly every row, and the point of the
            // column is spotting the handful that are not.
            Text(variable.scope.label)
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    variable.scope == .session
                        ? Theme.Accent.selection.color : Theme.Text.tertiary.color
                )
                .frame(width: 70, alignment: .leading)
            // Two lines, not one. A value here is a path, a list of flags or a
            // log prefix as often as it is a number, and truncating
            // `shared_preload_libraries` to the width of a column would hide
            // exactly what somebody opened this to read.
            Text(variable.value)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .lineLimit(2)
                .truncationMode(.tail)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(variable.name), \(variable.scope.label), \(variable.value)")
    }

    /// The count, whatever the last copy or failure said, and the two buttons.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(AppModel.pluralized(rows.count, "setting", "settings"))
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
            if !model.variableReport.isEmpty {
                Text(model.variableReport)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.Space.sm)
            // Greyed rather than merely inert when there is nothing to copy, for
            // the reason `ProcessesSheet` greys its kill buttons: an explicit
            // colour survives `disabled`, and a button that looks pressable and
            // does nothing reads as a broken button.
            Button("Copy") { model.copyVisibleVariables() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(
                    rows.isEmpty ? Theme.Text.tertiary.color : Theme.Accent.selection.color
                )
                .disabled(rows.isEmpty)
            Button("Done") { model.closeVariables() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
    }
}
