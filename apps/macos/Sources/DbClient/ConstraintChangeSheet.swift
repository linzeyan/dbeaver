import SwiftUI

/// The sheet behind New Constraint and Drop Constraint: what is about to happen
/// to one constraint of one table, and the exact statement that will do it.
///
/// Preview-then-apply, the shape `IndexChangeSheet` has, and one sheet for both
/// verbs and all three sorts for the same reason: they act on the same object
/// and differ in a verb and a pane of controls.
///
/// The sort picker stays live after the menu preselects it. The two sections
/// this sheet is opened from call the object different things — a row in Foreign
/// keys says "New Foreign Key…" — and the form underneath is still one form, so
/// somebody who opened it from the wrong section changes the picker instead of
/// closing it and finding the other menu. What that costs is a sheet whose title
/// row can end up naming a sort the menu did not; what it buys is that no answer
/// already typed is thrown away, which is also why `NewConstraint` is a struct
/// holding every sort's fields rather than an enum holding one sort's.
struct ConstraintChangeSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.ConstraintChangePlan? { model.constraintPlan }
    private var change: ConstraintChange { plan?.change ?? .drop(name: "", sort: .unique) }

    /// The columns of the relation the sheet was opened over, which is what a
    /// constraint can be built from. Names and not `ColumnInfo`, because what
    /// crosses is a name and what a picker shows is a name.
    private var available: [String] { model.columns.map(\.name) }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        .frame(width: 560)
        .background(Theme.Surface.raised.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.constraintPlan = nil }
    }

    // MARK: - What is being changed

    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            HStack(spacing: Theme.Space.sm) {
                Text(plan?.qualified ?? "")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            switch change {
            case .create(let constraint): createFields(constraint)
            case .drop(let name, _):
                Text(name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
            }
            Text(change.sort.consequence)
                .font(Theme.Typography.caption)
                .foregroundStyle(
                    change.isDestructive ? Theme.Semantic.warning.color : Theme.Text.secondary.color
                )
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func createFields(_ constraint: NewConstraint) -> some View {
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Kind")
            Picker("", selection: binding(\.sort)) {
                ForEach(ConstraintSort.allCases, id: \.self) { sort in
                    Text(sort.label).tag(sort)
                }
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("What kind of constraint this is")
            Spacer(minLength: 0)
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Name")
            TextField("constraint_name", text: binding(\.name))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The name of the new constraint")
        }
        switch constraint.sort {
        case .check:
            HStack(spacing: Theme.Space.sm) {
                fieldLabel("Check")
                // A free-text field, and the one place on this sheet where SQL
                // is typed. A check is an expression in the server's grammar and
                // there is no closed set to offer instead — the statement below
                // is where what was typed is read back.
                TextField("qty > 0", text: binding(\.expression))
                    .textFieldStyle(.roundedBorder)
                    .font(Theme.Typography.mono)
                    .accessibilityLabel("The expression every row has to satisfy")
            }
        case .unique:
            columnRows(constraint)
        case .foreignKey:
            HStack(spacing: Theme.Space.sm) {
                fieldLabel("References")
                TextField("schema", text: binding(\.otherSchema))
                    .textFieldStyle(.roundedBorder)
                    .font(Theme.Typography.mono)
                    .frame(width: 130)
                    .accessibilityLabel("The container the referenced table is in")
                Text(".")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.tertiary.color)
                // Two fields and not one, so that nothing here has to split
                // `sales.customers` on a dot — a name with one in it would come
                // apart, and quoting it back together is the core's job.
                TextField("table", text: binding(\.otherTable))
                    .textFieldStyle(.roundedBorder)
                    .font(Theme.Typography.mono)
                    .accessibilityLabel("The table this key points at")
            }
            columnRows(constraint)
            HStack(spacing: Theme.Space.sm) {
                fieldLabel("On delete")
                rulePicker(binding(\.onDelete), "when the referenced row is deleted")
                fieldLabel("On update")
                rulePicker(binding(\.onUpdate), "when the referenced key changes")
                Spacer(minLength: 0)
            }
        }
    }

    private func rulePicker(_ selection: Binding<ReferentialAction>, _ what: String) -> some View {
        Picker("", selection: selection) {
            ForEach(ReferentialAction.allCases, id: \.self) { action in
                Text(action.label).tag(action)
            }
        }
        .labelsHidden()
        .frame(width: 130)
        .accessibilityLabel("What happens to these rows \(what)")
    }

    /// One row per column, and for a foreign key each row carries both ends.
    ///
    /// Pairs rather than two lists side by side: a key over `(a, b)` referencing
    /// `(x)` is a statement the server refuses, and a row holding both ends
    /// cannot express it. The core refuses a mismatch anyway — the boundary has
    /// other callers — but nothing on this sheet can produce one.
    @ViewBuilder
    private func columnRows(_ constraint: NewConstraint) -> some View {
        ForEach(Array(constraint.columns.enumerated()), id: \.element.id) { position, column in
            HStack(spacing: Theme.Space.sm) {
                fieldLabel(position == 0 ? "Columns" : "")
                Picker("", selection: columnBinding(position)) {
                    // The empty row is what an unanswered one shows, and the
                    // sheet says so below rather than picking a column nobody
                    // chose.
                    Text("Choose a column").tag("")
                    Divider()
                    ForEach(available, id: \.self) { name in
                        Text(name).tag(name)
                    }
                }
                .labelsHidden()
                .frame(width: 180)
                .accessibilityLabel("Column \(position + 1) of the new constraint")
                if constraint.sort == .foreignKey {
                    Text("→")
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.Text.tertiary.color)
                    // Typed and not picked: the other table's columns are a read
                    // this sheet does not make, and holding another relation's
                    // metadata open while a form is being filled in is the thing
                    // this window does not do.
                    TextField("column", text: otherBinding(position))
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.Typography.mono)
                        .frame(width: 150)
                        .accessibilityLabel(
                            "The column \(position + 1) of the new key points at")
                }
                Spacer(minLength: 0)
                Button {
                    edit { $0.columns.remove(at: position) }
                } label: {
                    Image(systemName: "minus.circle")
                        .foregroundStyle(Theme.Text.secondary.color)
                }
                .buttonStyle(.plain)
                .frame(width: 20)
                // The last row cannot go: a constraint over no columns
                // constrains nothing, and the core refuses one.
                .disabled(constraint.columns.count == 1)
                .accessibilityLabel("Remove column \(position + 1)")
                .help(column.name.isEmpty ? "Remove this row" : "Remove \(column.name)")
            }
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("")
            Button("Add Column") { edit { $0.columns.append(ConstraintColumn()) } }
                .font(Theme.Typography.body)
                // A column can be in a constraint once. Offering a row that
                // could only be filled with a repeat would be offering a
                // refusal.
                .disabled(constraint.columns.count >= available.count)
            Spacer(minLength: 0)
        }
    }

    private func fieldLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.body)
            .foregroundStyle(Theme.Text.secondary.color)
            .frame(width: 68, alignment: .leading)
    }

    // MARK: - Bindings

    private func binding<Value>(_ field: WritableKeyPath<NewConstraint, Value>) -> Binding<Value>
    where Value: Equatable {
        Binding(
            get: {
                if case .create(let constraint) = change { return constraint[keyPath: field] }
                return NewConstraint()[keyPath: field]
            },
            set: { value in edit { $0[keyPath: field] = value } })
    }

    private func columnBinding(_ position: Int) -> Binding<String> {
        Binding(
            get: {
                guard case .create(let constraint) = change,
                    constraint.columns.indices.contains(position)
                else { return "" }
                return constraint.columns[position].name
            },
            set: { name in
                edit { constraint in
                    guard constraint.columns.indices.contains(position) else { return }
                    constraint.columns[position].name = name
                }
            })
    }

    private func otherBinding(_ position: Int) -> Binding<String> {
        Binding(
            get: {
                guard case .create(let constraint) = change,
                    constraint.columns.indices.contains(position)
                else { return "" }
                return constraint.columns[position].other
            },
            set: { name in
                edit { constraint in
                    guard constraint.columns.indices.contains(position) else { return }
                    constraint.columns[position].other = name
                }
            })
    }

    /// Applies `change` to the constraint a create is carrying, and nothing
    /// otherwise.
    private func edit(_ change: (inout NewConstraint) -> Void) {
        model.editConstraintChange { pending in
            guard case .create(var constraint) = pending else { return }
            change(&constraint)
            pending = .create(constraint)
        }
    }

    // MARK: - What will run

    private var preview: some View {
        ScrollView {
            Text(plan?.preview ?? "")
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(Theme.Space.md)
        }
        // Room for three lines: a foreign key naming two tables and two rules
        // wraps where a `DROP CONSTRAINT` does not.
        .frame(height: 92)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.constraintChangeObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.constraintChangeObstacle == nil
                        ? Theme.Text.tertiary.color : Theme.Semantic.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.constraintPlan = nil }
                .keyboardShortcut(.cancelAction)
            Button(change.actionTitle) { model.applyConstraintChange() }
                // No default action on the drop, for the reason the other sheets
                // give: Return is the key somebody presses to dismiss a sheet
                // they opened by accident.
                .keyboardShortcut(change.isDestructive ? nil : KeyboardShortcut.defaultAction)
                .disabled(model.constraintChangeObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}
