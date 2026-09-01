import SwiftUI

/// The sheet behind Drop, Empty and Rename: what is about to happen, and the
/// exact statement that will do it.
///
/// Preview-then-apply, the shape `CreateTableSheet` has, and here the reason is
/// stronger: two of the three cannot be undone. A `DROP TABLE` is four words and
/// the interesting one is the name — whether it says the table somebody meant,
/// and whether the server spelled it as a table or as a materialized view. That
/// is worth reading once, and unreadable a second later.
///
/// One sheet for all three rather than one each. They ask the same question of
/// the same object and differ in a verb, a colour and one field; three files
/// would be three places for the statement pane to drift apart.
struct RelationChangeSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.RelationChangePlan? { model.changePlan }
    private var change: TableChange { plan?.change ?? .drop }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        // Width fixed, height from the content. Unlike the create sheet, whose
        // pane holds one line per column of a file, everything `table_change`
        // writes is a single statement — so a fixed height was three hundred
        // points of black under four words.
        .frame(width: 540)
        .background(Theme.surface.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.changePlan = nil }
    }

    /// What is being changed, and — for a rename — what into.
    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            HStack(spacing: Theme.Space.sm) {
                Text(plan?.qualified ?? "")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(kindLabel)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textSecondary.color)
                Spacer(minLength: 0)
            }
            if change == .rename {
                HStack(spacing: Theme.Space.sm) {
                    Text("To")
                        .font(Theme.Typography.body)
                        .foregroundStyle(Theme.textSecondary.color)
                    TextField("New name", text: nameBinding)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.Typography.mono)
                        .accessibilityLabel("New name for this relation")
                }
            }
            Text(consequence)
                .font(Theme.Typography.caption)
                .foregroundStyle(consequenceTone)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var nameBinding: Binding<String> {
        Binding(
            get: { model.changePlan?.newName ?? "" },
            set: { model.setRelationNewName($0) })
    }

    /// What the row was, in the tree's own words. The kind is on screen because
    /// it decides the statement: PostgreSQL drops a materialized view with a
    /// different one, and neither a view nor a matview can be emptied.
    private var kindLabel: String {
        plan?.relation.kind.label ?? ""
    }

    /// The warning tone for the two that destroy something, and the ordinary one
    /// for the rename. The only colour that separates the three sheets.
    private var consequenceTone: Color {
        change.isDestructive ? Theme.warning.color : Theme.textSecondary.color
    }

    /// The sentence under the name, which is the one thing here that is not a
    /// restatement of the statement below it.
    private var consequence: String {
        switch change {
        case .drop:
            return "This cannot be undone. Anything that depends on it will refuse or break."
        case .truncate:
            return "Every row goes. This cannot be undone, and it is not a delete the "
                + "transaction can always take back."
        case .rename:
            return "Anything that names it — a view, a saved query, an application — will "
                + "not find it afterwards."
        }
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
        // Room for three lines of it. One is what these statements are; the
        // other two are for a schema and a name long enough to wrap, which is
        // the case that would otherwise scroll a four-word statement.
        .frame(height: 92)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.relationChangeObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.relationChangeObstacle == nil
                        ? Theme.textTertiary.color : Theme.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.changePlan = nil }
                .keyboardShortcut(.cancelAction)
            Button(change.actionTitle) { model.applyRelationChange() }
                // No default action on the two that destroy something. Return is
                // the key somebody presses to dismiss a sheet they opened by
                // accident, and on this one it would be the key that runs the
                // drop. The rename keeps it: the field above is where the caret
                // already is, and Return is what finishes typing into a field.
                .keyboardShortcut(change.isDestructive ? nil : KeyboardShortcut.defaultAction)
                .disabled(model.relationChangeObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}
