import SwiftUI

/// Window shell: navigator on the left, tabbed detail on the right, status
/// along the bottom. The layout follows Sequel Ace — pick an object, then
/// switch between describing it, browsing it, and querying it.

/// Where keyboard focus can be. Named so panes can hand focus to each other and
/// so the filter fields can draw their own focus ring; SwiftUI's default ring
/// does not survive the custom field backgrounds this window uses.
enum FocusArea: Hashable {
    case navigatorFilter
    case structureTable
    case whereField
    case orderField
    case editor
    // The connection form's fields. In the same enum as the panes' because the
    // form is the same window: it replaces the shell rather than floating over
    // it, so focus is only ever in one of these places at a time.
    case connectHost
    case connectPort
    case connectDatabase
    case connectUser
    case connectPassword
}

struct MainView: View {
    @Bindable var model: AppModel
    @FocusState private var focus: FocusArea?

    var body: some View {
        NavigationSplitView {
            NavigatorView(model: model, focus: $focus)
                .navigationSplitViewColumnWidth(min: 200, ideal: 250, max: 420)
        } detail: {
            DetailPane(model: model, focus: $focus)
        }
        .toolbar { toolbarContent }
        // The View menu's Filter Objects item, arriving as a bumped counter.
        // An `NSMenuItem` action runs outside the view tree and cannot assign
        // to a `@FocusState`; this is the only end of that wire that can.
        .onChange(of: model.filterFocusRequests) { focus = .navigatorFilter }
        .navigationTitle(model.selected?.name ?? "DbClient")
        .navigationSubtitle(
            model.selected.map { "\($0.kind.label) · \($0.schema)" } ?? model.connectionLabel)
    }

    @ToolbarContentBuilder
    private var toolbarContent: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            HStack(spacing: Theme.Space.xs + 2) {
                StatusDot(state: model.connectionState)
                Text(model.connectionLabel)
                    .font(Theme.Typography.bodyEmphasis)
                    .foregroundStyle(Theme.textSecondary.color)
            }
            .help("\(model.connectionState.label) — \(model.connectionLabel)")
        }

        ToolbarItem(placement: .primaryAction) {
            Button {
                model.runCurrentQuery()
            } label: {
                Label("Run", systemImage: "play.fill")
            }
            .keyboardShortcut("r", modifiers: .command)
            .disabled(model.isBusy || !model.canRun)
            .help("Run the current query (⌘R)")
        }
    }
}

// MARK: - Navigator

struct NavigatorView: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(spacing: 0) {
            SidebarFilterField(text: $model.navigatorFilter, focus: $focus)
                .padding(.horizontal, Theme.Space.sm)
                .padding(.vertical, Theme.Space.sm)

            if model.schemas.isEmpty {
                EmptyState(
                    symbol: "server.rack",
                    title: "No schemas",
                    hint: "Nothing to browse on this connection yet.")
            } else if model.matchedRelationCount == 0, model.isFiltering {
                // A filtered tree that matched nothing has to say so. Left
                // blank it reads as a navigator that failed to load, which is
                // the one reading that would send someone looking for a bug
                // instead of for a shorter word.
                EmptyState(
                    symbol: "magnifyingglass",
                    title: "No matches",
                    hint: "No relation or schema is named like “\(model.navigatorFilter)”.")
            } else if model.matchedRelationCount == 0 {
                // Same shape, different fact: the schemas are there and hold
                // nothing. Saying "no matches" here would blame a filter that
                // is not switched on.
                EmptyState(
                    symbol: "square.stack.3d.up",
                    title: "No objects",
                    hint: "These schemas hold no tables or views.")
            } else {
                List(selection: $model.navigatorSelection) {
                    ForEach(model.schemas) { schema in
                        let relations = model.visibleRelations(in: schema.name)
                        if !relations.isEmpty {
                            DisclosureGroup(isExpanded: expansion(for: schema.name)) {
                                ForEach(relations) { relation in
                                    NavigatorRow(relation: relation)
                                        .tag(relation)
                                }
                            } label: {
                                SchemaLabel(name: schema.name, count: relations.count)
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
            }
        }
        .safeAreaInset(edge: .bottom) {
            // Object count belongs where the objects are, not in the main status
            // bar, which describes the result set.
            HStack {
                Text(countLabel)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                Spacer()
                // Beside the count, because the two are about the same thing:
                // what this tree currently holds, and how to make "currently"
                // true again. ⇧⌘R reaches it too, but a shortcut is invisible,
                // and a list that can silently go stale needs a visible way to
                // un-stale it. Not in the toolbar — that side of the window is
                // about the result, and this is about the sidebar.
                Button {
                    model.refresh()
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 10, weight: .medium))
                        .frame(width: 18, height: 16)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                // Coloured rather than left to the button style's dimming, so
                // the disabled state reads at 10pt on a dark background.
                .foregroundStyle(
                    model.canRefresh ? Theme.textSecondary.color : Theme.textTertiary.color
                )
                .disabled(!model.canRefresh)
                .help("Reload schemas and objects from the database (⇧⌘R)")
                .accessibilityLabel("Refresh objects")
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.xs + 2)
            .background(Theme.surface.color)
            .overlay(alignment: .top) {
                Rectangle().fill(Theme.separator.color).frame(height: 1)
            }
        }
    }

    private var countLabel: String {
        let matched = model.matchedRelationCount
        let total = model.totalRelationCount
        return matched == total
            ? AppModel.pluralized(total, "object")
            : "\(matched) of \(AppModel.pluralized(total, "object"))"
    }

    private func expansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { model.isExpanded(schema) },
            set: { isOpen in
                // Dropped while filtering. The getter answers from the matches
                // then, not from this set, so a write would silently edit the
                // arrangement the user gets back when they clear the field
                // without changing anything on screen now.
                guard !model.isFiltering else { return }
                if isOpen { model.expanded.insert(schema) } else { model.expanded.remove(schema) }
            })
    }
}

