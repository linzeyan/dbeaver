import SwiftUI

/// The sheet behind Add Column, Drop Column and Rename Column: what is about to
/// happen to one column, and the exact statement that will do it.
///
/// Preview-then-apply, the shape `RelationChangeSheet` has, and one sheet for
/// all three for the same reason: they ask the same question of the same object
/// and differ in a verb and one control. Three files would be three places for
/// the statement pane to drift apart.
///
/// The add case has more to fill in than the other two — a name, a kind, a null
/// and a default — and it is laid out down the sheet rather than across a row,
/// which is what the Create Table form does for many columns at once. One column
/// has room for labels, and the labels are what say which checkbox is which.
struct ColumnChangeSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.ColumnChangePlan? { model.columnPlan }
    private var change: ColumnChange { plan?.change ?? .drop(name: "") }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        // Width fixed, height from the content: the add case is three controls
        // taller than the other two, and a fixed height would be black space
        // under a one-line `DROP COLUMN`.
        .frame(width: 560)
        .background(Theme.surface.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.columnPlan = nil }
    }

    // MARK: - What is being changed

    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            HStack(spacing: Theme.Space.sm) {
                Text(plan?.qualified ?? "")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            switch change {
            case .add(let column): addFields(column)
            case .drop(let name):
                Text(name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
            case .rename(let name, _):
                HStack(spacing: Theme.Space.sm) {
                    Text(name)
                        .font(Theme.Typography.mono)
                        .foregroundStyle(Theme.text.color)
                    Text("to")
                        .font(Theme.Typography.body)
                        .foregroundStyle(Theme.textSecondary.color)
                    TextField("New name", text: newNameBinding)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.Typography.mono)
                        .accessibilityLabel("New name for this column")
                }
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

    /// The same five answers the Create Table form takes, minus the key.
    ///
    /// No Key checkbox: a primary key is a rule about the whole table, and a
    /// table with rows in it has no room for another. The core refuses one if it
    /// arrives, and offering a checkbox that always refuses would be a control
    /// that lies about what it does.
    @ViewBuilder
    private func addFields(_ column: NewTableColumn) -> some View {
        HStack(spacing: Theme.Space.sm) {
            Text("Name")
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
                .frame(width: 56, alignment: .leading)
            TextField("column_name", text: addBinding(\.name))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The name of the new column")
        }
        HStack(spacing: Theme.Space.sm) {
            Text("Type")
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
                .frame(width: 56, alignment: .leading)
            Picker("", selection: kindBinding) {
                ForEach(ColumnKind.offered, id: \.self) { kind in
                    Text(kind.label).tag(kind)
                }
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("What the new column holds")
            if let size = column.kind.decimalSize {
                sizeField(value: size.precision, label: "Digits before and after the point") {
                    .decimal(precision: $0, scale: size.scale)
                }
                sizeField(value: size.scale, label: "Digits after the point") {
                    .decimal(precision: size.precision, scale: $0)
                }
            }
            Toggle("Can be null", isOn: addBinding(\.nullable))
                .font(Theme.Typography.body)
                .accessibilityLabel("The new column can hold a null")
            Spacer(minLength: 0)
        }
        HStack(spacing: Theme.Space.sm) {
            Text("Default")
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
                .frame(width: 56, alignment: .leading)
            TextField("none", text: addBinding(\.defaultValue))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The default for the new column")
        }
    }

    /// One half of a decimal's size, as the Create Table form's row takes it.
    private func sizeField(
        value: Int, label: String, set: @escaping (Int) -> ColumnKind
    ) -> some View {
        TextField(
            "",
            text: Binding(
                get: { String(value) },
                // A field being emptied to be retyped reads as 0 for the
                // keystroke in between, and the core refuses a size it cannot
                // parse rather than this inventing one.
                set: { typed in edit { $0.kind = set(Int(typed) ?? 0) } })
        )
        .textFieldStyle(.roundedBorder)
        .font(Theme.Typography.monoSmall)
        .frame(width: 34)
        .accessibilityLabel(label)
    }

    /// The sentence under the name, which is the one thing here that is not a
    /// restatement of the statement below it.
    private var consequence: String {
        switch change {
        case .add:
            return "The rows already in the table get this column's default, or a null where "
                + "there is none."
        case .drop:
            return "This cannot be undone. The values go with the column, and anything that "
                + "names it — an index, a view, an application — will refuse or break."
        case .rename:
            return "The values stay. Anything that names the column — a view, a saved query, "
                + "an application — will not find it afterwards."
        }
    }

    // MARK: - Bindings

    private var newNameBinding: Binding<String> {
        Binding(
            get: {
                if case .rename(_, let to) = change { return to }
                return ""
            },
            set: { typed in
                model.editColumnChange { change in
                    if case .rename(let name, _) = change {
                        change = .rename(name: name, to: typed)
                    }
                }
            })
    }

    private func addBinding<Value>(_ field: WritableKeyPath<NewTableColumn, Value>) -> Binding<
        Value
    >
    where Value: Equatable {
        Binding(
            get: {
                if case .add(let column) = change { return column[keyPath: field] }
                return NewTableColumn()[keyPath: field]
            },
            set: { value in edit { $0[keyPath: field] = value } })
    }

    /// The kind, keeping whatever size a decimal already had — the rule the
    /// Create Table form's picker has, and for the same reason: one menu row
    /// stands for many values.
    private var kindBinding: Binding<ColumnKind> {
        Binding(
            get: {
                guard case .add(let column) = change else { return .text }
                return ColumnKind.offered.first { $0.isSameKind(as: column.kind) } ?? column.kind
            },
            set: { chosen in
                edit { column in
                    column.kind = chosen.isSameKind(as: column.kind) ? column.kind : chosen
                }
            })
    }

    /// Applies `change` to the column an add is carrying, and nothing otherwise.
    private func edit(_ change: (inout NewTableColumn) -> Void) {
        model.editColumnChange { pending in
            guard case .add(var column) = pending else { return }
            change(&column)
            pending = .add(column)
        }
    }

    // MARK: - What will run

    private var preview: some View {
        ScrollView {
            Text(plan?.preview ?? "")
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.text.color)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(Theme.Space.md)
        }
        // Room for three lines: one is what these statements are, and the other
        // two are for a schema and a name long enough to wrap.
        .frame(height: 92)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.columnChangeObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.columnChangeObstacle == nil
                        ? Theme.textTertiary.color : Theme.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.columnPlan = nil }
                .keyboardShortcut(.cancelAction)
            Button(change.actionTitle) { model.applyColumnChange() }
                // No default action on the one that destroys something, for the
                // reason `RelationChangeSheet` gives: Return is the key somebody
                // presses to dismiss a sheet they opened by accident. The other
                // two keep it, the caret being in a field the whole time.
                .keyboardShortcut(change.isDestructive ? nil : KeyboardShortcut.defaultAction)
                .disabled(model.columnChangeObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}
