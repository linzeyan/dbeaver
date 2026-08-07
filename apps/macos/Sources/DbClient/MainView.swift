import SwiftUI

/// Window shell: navigator on the left, tabbed detail on the right, status
/// along the bottom. The layout follows Sequel Ace — pick an object, then
/// switch between describing it, browsing it, and querying it.

/// Where keyboard focus can be. Named so panes can hand focus to each other and
/// so the filter fields can draw their own focus ring; SwiftUI's default ring
/// does not survive the custom field backgrounds this window uses.
enum FocusArea: Hashable {
    case navigatorFilter
    case whereField
    case orderField
    case editor
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
            } else if model.matchedRelationCount == 0 {
                EmptyState(
                    symbol: "magnifyingglass",
                    title: "No matches",
                    hint: "Nothing named like “\(model.navigatorFilter)”.")
            } else {
                List(selection: $model.selected) {
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
            ? "\(total) objects"
            : "\(matched) of \(total) objects"
    }

    private func expansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { model.isExpanded(schema) },
            set: { isOpen in
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
        .accessibilityLabel("Schema \(name), \(count) objects")
    }
}

struct NavigatorRow: View {
    let relation: RelationInfo

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: relation.kind.symbol)
                .font(.system(size: 11))
                .foregroundStyle(
                    relation.kind == .table ? Theme.accent.color : Theme.textSecondary.color)
                .frame(width: 14)

            Text(relation.name)
                .font(Theme.Typography.body)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.xs)

            if relation.estimatedRows > 0 {
                Text(AppModel.formatted(relation.estimatedRows))
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
                case .structure: StructurePane(model: model)
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
    }
}

// MARK: - Panes

struct StructurePane: View {
    @Bindable var model: AppModel

    var body: some View {
        if model.columns.isEmpty {
            EmptyState(
                symbol: "list.bullet.rectangle",
                title: "No structure to show",
                hint: "Choose a table or view in the sidebar.")
        } else {
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
        }
    }
}

struct ContentPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(spacing: 0) {
            if model.selected == nil {
                EmptyState(
                    symbol: "tablecells",
                    title: "Nothing selected",
                    hint: "Choose a table in the sidebar to browse its rows.")
            } else {
                MetalGridView(
                    table: model.grid,
                    generation: model.gridGeneration,
                    selection: $model.gridSelection,
                    claimsInitialFocus: true)
                    .overlay { LoadingVeil(isVisible: model.isBusy) }
                    .accessibilityLabel("Result grid")

                CellInspector(cell: model.inspectedCell)
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
            FieldLabel(text: "Where")
            CompactField(
                placeholder: "id > 100", text: $model.whereClause,
                area: .whereField, focus: $focus, onSubmit: model.applyFilters)

            FieldLabel(text: "Order by")
            CompactField(
                placeholder: "id desc", text: $model.orderClause,
                area: .orderField, focus: $focus, onSubmit: model.applyFilters)
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
        VSplitView {
            ZStack(alignment: .bottomTrailing) {
                TextEditor(text: $model.queryText)
                    .font(Theme.Typography.editor)
                    .scrollContentBackground(.hidden)
                    .padding(.horizontal, Theme.Space.md)
                    .padding(.vertical, Theme.Space.sm)
                    .background(Theme.background.color)
                    .focused($focus, equals: .editor)
                    .accessibilityLabel("SQL editor")

                Text("⌘R to run")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.textTertiary.color)
                    .padding(Theme.Space.sm)
                    .accessibilityHidden(true)
            }
            // The split opens at `maxHeight`, so this is the editor's starting
            // size as much as its ceiling: enough for a statement of about ten
            // lines, with the result keeping the rest. Drag for more.
            .frame(minHeight: 72, idealHeight: 120, maxHeight: 200)

            VStack(spacing: 0) {
                MetalGridView(
                    table: model.grid,
                    generation: model.gridGeneration,
                    selection: $model.gridSelection)
                    .overlay { LoadingVeil(isVisible: model.isBusy) }
                    .accessibilityLabel("Query result grid")

                CellInspector(cell: model.inspectedCell)
            }
            .frame(minHeight: 160)
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
struct CellInspector: View {
    let cell: AppModel.InspectedCell?

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            if let cell {
                Text(cell.column)
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.textSecondary.color)

                if !cell.type.isEmpty {
                    Text(cell.type)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.textTertiary.color)
                }

                Text(cell.value)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(
                        cell.isNull ? Theme.textTertiary.color : Theme.text.color)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)

                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(cell.isNull ? "" : cell.value, forType: .string)
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
        HStack(spacing: Theme.Space.md) {
            Text(model.statusLine)
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textSecondary.color)
                .lineLimit(1)

            Spacer(minLength: Theme.Space.sm)

            if model.activeTab != .structure {
                if let cell = model.inspectedCell {
                    Text(cell.address)
                        .font(Theme.Typography.digits)
                        .foregroundStyle(Theme.textTertiary.color)
                }

                if !model.grid.columns.isEmpty {
                    Text("\(model.grid.columns.count) cols")
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
}
