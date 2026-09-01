import SwiftUI

/// The sheet behind New Database and Drop Database: what is about to happen, and
/// the exact statement that will do it.
///
/// Preview-then-apply, like `RelationChangeSheet`, and its own file rather than
/// a third case in that one: these act on a different object, and a view that
/// took either would have to ask which it was holding at every line. What they
/// share is the shape, which is the part worth copying.
///
/// The drop here is the most destructive button in the application — a database
/// takes every relation in it — so the statement is on screen before anything is
/// sent, and Return does not press it.
struct DatabaseChangeSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.DatabaseChangePlan? { model.databasePlan }
    private var change: DatabaseChange { plan?.change ?? .create }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        .frame(width: 540)
        .background(Theme.surface.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.databasePlan = nil }
    }

    /// The name — typed for a create, stated for a drop — and what it costs.
    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            if change == .create {
                HStack(spacing: Theme.Space.sm) {
                    Text("Name")
                        .font(Theme.Typography.body)
                        .foregroundStyle(Theme.textSecondary.color)
                    TextField("New database", text: nameBinding)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.Typography.mono)
                        .accessibilityLabel("Name for the new database")
                }
            } else {
                Text(plan?.name ?? "")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            Text(consequence)
                .font(Theme.Typography.caption)
                .foregroundStyle(
                    change.isDestructive ? Theme.warning.color : Theme.textSecondary.color
                )
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var nameBinding: Binding<String> {
        Binding(
            get: { model.databasePlan?.name ?? "" },
            set: { model.setNewDatabaseName($0) })
    }

    /// The sentence under the name. The create's is not a warning and says the
    /// one thing that is not obvious: what comes back is empty.
    private var consequence: String {
        switch change {
        case .create:
            return "It arrives empty, with the server's own defaults for everything else. "
                + "This tab stays where it is."
        case .drop:
            return "This cannot be undone. Every table, view and routine in the "
                + "database goes with it."
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
        .frame(height: 92)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.databaseChangeObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.databaseChangeObstacle == nil
                        ? Theme.textTertiary.color : Theme.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.databasePlan = nil }
                .keyboardShortcut(.cancelAction)
            Button(change.actionTitle) { model.applyDatabaseChange() }
                // The drop takes no default action, for the reason the relation
                // sheet's two do not: Return is how somebody dismisses a sheet
                // they opened by accident. The create keeps it — the caret is in
                // the name field, and Return is what finishes typing into one.
                .keyboardShortcut(change.isDestructive ? nil : KeyboardShortcut.defaultAction)
                .disabled(model.databaseChangeObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}