private struct SchemaLabel: View {
    let name: String
    let count: Int

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: "square.stack.3d.up")
                .font(.system(size: 10))
                .foregroundStyle(Theme.textSecondary.color)
            Text(name)
                .font(Theme.Typography.bodyEmphasis)
            Spacer(minLength: Theme.Space.xs)
            Text("\(count)")
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textTertiary.color)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Schema \(name), \(AppModel.pluralized(count, "object"))")
    }
}

struct NavigatorRow: View {
    let relation: RelationInfo

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: relation.kind.symbol)
                .font(.system(size: 11))
                .foregroundStyle(
                    relation.kind == .table ? Theme.accent.color : Theme.textSecondary.color
                )
                .frame(width: 14)

            Text(relation.name)
                .font(Theme.Typography.body)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.xs)

            if relation.estimatedRows > 0 {
                // Marked as approximate because it is: pg_class.reltuples is
                // whatever the last ANALYZE saw, and every write since has
                // drifted from it. The status bar already writes "~1,000,000"
                // for the same number, and a bare figure here would leave the
                // navigator as the one place claiming an exact count.
                Text("~\(AppModel.formatted(relation.estimatedRows))")
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.textTertiary.color)
            }
        }
        .padding(.vertical, 1)
        .help("\(relation.kind.label) · ~\(AppModel.formatted(relation.estimatedRows)) rows")
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "\(relation.name), \(relation.kind.label), "
                + "about \(AppModel.formatted(relation.estimatedRows)) rows")
    }
}

// MARK: - Detail

struct DetailPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(spacing: 0) {
            TabBar(selection: $model.activeTab)
            Rectangle().fill(Theme.separator.color).frame(height: 1)

            if let error = model.errorMessage {
                InlineBanner(message: error) { model.errorMessage = nil }
                    .transition(.move(edge: .top).combined(with: .opacity))
            }

            Group {
                switch model.activeTab {
                case .structure: StructurePane(model: model, focus: $focus)
                case .content: ContentPane(model: model, focus: $focus)
                case .query: QueryPane(model: model, focus: $focus)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            Rectangle().fill(Theme.separator.color).frame(height: 1)
            StatusBar(model: model)
        }
        .background(Theme.background.color)
        .animation(Theme.Motion.ease(reduceMotion), value: model.errorMessage)
        // Focus follows the tab. Without this, switching to Query leaves the
        // caret wherever it was and the editor needs a click before it accepts
        // typing; and on the other tabs SwiftUI parks focus on the first text
        // field it finds, which puts a ring on the sidebar filter.
        //
        // Set explicitly rather than through `.defaultFocus`, which does not
        // take effect under an `NSHostingView`. Run from `.task` rather than
        // `.onChange(initial:)` because the automatic assignment this is
        // overriding happens after the first layout pass, and an override that
        // runs before it simply loses.
        .task(id: model.activeTab) {
            switch model.activeTab {
            case .query: focus = .editor
            case .structure: focus = .structureTable
            // Content hands focus to the Metal grid, which claims first
            // responder itself; clearing SwiftUI's focus is what lets it.
            case .content: focus = nil
            }
        }
    }
}

// MARK: - Panes

/// Which of the Structure tab's lower sections is showing. Not reset when the
/// relation changes: someone comparing triggers across tables is doing exactly
/// that, and snapping back to Indexes on every click would fight them.
enum StructureDetail: String, CaseIterable, Identifiable {
    case indexes = "Indexes"
    case foreignKeys = "Foreign keys"
    case referencedBy = "Referenced by"
    case constraints = "Constraints"
    case triggers = "Triggers"
    /// Offered only for a relation that has one; see `AppModel.structureSections`.
    case definition = "Definition"

    var id: String { rawValue }
}

