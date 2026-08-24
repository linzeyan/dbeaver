import SwiftUI

/// The Create Table sheet: where the table goes, what it is called, and the
/// exact statement that will run.
///
/// Preview-then-apply. The types under it were inferred from the head of the
/// file and are a guess — a column of `2026-08-24` becomes a timestamp and a
/// column of `08/24/2026` stays text, because a client that decided which half
/// was the month would decide wrong for half the world. Which of the two
/// happened is worth reading before a table exists, and unreadable afterwards.
///
/// The statement is shown and not edited. There is one thing on this sheet to
/// change and it is the name; a statement somebody had retyped would no longer
/// be the statement this window can say anything about, and a different set of
/// types is a `CREATE TABLE` written in the Query pane, where every other
/// hand-written statement lives.
struct CreateTableSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.CreateTablePlan? { model.createPlan }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        .frame(width: 540, height: 420)
        .background(Theme.surface.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.createPlan = nil }
    }

    /// The file, and the two halves of where it is going.
    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            HStack(spacing: Theme.Space.sm) {
                Text(plan?.url.lastPathComponent ?? "")
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            HStack(spacing: Theme.Space.sm) {
                // A picker rather than a field: the schemas are known, and a
                // typed one that does not exist is a refusal from the server
                // after the button rather than a list before it.
                Picker("Schema", selection: schemaBinding) {
                    ForEach(model.schemas) { schema in
                        Text(schema.name).tag(schema.name)
                    }
                }
                .frame(maxWidth: 200)
                .accessibilityLabel("Schema for the new table")
                // Spelled out beside the field, because the placeholder that
                // would have said it is never seen: the field arrives with a
                // name in it already.
                Text("Table")
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.textSecondary.color)
                TextField("Table", text: nameBinding)
                    .textFieldStyle(.roundedBorder)
                    .font(Theme.Typography.mono)
                    .accessibilityLabel("Name for the new table")
            }
            Text(
                "The types are read from the first rows of the file. Nothing is run until "
                    + "Create is pressed; the file is offered to the table straight afterwards."
            )
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.textSecondary.color)
            .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var schemaBinding: Binding<String> {
        Binding(
            get: { model.createPlan?.schema ?? "" },
            set: { model.setCreateTableTarget(schema: $0) })
    }

    private var nameBinding: Binding<String> {
        Binding(
            get: { model.createPlan?.name ?? "" },
            set: { model.setCreateTableTarget(name: $0) })
    }

    /// The statement, as the connection writes it.
    @ViewBuilder
    private var preview: some View {
        ScrollView {
            Text(plan?.preview ?? "")
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.text.color)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(Theme.Space.md)
        }
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.createPlanObstacle ?? summary)
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.createPlanObstacle == nil
                        ? Theme.textTertiary.color : Theme.warning.color
                )
                .lineLimit(1)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.createPlan = nil }
                .keyboardShortcut(.cancelAction)
            Button("Create") { model.startPlannedCreate() }
                .keyboardShortcut(.defaultAction)
                .disabled(model.createPlanObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }

    /// What the statement amounts to, once there is one: the count is the fact
    /// the wall of text is hard to read off.
    private var summary: String {
        guard let statement = plan?.statement else { return "" }
        let columns = statement.components(separatedBy: ",\n").count
        return "\(AppModel.pluralized(columns, "column")) · nothing has run yet"
    }
}
