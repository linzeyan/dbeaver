import SwiftUI

/// Window shell: navigator on the left, tabbed detail on the right, status
/// along the bottom. The layout follows Sequel Ace — pick an object, then
/// switch between describing it, browsing it, and querying it.
/// Where keyboard focus starts and moves between. Declared so the navigator
/// takes initial focus: otherwise SwiftUI focuses the first text field, putting
/// a focus ring on the filter bar before the user has chosen anything.
enum FocusArea: Hashable {
    case navigator
    case filter
    case editor
}

struct MainView: View {
    @Bindable var model: AppModel
    @FocusState private var focus: FocusArea?

    var body: some View {
        NavigationSplitView {
            NavigatorView(model: model, focus: $focus)
                .navigationSplitViewColumnWidth(min: 180, ideal: 230, max: 400)
        } detail: {
            DetailPane(model: model, focus: $focus)
        }
        .defaultFocus($focus, .navigator)
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Label(model.connectionLabel, systemImage: "cylinder.split.1x2")
                    .labelStyle(.titleAndIcon)
            }
            ToolbarItem(placement: .primaryAction) {
                if model.isBusy {
                    ProgressView().controlSize(.small)
                }
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    model.runCurrentQuery()
                } label: {
                    Label("Run", systemImage: "play.fill")
                }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(model.isBusy)
                .help("Run the current query (⌘R)")
            }
        }
        .alert(
            "Database error",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } })
        ) {
            Button("OK", role: .cancel) { model.errorMessage = nil }
        } message: {
            Text(model.errorMessage ?? "")
        }
    }
}

// MARK: - Navigator

struct NavigatorView: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        List(selection: $model.selected) {
            ForEach(model.schemas) { schema in
                DisclosureGroup(isExpanded: expansion(for: schema.name)) {
                    ForEach(model.relations[schema.name] ?? []) { relation in
                        NavigatorRow(relation: relation)
                            .tag(relation)
                    }
                } label: {
                    Label(schema.name, systemImage: "folder")
                        .font(.system(size: 12, weight: .medium))
                }
            }
        }
        .listStyle(.sidebar)
        .focused($focus, equals: .navigator)
        .safeAreaInset(edge: .bottom) {
            // Object count belongs where the objects are, not in the main status
            // bar, which describes the result set.
            HStack {
                Text("\(totalRelations) objects")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .background(.bar)
        }
    }

    private var totalRelations: Int {
        model.relations.values.reduce(0) { $0 + $1.count }
    }

    private func expansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { model.expanded.contains(schema) },
            set: { isOpen in
                if isOpen { model.expanded.insert(schema) }
                else { model.expanded.remove(schema) }
            })
    }
}

struct NavigatorRow: View {
    let relation: RelationInfo

    var body: some View {
        Label {
            HStack(spacing: 6) {
                Text(relation.name)
                    .font(.system(size: 12))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 4)
                if relation.estimatedRows > 0 {
                    Text(AppModel.formatted(relation.estimatedRows))
                        .font(.system(size: 10).monospacedDigit())
                        .foregroundStyle(.tertiary)
                }
            }
        } icon: {
            Image(systemName: relation.kind.symbol)
                .foregroundStyle(relation.kind == .table ? Color.accentColor : .secondary)
        }
        .help("\(relation.kind.label) · ~\(AppModel.formatted(relation.estimatedRows)) rows")
    }
}

// MARK: - Detail

struct DetailPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(spacing: 0) {
            TabStrip(selection: $model.activeTab)

            Divider()

            Group {
                switch model.activeTab {
                case .structure: StructurePane(model: model)
                case .content: ContentPane(model: model, focus: $focus)
                case .query: QueryPane(model: model, focus: $focus)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            Divider()
            StatusBar(model: model)
        }
        .navigationTitle(model.selected?.name ?? "No selection")
        .navigationSubtitle(model.selected.map { "\($0.kind.label) · \($0.schema)" } ?? "")
    }
}

struct TabStrip: View {
    @Binding var selection: DetailTab

    var body: some View {
        HStack(spacing: 2) {
            ForEach(DetailTab.allCases) { tab in
                Button {
                    selection = tab
                } label: {
                    Label(tab.rawValue, systemImage: tab.symbol)
                        .font(.system(size: 12))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .background(
                            RoundedRectangle(cornerRadius: 5)
                                .fill(selection == tab
                                      ? Color.accentColor.opacity(0.18)
                                      : Color.clear))
                        .foregroundStyle(selection == tab ? Color.accentColor : .secondary)
                }
                .buttonStyle(.plain)
            }
            Spacer()
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(.bar)
    }
}

// MARK: - Panes

struct StructurePane: View {
    @Bindable var model: AppModel

    var body: some View {
        if model.columns.isEmpty {
            EmptyPane(message: "Select a table to see its structure")
        } else {
            Table(model.columns) {
                TableColumn("") { column in
                    // The key marker earns a column of its own: it is the first
                    // thing anyone looks for in a structure view.
                    if column.isPrimaryKey {
                        Image(systemName: "key.fill")
                            .foregroundStyle(.orange)
                            .help("Primary key")
                    }
                }
                .width(20)

                TableColumn("Column") { column in
                    Text(column.name).font(.system(size: 12, design: .monospaced))
                }

                TableColumn("Type") { column in
                    Text(column.dataType)
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(.secondary)
                }

                TableColumn("Nullable") { column in
                    Text(column.nullable ? "YES" : "NO")
                        .font(.system(size: 11))
                        .foregroundStyle(column.nullable ? .secondary : .primary)
                }
                .width(70)

                TableColumn("Default") { column in
                    Text(column.defaultValue ?? "—")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        }
    }
}

struct ContentPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(spacing: 0) {
            if model.selected == nil {
                EmptyPane(message: "Select a table to browse its rows")
            } else {
                MetalGridView(table: model.grid, generation: model.gridGeneration)
            }
            Divider()
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
        HStack(spacing: 8) {
            Text("WHERE").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            TextField("id > 100", text: $model.whereClause)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 12, design: .monospaced))
                .focused($focus, equals: .filter)
                .onSubmit { model.applyFilters() }

            Text("ORDER BY").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            TextField("id DESC", text: $model.orderClause)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 12, design: .monospaced))
                .frame(maxWidth: 180)
                .onSubmit { model.applyFilters() }

            Button("Apply") { model.applyFilters() }
                .controlSize(.small)
                .disabled(model.selected == nil || model.isBusy)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(.bar)
    }
}

struct QueryPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VSplitView {
            TextEditor(text: $model.queryText)
                .font(.system(size: 13, design: .monospaced))
                .focused($focus, equals: .editor)
                // Bounded so the result keeps most of the pane; the split is
                // still draggable when a longer statement needs the room.
                .frame(minHeight: 80, idealHeight: 130, maxHeight: 260)

            MetalGridView(table: model.grid, generation: model.gridGeneration)
                .frame(minHeight: 160)
        }
    }
}

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: 10) {
            Text(model.status)
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(.secondary)
            Spacer()
            if model.loadedRows > 0 {
                Text("\(model.columns.count) cols")
                    .font(.system(size: 11).monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .background(.bar)
    }
}

struct EmptyPane: View {
    let message: String

    var body: some View {
        VStack {
            Spacer()
            Text(message)
                .font(.system(size: 13))
                .foregroundStyle(.tertiary)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