struct StructurePane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?
    @State private var detail: StructureDetail = .indexes

    var body: some View {
        if model.columns.isEmpty {
            EmptyState(
                symbol: "list.bullet.rectangle",
                title: "No structure to show",
                hint: "Choose a table or view in the sidebar.")
        } else {
            VSplitView {
                columnsTable
                    .frame(minHeight: 160)

                // The sections will not fit side by side, so they share one area
                // and the strip selects between them. A list section is listed
                // even at zero: the count is the answer to "does this table have
                // triggers", and hiding the row would make the strip's contents
                // shift from table to table.
                VStack(spacing: 0) {
                    StructureDetailStrip(
                        model: model,
                        selected: Binding(get: { section }, set: { detail = $0 }))
                    detailTable
                }
                .frame(minHeight: 110, idealHeight: 190, maxHeight: 340)
                .task {
                    if let opened = model.initialStructureDetail { detail = opened }
                }
            }
        }
    }

    /// The section actually on screen. `detail` is the user's last pick, which
    /// may name Definition while a table is selected — remembered on purpose, so
    /// stepping through a list of views does not reset the strip on the one
    /// table in the middle, but not something this can show.
    private var section: StructureDetail {
        model.structureSections.contains(detail) ? detail : .indexes
    }

    @ViewBuilder
    private var detailTable: some View {
        switch section {
        case .indexes:
            table(model.indexes, empty: "No indexes") { indexesTable }
        case .foreignKeys:
            table(model.foreignKeys, empty: "No foreign keys") { foreignKeysTable }
        case .referencedBy:
            table(model.referencedBy, empty: "Nothing references this table") {
                referencedByTable
            }
        case .constraints:
            table(model.constraints, empty: "No check or unique constraints") {
                constraintsTable
            }
        case .triggers:
            table(model.triggers, empty: "No triggers") { triggersTable }
        case .definition:
            // `section` only names this for a relation that has one, so the
            // fallback is unreachable — it exists because the switch must be
            // total, not because a blank Definition section is a state.
            if let sql = model.definition {
                definitionText(sql)
            } else {
                emptyLine("No definition")
            }
        }
    }

    /// A section's table, or a line saying it is empty. An empty `Table` draws
    /// a bare header over nothing, which reads as a table that failed to load.
    @ViewBuilder
    private func table<T>(
        _ rows: [T], empty: String, @ViewBuilder content: () -> some View
    ) -> some View {
        if rows.isEmpty {
            emptyLine(empty)
        } else {
            content()
        }
    }

    private func emptyLine(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.textTertiary.color)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// The view's defining statement, verbatim.
    ///
    /// The one section that is prose rather than rows, so it gets a scroll view
    /// instead of a `Table`: the statement runs to tens of lines and the strip
    /// is a few hundred points tall. Scrolls both ways and does not wrap —
    /// re-flowing SQL destroys the indentation that the server put there to make
    /// it readable, and a wrapped line reads as part of the one below it.
    /// Selectable because the useful thing to do with a definition is paste it
    /// into the Query tab.
    private func definitionText(_ sql: String) -> some View {
        ScrollView([.vertical, .horizontal]) {
            Text(sql)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.text.color)
                .textSelection(.enabled)
                .fixedSize()
                .padding(.horizontal, Theme.Space.md)
                .padding(.vertical, Theme.Space.sm)
        }
        // A two-axis scroll view centres content smaller than its viewport, so
        // a short definition lands in the middle of the pane like a caption.
        // The anchor puts it where reading starts.
        .defaultScrollAnchor(.topLeading)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityLabel("View definition")
    }

    private var columnsTable: some View {
        Table(model.columns) {
            TableColumn("") { column in
                // The key marker earns a column of its own: it is the first
                // thing anyone looks for in a structure view. The tooltip
                // and label carry it too, so it is not colour-only.
                if column.isPrimaryKey {
                    Image(systemName: "key.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(Theme.warning.color)
                        .help("Primary key")
                        .accessibilityLabel("Primary key")
                }
            }
            .width(18)

            TableColumn("Column") { column in
                Text(column.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
            }

            TableColumn("Type") { column in
                Text(column.dataType)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
            }

            TableColumn("Null") { column in
                // Words, not a checkmark: "NO" is the constraint a reader
                // is scanning for, and a bare glyph makes them guess which
                // direction it means.
                Text(column.nullable ? "YES" : "NO")
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(
                        column.nullable
                            ? Theme.textTertiary.color : Theme.text.color)
            }
            .width(48)

            TableColumn("Default") { column in
                Text(column.defaultValue ?? "—")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
            }
        }
        // Striping off. AppKit paints the alternating background across the
        // table's whole height, so the area past the last column renders as
        // a stack of empty bars that read as rows the table failed to fill.
        .tableStyle(.inset(alternatesRowBackgrounds: false))
        // A focus target so this pane has somewhere for focus to be.
        // Clearing focus is not enough — SwiftUI then falls back to the
        // only text field on screen, which is the sidebar's filter, and the
        // tab opens with a ring on a control in a different pane.
        .focusable()
        .focused($focus, equals: .structureTable)
    }

    private var indexesTable: some View {
        Table(model.indexes) {
            TableColumn("") { index in
                if index.isPrimary {
                    Image(systemName: "key.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(Theme.warning.color)
                        .help("Primary key")
                        .accessibilityLabel("Primary key")
                }
            }
            .width(18)

            TableColumn("Name") { index in
                // Index names are long and the column is narrow, so the
                // tooltip is what makes a truncated one recoverable.
                Text(index.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                    .help(index.name)
            }

            TableColumn("Keys") { index in
                // The predicate is part of what the index covers, so it rides
                // with the keys rather than being dropped: a partial index
                // shown as a plain one claims coverage it lacks.
                Text(Self.keyLabel(index))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
                    .help(Self.keyLabel(index))
            }

            TableColumn("Kind") { index in
                Text(index.kindLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
            }
            .width(min: 70, ideal: 96)
        }
        .tableStyle(.inset(alternatesRowBackgrounds: false))
    }

    private static func keyLabel(_ index: IndexInfo) -> String {
        index.columns.joined(separator: ", ")
            + (index.predicate.map { " WHERE \($0)" } ?? "")
    }

    private var foreignKeysTable: some View {
        Table(model.foreignKeys) {
            TableColumn("Columns") { key in
                Text(key.localColumns.joined(separator: ", "))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
            }

            TableColumn("References") { key in
                Text(key.otherLabel(sameSchemaAs: model.selected?.schema ?? ""))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
            }

            TableColumn("On") { key in
                Text(key.actionLabel.isEmpty ? "—" : key.actionLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
            }
            .width(min: 80, ideal: 150)
        }
        .tableStyle(.inset(alternatesRowBackgrounds: false))
    }

    private var referencedByTable: some View {
        Table(model.referencedBy) {
            // The referencing table leads, because the question this section
            // answers is "who depends on me", not "through which of my
            // columns".
            TableColumn("From") { key in
                Text(key.otherLabel(sameSchemaAs: model.selected?.schema ?? ""))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
            }

            TableColumn("To columns") { key in
                Text(key.localColumns.joined(separator: ", "))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
            }

            TableColumn("On") { key in
                // ON DELETE CASCADE on an inbound key is the one that decides
                // what happens to other people's rows when you delete yours.
                Text(key.actionLabel.isEmpty ? "—" : key.actionLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(
                        key.onDelete == "CASCADE"
                            ? Theme.warning.color : Theme.textTertiary.color
                    )
                    .lineLimit(1)
            }
            .width(min: 80, ideal: 150)
        }
        .tableStyle(.inset(alternatesRowBackgrounds: false))
    }

    private var constraintsTable: some View {
        Table(model.constraints) {
            TableColumn("Kind") { constraint in
                Text(constraint.kind.label)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.textTertiary.color)
            }
            .width(min: 56, ideal: 66)

            TableColumn("Name") { constraint in
                Text(constraint.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                    .help(constraint.name)
            }

            TableColumn("Definition") { constraint in
                Text(constraint.definition)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
                    .help(constraint.definition)
            }
        }
        .tableStyle(.inset(alternatesRowBackgrounds: false))
    }

    private var triggersTable: some View {
        Table(model.triggers) {
            TableColumn("") { trigger in
                // A disabled trigger listed like any other makes the reader
                // expect behaviour that will not happen, so the state shows
                // before the name does.
                if !trigger.enabled {
                    Image(systemName: "pause.circle")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.textTertiary.color)
                        .help("Disabled")
                        .accessibilityLabel("Disabled")
                }
            }
            .width(18)

            TableColumn("Name") { trigger in
                Text(trigger.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(
                        trigger.enabled ? Theme.text.color : Theme.textTertiary.color
                    )
                    .lineLimit(1)
                    .help(trigger.name)
            }

            TableColumn("When") { trigger in
                Text(trigger.whenLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.textSecondary.color)
                    .lineLimit(1)
            }

            TableColumn("Runs") { trigger in
                Text("\(trigger.function)()")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
                    .help(trigger.function)
            }
        }
        .tableStyle(.inset(alternatesRowBackgrounds: false))
    }
}

/// Selector for the Structure tab's lower sections, each with its count.
///
/// The counts are the point: they answer "does this table have triggers"
/// without a click, which is most of what anyone wants from the list sections
/// most of the time. Definition has no count — it is one value, and the section
/// being offered at all is already the answer.
private struct StructureDetailStrip: View {
    let model: AppModel
    @Binding var selected: StructureDetail

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            ForEach(model.structureSections) { section in
                let count = model.structureDetailCount(section)
                Button {
                    selected = section
                } label: {
                    HStack(spacing: Theme.Space.xs) {
                        Text(section.rawValue)
                            .font(Theme.Typography.caption)
                        if let count {
                            Text("\(count)")
                                .font(Theme.Typography.digits)
                                .foregroundStyle(
                                    selected == section
                                        ? Theme.textSecondary.color : Theme.textTertiary.color)
                        }
                    }
                    .padding(.horizontal, Theme.Space.sm)
                    .frame(height: 20)
                    .background(
                        RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous)
                            .fill(
                                selected == section
                                    ? Theme.surfaceRaised.color : Color.clear)
                    )
                    .foregroundStyle(
                        selected == section ? Theme.text.color : Theme.textSecondary.color)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(count.map { "\(section.rawValue), \($0)" } ?? section.rawValue)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, Theme.Space.sm)
        .frame(height: 26)
        .background(Theme.surface.color)
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
    }
}

struct ContentPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        @Bindable var result = model.browseResult
        return VStack(spacing: 0) {
            if model.selected == nil {
                EmptyState(
                    symbol: "tablecells",
                    title: "Nothing selected",
                    hint: "Choose a table in the sidebar to browse its rows.")
            } else {
                MetalGridView(
                    table: model.browseResult.table,
                    generation: model.browseResult.generation,
                    rowCount: model.browseResult.rowCount,
                    declaredTypes: model.declaredColumnTypes,
                    selection: $result.selection,
                    claimsInitialFocus: true,
                    sort: model.gridSort,
                    onSortColumn: { model.toggleSort(column: $0) }
                )
                .overlay { LoadingVeil(isVisible: model.browseResult.isLoading) }
                .accessibilityLabel("Result grid")

                CellInspector(cell: model.inspectedCell(in: model.browseResult))
            }

            Rectangle().fill(Theme.separator.color).frame(height: 1)
            FilterBar(model: model, focus: $focus)
        }
    }
}

/// WHERE and ORDER BY, the two filters a browse pane actually needs. They build
/// a query rather than filtering in memory, so they work on results larger than
/// what has been fetched.
struct FilterBar: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            let hint = model.filterHint

            FieldLabel(text: "Where")
            CompactField(
                placeholder: hint.where, text: $model.whereClause,
                area: .whereField, focus: $focus, onSubmit: model.applyFilters)

            FieldLabel(text: "Order by")
            CompactField(
                placeholder: hint.order, text: $model.orderClause,
                area: .orderField, focus: $focus, onSubmit: model.applyFilters
            )
            .frame(maxWidth: 190)

            Button("Apply") { model.applyFilters() }
                .controlSize(.small)
                .disabled(model.selected == nil || model.isBusy)
                .help("Re-run the browse query with these filters (↩)")
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
        .background(Theme.surface.color)
    }
}

struct QueryPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        @Bindable var result = model.queryResult
        return VSplitView {
            ZStack(alignment: .bottomTrailing) {
                // The selection binding is what makes ⌘R mean "this statement":
                // without it the pane knows the text and not where in it the
                // user is standing.
                SQLEditor(text: $model.queryText, selection: $model.querySelection)
                    .padding(.horizontal, Theme.Space.md)
                    .padding(.vertical, Theme.Space.sm)
                    .background(Theme.background.color)
                    .focused($focus, equals: .editor)
                    .accessibilityLabel("SQL editor")

                HStack(spacing: Theme.Space.sm) {
                    // Says which statement is about to run, before it runs. A
                    // buffer of five makes ⌘R a guess otherwise, and the wrong
                    // guess is a statement the user did not mean to execute.
                    Text(model.runTarget?.hint ?? "nothing to run")
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.textTertiary.color)
                        .accessibilityHidden(true)

                    // The corner of the editor is where this belongs: it is
                    // about the buffer, not about the result. ⇧⌘H reaches it
                    // too, but a shortcut is invisible, and a list nothing on
                    // screen mentions is a feature only the menu bar knows
                    // about — the same argument the inspector strip's chevron
                    // settled for the value viewer.
                    Button {
                        model.isHistoryOpen.toggle()
                    } label: {
                        Image(systemName: "clock.arrow.circlepath")
                            .font(.system(size: 11, weight: .medium))
                            .frame(width: 18, height: 16)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(
                        model.isHistoryOpen ? Theme.accent.color : Theme.textSecondary.color
                    )
                    .help("Statements this window has run (⇧⌘H)")
                    .accessibilityLabel("Query history")
                }
                .padding(Theme.Space.sm)
            }
            // The split opens at `maxHeight`, so this is the editor's starting
            // size as much as its ceiling: enough for a statement of about ten
            // lines, with the result keeping the rest. Drag for more.
            .frame(minHeight: 72, idealHeight: 120, maxHeight: 200)

            VStack(spacing: 0) {
                // Directly under the editor it feeds, and above the outcome
                // list, which describes the run rather than the buffer.
                if model.isHistoryOpen {
                    QueryHistoryPanel(model: model)
                    Rectangle().fill(Theme.separator.color).frame(height: 1)
                }

                // Only for a run of several. A ⌘R over one statement has one
                // outcome and the grid is already showing it; a list of one
                // would be chrome charged to the common case to describe the
                // rare one.
                if model.scriptSteps.count > 1 {
                    ScriptOutcomeList(model: model)
                    Rectangle().fill(Theme.separator.color).frame(height: 1)
                }

                // Until this pane has run something there is nothing to show.
                // It used to fall back to the browse's grid, which put rows
                // under a statement that had not produced them.
                if let step = model.selectedScriptStep {
                    if step.outcome.hasGrid {
                        MetalGridView(
                            table: model.queryResult.table,
                            generation: model.queryResult.generation,
                            rowCount: model.queryResult.rowCount,
                            selection: $result.selection
                        )
                        .overlay { LoadingVeil(isVisible: model.queryResult.isLoading) }
                        .accessibilityLabel("Query result grid")

                        CellInspector(cell: model.inspectedCell(in: model.queryResult))
                    } else {
                        // A statement that returned no rows still has an answer,
                        // and an empty grid with no columns is not it — that
                        // reads as a query that broke rather than as an UPDATE
                        // that worked.
                        StatementNote(step: step)
                            .overlay { LoadingVeil(isVisible: model.queryResult.isLoading) }
                    }
                } else {
                    EmptyState(
                        symbol: "terminal",
                        title: "No results yet",
                        hint: "Press ⌘R to run the statement above, ⌥⌘R for all of them."
                    )
                    .overlay { LoadingVeil(isVisible: model.queryResult.isLoading) }
                }
            }
            .frame(minHeight: 160)
        }
    }
}

