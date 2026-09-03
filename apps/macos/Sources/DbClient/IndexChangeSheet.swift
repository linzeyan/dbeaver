import SwiftUI

/// The sheet behind New Index and Drop Index: what is about to happen to one
/// index of one table, and the exact statement that will do it.
///
/// Preview-then-apply, the shape `ColumnChangeSheet` has, and one sheet for both
/// verbs for the same reason: they act on the same object and differ in a verb
/// and one pane of controls.
///
/// The create case has a list that grows, which is the Create Table form's shape
/// rather than the column sheet's — an index is over columns in an order, and a
/// row that can be added, removed and repointed is what says the order is
/// somebody's choice.
struct IndexChangeSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.IndexChangePlan? { model.indexPlan }
    private var change: IndexChange { plan?.change ?? .drop(name: "") }

    /// The columns of the relation the sheet was opened over, which is what an
    /// index can be built from. Names and not `ColumnInfo`, because what crosses
    /// is a name and what a picker shows is a name.
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
        .onExitCommand { model.indexPlan = nil }
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
            case .create(let index): createFields(index)
            case .drop(let name):
                Text(name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
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

    @ViewBuilder
    private func createFields(_ index: NewIndex) -> some View {
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("Name")
            TextField("index_name", text: binding(\.name))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The name of the new index")
            Toggle("Unique", isOn: binding(\.unique))
                .font(Theme.Typography.body)
                .accessibilityLabel("The new index refuses a repeated value")
        }
        // Drawn only where the core offers a choice. Every server here has one
        // access method it uses by default, and the ones with only that are the
        // ones whose list is empty — a picker with one row in it would be a
        // control that asks a question with one answer.
        if !model.capabilities.indexMethods.isEmpty {
            HStack(spacing: Theme.Space.sm) {
                fieldLabel("Method")
                Picker("", selection: methodBinding) {
                    Text("Default").tag(String?.none)
                    Divider()
                    ForEach(model.capabilities.indexMethods, id: \.self) { method in
                        Text(method).tag(String?.some(method))
                    }
                }
                .labelsHidden()
                .frame(width: 150)
                .accessibilityLabel("How the new index is stored")
                Spacer(minLength: 0)
            }
        }
        columnRows(index)
    }

    /// One row per key column, in key order.
    ///
    /// Rows rather than a set of checkboxes: an index on `(a, b)` is not an
    /// index on `(b, a)`, and a control with no order in it could not say which
    /// was meant.
    @ViewBuilder
    private func columnRows(_ index: NewIndex) -> some View {
        ForEach(Array(index.columns.enumerated()), id: \.element.id) { position, column in
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
                .frame(width: 220)
                .accessibilityLabel("Key column \(position + 1) of the new index")
                Text(position == 0 ? "sorted first" : "then")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.tertiary.color)
                Spacer(minLength: 0)
                Button {
                    edit { $0.columns.remove(at: position) }
                } label: {
                    Image(systemName: "minus.circle")
                        .foregroundStyle(Theme.Text.secondary.color)
                }
                .buttonStyle(.plain)
                .frame(width: 20)
                // The last row cannot go: an index over no columns indexes
                // nothing, and the core refuses one.
                .disabled(index.columns.count == 1)
                .accessibilityLabel("Remove key column \(position + 1)")
                .help(column.name.isEmpty ? "Remove this row" : "Remove \(column.name)")
            }
        }
        HStack(spacing: Theme.Space.sm) {
            fieldLabel("")
            Button("Add Column") { edit { $0.columns.append(IndexColumn()) } }
                .font(Theme.Typography.body)
                // A column can be in an index once. Offering a row that could
                // only be filled with a repeat would be offering a refusal.
                .disabled(index.columns.count >= available.count)
            Spacer(minLength: 0)
        }
    }

    private func fieldLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.body)
            .foregroundStyle(Theme.Text.secondary.color)
            .frame(width: 62, alignment: .leading)
    }

    /// The sentence under the fields, which is the one thing here that is not a
    /// restatement of the statement below it.
    private var consequence: String {
        switch change {
        case .create:
            return "The server reads the whole table to build it, and holds the table against "
                + "writes while it does. A unique index is refused if the values are not."
        case .drop:
            return "The rows stay; only the index goes. Anything relying on it for speed gets "
                + "slower, and building it again means reading the whole table."
        }
    }

    // MARK: - Bindings

    private func binding<Value>(_ field: WritableKeyPath<NewIndex, Value>) -> Binding<Value>
    where Value: Equatable {
        Binding(
            get: {
                if case .create(let index) = change { return index[keyPath: field] }
                return NewIndex()[keyPath: field]
            },
            set: { value in edit { $0[keyPath: field] = value } })
    }

    private var methodBinding: Binding<String?> {
        Binding(
            get: {
                guard case .create(let index) = change else { return nil }
                return index.method
            },
            set: { chosen in edit { $0.method = chosen } })
    }

    private func columnBinding(_ position: Int) -> Binding<String> {
        Binding(
            get: {
                guard case .create(let index) = change, index.columns.indices.contains(position)
                else { return "" }
                return index.columns[position].name
            },
            set: { name in
                edit { index in
                    guard index.columns.indices.contains(position) else { return }
                    index.columns[position].name = name
                }
            })
    }

    /// Applies `change` to the index a create is carrying, and nothing otherwise.
    private func edit(_ change: (inout NewIndex) -> Void) {
        model.editIndexChange { pending in
            guard case .create(var index) = pending else { return }
            change(&index)
            pending = .create(index)
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
        // Room for three lines: a `CREATE INDEX` over several columns wraps
        // where a `DROP INDEX` does not.
        .frame(height: 92)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.indexChangeObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.indexChangeObstacle == nil
                        ? Theme.Text.tertiary.color : Theme.Semantic.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.indexPlan = nil }
                .keyboardShortcut(.cancelAction)
            Button(change.actionTitle) { model.applyIndexChange() }
                // No default action on the drop, for the reason the other sheets
                // give: Return is the key somebody presses to dismiss a sheet
                // they opened by accident.
                .keyboardShortcut(change.isDestructive ? nil : KeyboardShortcut.defaultAction)
                .disabled(model.indexChangeObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}
