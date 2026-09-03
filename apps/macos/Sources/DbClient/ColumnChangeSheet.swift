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
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        // Width fixed, height from the content: the add case is three controls
        // taller than the other two, and a fixed height would be black space
        // under a one-line `DROP COLUMN`.
        .frame(width: 560)
        .background(Theme.Surface.raised.color)
        // Escape closes it. Nothing has run yet, so closing needs no question.
        .onExitCommand { model.columnPlan = nil }
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
            case .add(let column): addFields(column)
            case .alter(let alteration):
                Text(standing(alteration))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
                    .truncationMode(.middle)
                alterFields(alteration)
            case .drop(let name):
                Text(name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
            case .rename(let name, _):
                HStack(spacing: Theme.Space.sm) {
                    Text(name)
                        .font(Theme.Typography.mono)
                        .foregroundStyle(Theme.Text.primary.color)
                    Text("to")
                        .font(Theme.Typography.body)
                        .foregroundStyle(Theme.Text.secondary.color)
                    TextField("New name", text: newNameBinding)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.Typography.mono)
                        .accessibilityLabel("New name for this column")
                }
            }
            Text(consequence)
                .font(Theme.Typography.caption)
                .foregroundStyle(
                    change.isDestructive ? Theme.Semantic.warning.color : Theme.Text.secondary.color
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
            fieldLabel("Name")
            TextField("column_name", text: addBinding(\.name))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The name of the new column")
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Type")
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
            fieldLabel("Default")
            TextField("none", text: addBinding(\.defaultValue))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The default for the new column")
        }
    }

    /// The column as the server last described it, which is the sentence the
    /// three pickers below are read against.
    ///
    /// Said once, here, rather than repeated inside each control's "leave it"
    /// row: a picker reading "Leave as character varying(64)" is a picker whose
    /// own label has to be truncated.
    private func standing(_ alteration: ColumnAlteration) -> String {
        var parts = [alteration.name, alteration.currentType]
        parts.append(alteration.currentNullable ? "null" : "not null")
        if let value = alteration.currentDefault {
            parts.append("default \(value)")
        }
        return parts.joined(separator: " · ")
    }

    /// Three properties, each with "Unchanged" as its first answer.
    ///
    /// Pickers rather than checkboxes beside controls, so that leaving a
    /// property alone is a state somebody chose and can read back. The type is
    /// the reason this matters: a column the server calls
    /// `character varying(64)` is none of the seven kinds offered, and a form
    /// that started the picker on a guess would retype it whenever somebody came
    /// here to change the default.
    @ViewBuilder
    private func alterFields(_ alteration: ColumnAlteration) -> some View {
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Type")
            Picker("", selection: alterKindBinding(alteration)) {
                Text("Unchanged").tag(ColumnKind?.none)
                Divider()
                ForEach(ColumnKind.offered, id: \.self) { kind in
                    Text(kind.label).tag(ColumnKind?.some(kind))
                }
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("What this column will hold")
            if let size = alteration.kind?.decimalSize {
                sizeField(value: size.precision, label: "Digits before and after the point") {
                    .decimal(precision: $0, scale: size.scale)
                }
                sizeField(value: size.scale, label: "Digits after the point") {
                    .decimal(precision: size.precision, scale: $0)
                }
            }
            Spacer(minLength: 0)
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Null")
            Picker("", selection: alterNullableBinding) {
                Text("Unchanged").tag(Bool?.none)
                Divider()
                Text("Can be null").tag(Bool?.some(true))
                Text("Cannot be null").tag(Bool?.some(false))
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("Whether this column will take a null")
            Spacer(minLength: 0)
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Default")
            Picker("", selection: alterDefaultBinding) {
                Text("Unchanged").tag(DefaultKind.keep)
                Divider()
                Text("Set to…").tag(DefaultKind.set)
                Text("Remove it").tag(DefaultKind.drop)
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("What happens to this column's default")
            if case .set(let value) = alteration.defaultChange {
                TextField(
                    "",
                    text: Binding(
                        get: { value },
                        set: { typed in editAlteration { $0.defaultChange = .set(typed) } })
                )
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The column's new default")
            }
            Spacer(minLength: 0)
        }
    }

    private func fieldLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.body)
            .foregroundStyle(Theme.Text.secondary.color)
            .frame(width: 56, alignment: .leading)
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
        case .alter:
            return "The server reads every row to check it. A value that will not fit the new "
                + "type, or a null in a column that stops taking one, refuses the whole "
                + "statement."
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

    /// The same for an alteration.
    private func editAlteration(_ change: (inout ColumnAlteration) -> Void) {
        model.editColumnChange { pending in
            guard case .alter(var alteration) = pending else { return }
            change(&alteration)
            pending = .alter(alteration)
        }
    }

    /// Which of the default's three answers a picker row stands for.
    ///
    /// A separate type from `DefaultChange` because the value the third one
    /// carries must survive being switched away from and back — a picker tag
    /// holding the typed text would make every keystroke a different row.
    private enum DefaultKind: Hashable {
        case keep
        case set
        case drop
    }

    /// The type, which starts at "Unchanged" and stays there until somebody
    /// moves it. Decimal sizes behave as the Create Table form's picker does:
    /// one menu row stands for many values, so a size already chosen survives
    /// being reselected.
    private func alterKindBinding(_ alteration: ColumnAlteration) -> Binding<ColumnKind?> {
        Binding(
            get: {
                guard let kind = alteration.kind else { return nil }
                return ColumnKind.offered.first { $0.isSameKind(as: kind) } ?? kind
            },
            set: { chosen in
                editAlteration { alteration in
                    guard let chosen else {
                        alteration.kind = nil
                        return
                    }
                    if let current = alteration.kind, chosen.isSameKind(as: current) { return }
                    alteration.kind = chosen
                }
            })
    }

    private var alterNullableBinding: Binding<Bool?> {
        Binding(
            get: {
                guard case .alter(let alteration) = change else { return nil }
                return alteration.nullable
            },
            set: { chosen in editAlteration { $0.nullable = chosen } })
    }

    private var alterDefaultBinding: Binding<DefaultKind> {
        Binding(
            get: {
                guard case .alter(let alteration) = change else { return .keep }
                switch alteration.defaultChange {
                case .keep: return .keep
                case .drop: return .drop
                case .set: return .set
                }
            },
            set: { chosen in
                editAlteration { alteration in
                    switch chosen {
                    case .keep: alteration.defaultChange = .keep
                    case .drop: alteration.defaultChange = .drop
                    // Opened on the default the column already has, which is
                    // what somebody editing one rather than replacing it starts
                    // from. Empty where there is none, and the button waits for
                    // it — `SET DEFAULT` with nothing after it is a syntax error.
                    case .set:
                        if case .set = alteration.defaultChange { return }
                        alteration.defaultChange = .set(alteration.currentDefault ?? "")
                    }
                }
            })
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
                        ? Theme.Text.tertiary.color : Theme.Semantic.warning.color
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