/// What this window has run, newest first, with a way back into the editor.
///
/// It sits in the pane rather than in a popover or a sheet, and that is the
/// load-bearing decision. `CellValueViewer` gives most of the argument — a sheet
/// takes the key window and a popover closes on the first key that is not its
/// own — and this list adds one of its own: a screenshot is how everything in
/// this window is checked, `tools/capture-window.swift` captures a single window
/// by id, and a popover is a window of its own. A history nobody can capture is
/// a history nobody can review.
///
/// It is toggled rather than always present because the pane's vertical space is
/// already contested between the editor and the result, and a list of past
/// statements is worth less than either while it is not being read. Picking a
/// statement closes it again, so the cost is bounded to the moment it is in use.
private struct QueryHistoryPanel: View {
    @Bindable var model: AppModel
    /// Set by the Clear button, cleared by either answer. Clearing is
    /// irreversible, so it is asked in the panel's own header rather than
    /// through an alert: a modal would take the window away from the thing it
    /// is about, which is the objection `InlineBanner` already carries.
    @State private var confirmingClear = false
    @State private var hovered: UUID?

    /// Seven rows before it scrolls, two more than the outcome list gets: this
    /// is a list someone is searching, where that one is a run being read.
    private static let rowHeight: CGFloat = 22
    private static let maxRows = 7

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            if model.history.entries.isEmpty {
                // Says what fills the list rather than that it is empty. This is
                // where someone who found the panel before they needed it is
                // standing.
                Text("Nothing has run yet — ⌘R sends the statement the caret is in.")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                    .frame(maxWidth: .infinity)
                    .frame(height: Self.rowHeight * 2)
            } else {
                list
            }
        }
        .background(Theme.surface.color)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Query history")
    }

    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            if confirmingClear {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.dangerText.color)
                Text(
                    "Delete all \(AppModel.pluralized(model.history.entries.count, "statement"))? "
                        + "This cannot be undone."
                )
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.text.color)

                Spacer(minLength: Theme.Space.sm)

                Button("Cancel") { confirmingClear = false }
                    .controlSize(.small)
                Button("Delete") {
                    model.history.clear()
                    confirmingClear = false
                }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
                .tint(Theme.danger.color)
            } else {
                Text("History")
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.textSecondary.color)
                Text(AppModel.pluralized(model.history.entries.count, "statement"))
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.textTertiary.color)

                Spacer(minLength: Theme.Space.sm)

                if !model.history.entries.isEmpty {
                    Button("Clear…") { confirmingClear = true }
                        .controlSize(.small)
                        .help("Delete every statement in the history")
                }

                Button {
                    model.isHistoryOpen = false
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 9, weight: .bold))
                        .frame(width: 18, height: 18)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.textSecondary.color)
                .help("Hide the history (⇧⌘H)")
                .accessibilityLabel("Hide query history")
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.surfaceRaised.color)
    }

    private var list: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(model.history.entries) { entry in
                    Button {
                        model.recall(entry)
                    } label: {
                        row(entry)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .frame(
            height: Self.rowHeight
                * CGFloat(min(model.history.entries.count, Self.maxRows))
        )
    }

    private func row(_ entry: QueryHistoryEntry) -> some View {
        let failed = entry.outcome.isFailure
        return HStack(spacing: Theme.Space.sm) {
            // Shape as well as colour, for the reason `StatusDot` carries one:
            // the row still reads as a failure without colour vision.
            Image(systemName: failed ? "exclamationmark.triangle.fill" : "checkmark.circle")
                .font(.system(size: 9))
                .foregroundStyle(failed ? Theme.dangerText.color : Theme.run.color)
                .frame(width: 12)

            // The statement keeps the content tone whatever happened to it. It
            // is what the row is being scanned for, and a red line of SQL reads
            // as the text itself being the problem.
            Text(entry.preview)
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(Theme.text.color)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(entry.outcome.label)
                .font(Theme.Typography.digits)
                .foregroundStyle(failed ? Theme.dangerText.color : Theme.textSecondary.color)
                .lineLimit(1)

            Text(Self.age(of: entry))
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textTertiary.color)
                .frame(width: 62, alignment: .trailing)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: Self.rowHeight)
        .background(hovered == entry.id ? Theme.surfaceRaised.color : Color.clear)
        .overlay(alignment: .leading) {
            // The same 2pt rule `InlineBanner` wears, so a failure is findable
            // by running an eye down the edge rather than by reading four
            // columns of every row.
            Rectangle()
                .fill(failed ? Theme.danger.color : Color.clear)
                .frame(width: 2)
        }
        .contentShape(Rectangle())
        // Every row is one line of a statement that may be twenty; the tooltip
        // is what makes the rest of it reachable without recalling it first.
        .help(entry.sql)
        .onHover { inside in
            if inside {
                hovered = entry.id
            } else if hovered == entry.id {
                hovered = nil
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "\(entry.preview), \(entry.outcome.label), \(Self.age(of: entry))")
    }

    /// How long ago a statement went out, to one unit.
    ///
    /// Written out rather than handed to `RelativeDateTimeFormatter`, which gets
    /// two things wrong here. It follows the system locale, so on a Mac set to
    /// anything but English it drops a translated phrase into a window that is
    /// English everywhere else; and for a statement sent a moment ago it answers
    /// "in 0 sec.", which points the wrong way in time. One unit, because the
    /// question this column answers is which of two runs a row is, and it is
    /// answered by the number nearest the top.
    private static func age(of entry: QueryHistoryEntry) -> String {
        let seconds = Date().timeIntervalSince(entry.ranAt)
        switch seconds {
        case ..<60: return "just now"
        case ..<3600: return "\(Int(seconds / 60))m ago"
        case ..<86400: return "\(Int(seconds / 3600))h ago"
        default: return "\(Int(seconds / 86400))d ago"
        }
    }
}

