import SwiftUI

/// The form behind New Table: where it goes, what is in it, and the statement
/// that will make it.
///
/// Preview-then-apply, the shape `CreateTableSheet` and `RelationChangeSheet`
/// both have — and the one place in this application where a statement is
/// composed out of answers rather than read off something that already exists.
/// That is what makes the pane at the bottom worth its space: the type words are
/// this database's and not the ones anybody typed, so seeing `bigint` appear
/// under "Whole number" is how the picker explains itself.
///
/// Deliberately a small form. Five things per column — a name, a kind, whether
/// it takes a null, whether it is part of the key, and a default — which is what
/// a table needs to exist and be filled. A check constraint, a foreign key, a
/// collation, an index: all of those are the SQL editor's, a tab away, with the
/// whole language available. A form that grew to cover them would be a worse
/// editor than the editor.
struct NewTableSheet: View {
    @Bindable var model: AppModel

    private var plan: AppModel.NewTablePlan? { model.newTablePlan }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            columns
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            preview
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        .frame(width: 680)
        .background(Theme.surface.color)
        // Escape closes it. Nothing has run, so closing needs no question — and
        // unlike the change sheets there is nothing here that cannot be typed
        // again.
        .onExitCommand { model.newTablePlan = nil }
    }

    // MARK: - Where it goes

    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.containerNoun.capitalized)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
            Picker("", selection: schemaBinding) {
                ForEach(model.schemas, id: \.name) { schema in
                    Text(schema.name).tag(schema.name)
                }
            }
            .labelsHidden()
            .frame(width: 160)
            .accessibilityLabel("The \(model.containerNoun) the table is made in")

            Text("Name")
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
            TextField("table_name", text: nameBinding)
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .accessibilityLabel("The name of the new table")
        }
        .padding(Theme.Space.md)
    }

    private var schemaBinding: Binding<String> {
        Binding(
            get: { plan?.schema ?? "" },
            set: { schema in model.editNewTable { $0.schema = schema } })
    }

    private var nameBinding: Binding<String> {
        Binding(
            get: { plan?.name ?? "" },
            set: { name in model.editNewTable { $0.name = name } })
    }

    // MARK: - What is in it

    private var columns: some View {
        VStack(spacing: 0) {
            columnHeadings
            ScrollView {
                VStack(spacing: Theme.Space.xs) {
                    ForEach(Array((plan?.columns ?? []).enumerated()), id: \.element.id) {
                        index, column in
                        row(at: index, column: column)
                    }
                }
                .padding(.horizontal, Theme.Space.md)
                .padding(.bottom, Theme.Space.sm)
            }
            // Four rows before it scrolls, which is most of the tables a form
            // like this makes; past that the pane below stays where it is rather
            // than the sheet growing off the screen.
            .frame(height: 132)
            addButton
        }
    }

    private var columnHeadings: some View {
        HStack(spacing: Theme.Space.sm) {
            Text("Column").frame(width: 140, alignment: .leading)
            Text("Type").frame(width: 234, alignment: .leading)
            // The two checkboxes are labelled here and not beside themselves: a
            // toggle with its own label repeated down five rows is five times the
            // words for one fact.
            Text("Null").frame(width: 36, alignment: .center)
            Text("Key").frame(width: 36, alignment: .center)
            Text("Default").frame(maxWidth: .infinity, alignment: .leading)
            // Matches the width of the remove button below, so the headings sit
            // over the fields they name rather than one control to the left.
            Color.clear.frame(width: 20)
        }
        .font(Theme.Typography.caption)
        .foregroundStyle(Theme.textTertiary.color)
        .padding(.horizontal, Theme.Space.md)
        .padding(.bottom, Theme.Space.xs)
    }

    private func row(at index: Int, column: NewTableColumn) -> some View {
        HStack(spacing: Theme.Space.sm) {
            TextField("name", text: binding(index, \.name))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .frame(width: 140)
                .accessibilityLabel("The name of column \(index + 1)")

            HStack(spacing: Theme.Space.xs) {
                Picker("", selection: kindBinding(index)) {
                    ForEach(ColumnKind.offered, id: \.self) { kind in
                        Text(kind.label).tag(kind)
                    }
                }
                .labelsHidden()
                .frame(width: 150)
                .accessibilityLabel("What column \(index + 1) holds")
                // The size, for the one kind that carries one. The picker's width
                // is fixed rather than fitted, so a row that grows these two does
                // not shuffle every control beside it sideways.
                if let size = column.kind.decimalSize {
                    sizeField(index, value: size.precision) { precision, plan in
                        plan.columns[index].kind = .decimal(
                            precision: precision, scale: size.scale)
                    }
                    sizeField(index, value: size.scale) { scale, plan in
                        plan.columns[index].kind = .decimal(
                            precision: size.precision, scale: scale)
                    }
                }
            }
            .frame(width: 234, alignment: .leading)

            // A key column cannot hold a null on any of these servers, and two of
            // them fix it silently — PostgreSQL and MySQL make a `PRIMARY KEY`
            // column `NOT NULL` whatever the statement said. So the checkbox is
            // shut rather than overruled, which is the difference between a form
            // that was ignored and one that explains itself.
            Toggle("", isOn: binding(index, \.nullable))
                .labelsHidden()
                .frame(width: 36)
                .disabled(column.isPrimaryKey)
                .help(column.isPrimaryKey ? "A key column cannot hold a null." : "")
                .accessibilityLabel("Column \(index + 1) can hold a null")

            // The Null checkbox beside it is answered by the model, not here:
            // the statement has to say what the server will do, and every path
            // into the plan has to go through the same rule.
            Toggle("", isOn: binding(index, \.isPrimaryKey))
                .labelsHidden()
                .frame(width: 36)
                .accessibilityLabel("Column \(index + 1) is part of the primary key")

            TextField("none", text: binding(index, \.defaultValue))
                .textFieldStyle(.roundedBorder)
                .font(Theme.Typography.mono)
                .frame(maxWidth: .infinity)
                .accessibilityLabel("The default for column \(index + 1)")

            // Absent on the last remaining row rather than disabled: a table with
            // no columns is not a table, and the space is kept so the row does
            // not resize as columns come and go.
            if (plan?.columns.count ?? 0) > 1 {
                Button {
                    model.editNewTable { $0.columns.remove(at: index) }
                } label: {
                    Image(systemName: "minus.circle")
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.textTertiary.color)
                .frame(width: 20)
                .accessibilityLabel("Remove column \(index + 1)")
            } else {
                Color.clear.frame(width: 20, height: 1)
            }
        }
    }

    /// One half of a decimal's size.
    private func sizeField(
        _ index: Int, value: Int, set: @escaping (Int, inout AppModel.NewTablePlan) -> Void
    ) -> some View {
        TextField(
            "",
            text: Binding(
                get: { String(value) },
                set: { typed in
                    // A field that is being emptied to be retyped reads as 0 for
                    // the keystroke in between, and the core refuses a scale it
                    // cannot parse rather than this inventing one.
                    model.editNewTable { plan in set(Int(typed) ?? 0, &plan) }
                })
        )
        .textFieldStyle(.roundedBorder)
        .font(Theme.Typography.monoSmall)
        .frame(width: 34)
        .accessibilityLabel("Digits for column \(index + 1)")
    }

    private var addButton: some View {
        HStack {
            Button {
                model.editNewTable { $0.columns.append(NewTableColumn()) }
            } label: {
                Label("Add Column", systemImage: "plus")
                    .font(Theme.Typography.caption)
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.accent.color)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.bottom, Theme.Space.sm)
    }

    private func binding<Value>(
        _ index: Int, _ field: WritableKeyPath<NewTableColumn, Value>
    ) -> Binding<Value> where Value: Equatable {
        Binding(
            get: {
                plan?.columns[safe: index]?[keyPath: field] ?? NewTableColumn()[keyPath: field]
            },
            set: { value in
                model.editNewTable { plan in
                    guard plan.columns.indices.contains(index) else { return }
                    plan.columns[index][keyPath: field] = value
                }
            })
    }

    /// The kind, keeping whatever size a decimal already had.
    ///
    /// The picker offers one decimal row and the steppers beside it make many
    /// values, so selecting has to compare the kinds and not the values —
    /// otherwise moving a stepper leaves the menu showing nothing selected.
    private func kindBinding(_ index: Int) -> Binding<ColumnKind> {
        Binding(
            get: {
                let kind = plan?.columns[safe: index]?.kind ?? .text
                return ColumnKind.offered.first { $0.isSameKind(as: kind) } ?? kind
            },
            set: { chosen in
                model.editNewTable { plan in
                    guard plan.columns.indices.contains(index) else { return }
                    let current = plan.columns[index].kind
                    plan.columns[index].kind =
                        chosen.isSameKind(as: current) ? current : chosen
                }
            })
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
        // One line per column plus the brackets, for the four rows above.
        .frame(height: 128)
        .accessibilityLabel("The statement that will be run")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.newTableObstacle ?? "Nothing has run yet.")
                .font(Theme.Typography.micro)
                .foregroundStyle(
                    model.newTableObstacle == nil ? Theme.textTertiary.color : Theme.warning.color
                )
                .lineLimit(2)
            Spacer(minLength: Theme.Space.sm)
            Button("Cancel") { model.newTablePlan = nil }
                .keyboardShortcut(.cancelAction)
            // Return runs it, unlike the two destructive sheets. Nothing is lost
            // by a table made in the wrong place, and the caret is in one of
            // these fields the whole time — Return is what finishes typing.
            Button("Create") { model.applyNewTable() }
                .keyboardShortcut(.defaultAction)
                .disabled(model.newTableObstacle != nil)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}

extension Array {
    /// The element at `index`, or nil where a row has just been removed.
    ///
    /// SwiftUI hands a binding an index it captured before the array changed,
    /// which for a list with a remove button is one frame of every removal.
    fileprivate subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