/// Every statement of a run, in order, with what each one did.
///
/// The pane has one grid and a script has N results, so this is where the other
/// N-1 go. It may show a subset of the rows — one statement's at a time — but
/// never a subset of the statements: a run that stopped at the third of five
/// lists all five, and says of the last two that they did not run.
private struct ScriptOutcomeList: View {
    @Bindable var model: AppModel

    /// Six rows before it scrolls. Enough for the scripts people actually
    /// paste, and past that the editor above is worth more than a seventh row
    /// of chrome.
    private static let rowHeight: CGFloat = 20
    private static let maxRows = 6

    var body: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(Array(model.scriptSteps.enumerated()), id: \.element.id) { index, step in
                    Button {
                        model.selectedStep = index
                    } label: {
                        row(step, isSelected: index == model.selectedStep)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .frame(
            height: Self.rowHeight
                * CGFloat(min(model.scriptSteps.count, Self.maxRows))
        )
        .background(Theme.surface.color)
        .accessibilityLabel("Statement outcomes")
    }

    private func row(_ step: ScriptStep, isSelected: Bool) -> some View {
        HStack(spacing: Theme.Space.sm) {
            // The ordinal, because the status bar and the editor's corner both
            // count statements and this is the same numbering.
            Text("\(step.id)")
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textTertiary.color)
                .frame(width: 18, alignment: .trailing)

            Text(step.preview)
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(tone(step, isSelected: isSelected))
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(step.outcome.label)
                .font(Theme.Typography.digits)
                .foregroundStyle(outcomeTone(step.outcome))
                .lineLimit(1)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: Self.rowHeight)
        // The chrome's own selected tone, not the grid's. A row tinted with
        // `Grid.selectedRow` reads through the sidebar's vibrancy as a blue band
        // across the navigator at the same height, which is the data surface's
        // vocabulary leaking into a place it was never mixed for.
        .background(isSelected ? Theme.surfaceRaised.color : Color.clear)
        .overlay(alignment: .leading) {
            // The accent bar carries the selection where the fill is subtle by
            // design, and puts it at the edge the eye runs down when scanning a
            // list of ordinals.
            Rectangle()
                .fill(isSelected ? Theme.accent.color : Color.clear)
                .frame(width: 2)
        }
        .contentShape(Rectangle())
        .help(step.summary)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Statement \(step.id), \(step.outcome.label)")
        .accessibilityAddTraits(isSelected ? [.isSelected] : [])
    }

    /// A statement that never ran is dimmed to the tone used for labels rather
    /// than content, because that is what it has become: text describing
    /// something that did not happen.
    private func tone(_ step: ScriptStep, isSelected: Bool) -> Color {
        if case .notRun = step.outcome { return Theme.textTertiary.color }
        return isSelected ? Theme.text.color : Theme.textSecondary.color
    }

    private func outcomeTone(_ outcome: StatementOutcome) -> Color {
        switch outcome {
        case .failed: return Theme.dangerText.color
        case .notRun: return Theme.textTertiary.color
        case .rows, .completed: return Theme.textSecondary.color
        }
    }
}

/// What the pane says in place of a grid, for a statement that has no rows.
///
/// The three cases it covers are answers, not absences: an UPDATE that touched
/// four rows, a statement that failed, and one that never ran because of it.
private struct StatementNote: View {
    let step: ScriptStep

    var body: some View {
        VStack(spacing: Theme.Space.sm) {
            Image(systemName: symbol)
                .font(.system(size: 22, weight: .light))
                .foregroundStyle(tint)
            Text(step.note)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.textSecondary.color)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
                .textSelection(.enabled)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background.color)
        .accessibilityElement(children: .combine)
    }

    private var symbol: String {
        switch step.outcome {
        case .failed: return "exclamationmark.triangle"
        case .notRun: return "minus.circle"
        case .rows, .completed: return "checkmark.circle"
        }
    }

    private var tint: Color {
        switch step.outcome {
        case .failed: return Theme.dangerText.color
        case .notRun: return Theme.textTertiary.color
        case .rows, .completed: return Theme.run.color
        }
    }
}

// MARK: - Cell inspector

/// The selected cell, spelled out in full.
///
/// The grid truncates a value to its column width, so this strip is the only
/// place a long value can actually be read. It is always present rather than
/// appearing on selection: a strip that materialises on click makes the grid
/// resize under the cursor mid-interaction.
///
/// One line is enough for a number and for nothing else, so the strip opens into
/// `CellValueViewer` — see that file for why the viewer lives here rather than
/// in a sheet. While it is open the strip stops repeating the first fragment of
/// the value the pane below is already showing, and says what was done to it
/// instead.
struct CellInspector: View {
    let cell: AppModel.InspectedCell?

    var body: some View {
        // Rendered once and handed to both halves. The strip's descriptor and
        // the pane are two readings of the same work, and doing it twice would
        // re-indent a document twice on every arrow key.
        let rendered = cell.flatMap { $0.isExpanded ? RenderedValue.make(from: $0) : nil }
        VStack(spacing: 0) {
            strip(rendered)
            if let rendered {
                Rectangle().fill(Theme.separator.color).frame(height: 1)
                CellValueViewer(rendered: rendered)
            }
        }
    }

    private func strip(_ rendered: RenderedValue?) -> some View {
        HStack(spacing: Theme.Space.sm) {
            if let cell {
                // The shortcut is the fast way in and an invisible one. A strip
                // that can open has to show that it can, or the viewer is a
                // feature only the menu knows about.
                Button(action: cell.toggleExpanded) {
                    Image(systemName: cell.isExpanded ? "chevron.down" : "chevron.right")
                        .font(.system(size: 9, weight: .semibold))
                        .frame(width: 14, height: 18)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.textSecondary.color)
                .help(cell.isExpanded ? "Hide the value (⌥⌘V)" : "Show the value in full (⌥⌘V)")
                .accessibilityLabel(cell.isExpanded ? "Hide value" : "Show value in full")

                Text(cell.column)
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.textSecondary.color)

                if !cell.type.isEmpty {
                    Text(cell.type)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.textTertiary.color)
                }

                if let rendered {
                    Text(rendered.descriptor)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.textTertiary.color)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text(cell.value)
                        .font(Theme.Typography.monoSmall)
                        .foregroundStyle(
                            cell.isNull ? Theme.textTertiary.color : Theme.text.color
                        )
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }

                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(Self.copyText(of: cell), forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc")
                        .font(.system(size: 10))
                        .frame(width: 20, height: 18)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.textSecondary.color)
                .help("Copy value (⌘C)")
                .accessibilityLabel("Copy cell value")
            } else {
                Text("Select a cell to inspect its value")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.surfaceRaised.color)
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
        .accessibilityElement(children: .contain)
    }

    /// What the copy button puts on the pasteboard.
    ///
    /// Never the strip's own rendering. NULL copies as empty rather than as the
    /// word, which the grid's ⌘C decided first and for the same reason; a binary
    /// value copies as the whole `\x…` literal PostgreSQL accepts back, rather
    /// than as the bounded preview on screen — a copy that silently drops the
    /// end of a value is the worst kind of wrong.
    private static func copyText(of cell: AppModel.InspectedCell) -> String {
        guard !cell.isNull else { return "" }
        if case .binary(let bytes) = cell.rendering {
            return "\\x" + ValueRendering.hex(bytes)
        }
        return cell.value
    }
}

// MARK: - Loading

/// Dims the grid while a query runs.
///
/// Stale rows stay visible but visibly inactive: blanking the grid would lose
/// the context the user is comparing against, and leaving it undimmed would
/// present the previous result as if it were the answer.
struct LoadingVeil: View {
    let isVisible: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ZStack {
            if isVisible {
                Theme.background.opacity(0.55).color
                VStack(spacing: Theme.Space.sm) {
                    ProgressView().controlSize(.small)
                    Text("Running…")
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.textSecondary.color)
                }
            }
        }
        .allowsHitTesting(false)
        .animation(Theme.Motion.ease(reduceMotion), value: isVisible)
        .accessibilityHidden(!isVisible)
        .accessibilityLabel("Running query")
    }
}

// MARK: - Status bar

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            // A truncated result is worth catching out of the corner of an eye,
            // not only on a careful read of the count.
            if model.current.capped && model.activeTab != .structure {
                Image(systemName: "rectangle.compress.vertical")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.warning.color)
                    .help(truncationHelp)
                    .accessibilityLabel("Result truncated")
            }

            // The text stays neutral. A partial view is the normal state for a
            // large table, not a warning, and an amber status line that is
            // always on becomes wallpaper.
            Text(model.statusLine)
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textSecondary.color)
                .lineLimit(1)

            // Attached to the sentence it acts on, so "first 100,000 of
            // ~1,000,000" reads as something you can do to rather than only
            // something you were told.
            if model.canLoadMore {
                Button("Load more") { model.loadMore() }
                    .buttonStyle(.link)
                    .font(Theme.Typography.micro)
                    .help("Fetch the next \(AppModel.formatted(model.pageSize)) rows")
            } else if let obstacle = model.pagingObstacle, model.activeTab == .content {
                // In the slot the button would occupy. A missing button with the
                // reason only on hover reads as a bug; hover is not somewhere
                // anyone looks to find out why nothing is there.
                Text("· \(obstacle.label)")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.textTertiary.color)
                    .help(obstacle.detail)
            }

            Spacer(minLength: Theme.Space.sm)

            if model.activeTab != .structure {
                if let cell = model.inspectedCell(in: model.current) {
                    Text(cell.address)
                        .font(Theme.Typography.digits)
                        .foregroundStyle(Theme.textTertiary.color)
                }

                if !model.current.table.columns.isEmpty {
                    Text(AppModel.pluralized(model.current.table.columns.count, "col"))
                        .font(Theme.Typography.digits)
                        .foregroundStyle(Theme.textTertiary.color)
                }
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 24)
        .background(Theme.surface.color)
        .accessibilityElement(children: .contain)
    }

    /// A truncated result that cannot be paged has to say why, or the missing
    /// button reads as a bug rather than as a property of the table.
    private var truncationHelp: String {
        let shown = "Showing the first \(AppModel.formatted(model.current.rowCount)) rows"
        guard let obstacle = model.pagingObstacle else { return shown }
        return "\(shown). \(obstacle.detail)"
    }
}
