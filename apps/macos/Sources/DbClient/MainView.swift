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
    /// The cell editor's value field. Named here rather than left anonymous
    /// because `CompactField` draws its own focus ring from a `FocusArea`, and
    /// the ring is most of the point of putting the cell editor in one: it is
    /// the field that writes to the database.
    case cellValue
    /// The find bar's field. Its own case for the reason `cellValue` has one,
    /// and for one more: ⌘F opens the bar from a grid that holds the keyboard,
    /// so something has to move focus into the field, and that something needs a
    /// name to move it to.
    case gridFind
    // The connection form's fields. In the same enum as the panes' because the
    // form is the same window: it replaces the shell rather than floating over
    // it, so focus is only ever in one of these places at a time.
    case connectName
    case connectHost
    case connectPort
    case connectDatabase
    case connectUser
    case connectPassword
    /// The folder path. Its own case for the reason `connectRootCert` has one:
    /// `CompactField` draws its focus ring from a `FocusArea`, and two fields
    /// answering to one would both light up.
    case connectFolder
    /// The CA path. Its own case rather than sharing one, because `CompactField`
    /// draws its focus ring from a `FocusArea` and two fields answering to one
    /// would both light up.
    case connectRootCert
    /// The bastion's five fields. One case each for the reason above, and worth
    /// saying again here: these sit directly under the database's own host, port
    /// and user, so a shared case would ring the wrong half of the form.
    case connectSshHost
    case connectSshPort
    case connectSshUser
    case connectSshKey
    case connectSshSecret
    /// How long to wait for the database. Its own case for the reason the others
    /// have one: the ring is drawn from the area, and a shared case rings two
    /// fields.
    case connectTimeout
    /// How often to ping the idle connection. Its own case for the same reason,
    /// and it sits directly under Timeout, where a shared case would ring both.
    case connectKeepAlive
    /// The chooser's own filter field. Separate from `navigatorFilter` because the
    /// two are never on screen together and each draws its own ring.
    case connectionFilter
    /// A filter row's value field, and the far end of a row that is a range.
    ///
    /// Carrying the row's index because there are as many of these as there are
    /// rows, and `CompactField` draws its focus ring from whichever area it was
    /// given: one case shared by every row would ring all of them at once.
    case filterValue(Int)
    case filterSecond(Int)
}

struct MainView: View {
    @Bindable var model: AppModel
    @FocusState private var focus: FocusArea?

    var body: some View {
        // The connection strip spans the window rather than the detail column,
        // because the tree in the sidebar is the active connection's too.
        VStack(spacing: 0) {
            ConnectionTabBar(model: model)
            // Both columns swap together. A tab with nothing open on it has no
            // tree to navigate and no panes to draw, so what it offers instead
            // is the pair that gets it open: the connections kept on the left,
            // the one being named on the right. The split itself does not move,
            // which is what stops the window relaying out the moment a database
            // answers.
            NavigationSplitView {
                sidebar
            } detail: {
                if model.isShowingConnectionForm {
                    ConnectionFormPane(model: model, focus: $focus)
                } else {
                    DetailPane(model: model, focus: $focus)
                }
            }
        }
        // No animation across the swap, which is not a style choice. SwiftUI
        // cross-fades an `if`/`else` between two view trees by keeping both
        // alive for the duration, and the pane side contains an
        // `NSViewRepresentable` over a Metal layer. The form's layer survived
        // that fade stranded behind the grid, visible as a dark rectangle
        // wherever the grid had no rows to draw over it — and invisible on a
        // full table, which is why it took a four-row view to find.
        .transaction { $0.animation = nil }
        .toolbar { toolbarContent }
        // The View menu's Filter Objects item, arriving as a bumped counter.
        // An `NSMenuItem` action runs outside the view tree and cannot assign
        // to a `@FocusState`; this is the only end of that wire that can.
        .onChange(of: model.filterFocusRequests) { focus = .navigatorFilter }
        .sheet(isPresented: $model.isGoToOpen) { GoToPalette(model: model) }
        .sheet(isPresented: $model.isTransferPickerOpen) { TransferSheet(model: model) }
        .sheet(isPresented: $model.isImportSheetOpen) { ImportSheet(model: model) }
        .sheet(isPresented: $model.isCreateTableSheetOpen) { CreateTableSheet(model: model) }
        .sheet(isPresented: $model.isProcessesOpen) { ProcessesSheet(model: model) }
        .sheet(isPresented: $model.isVariablesOpen) { VariablesSheet(model: model) }
        .sheet(isPresented: $model.isRelationChangeSheetOpen) { RelationChangeSheet(model: model) }
        .sheet(isPresented: $model.isDatabaseChangeSheetOpen) { DatabaseChangeSheet(model: model) }
        .sheet(isPresented: $model.isNewTableSheetOpen) { NewTableSheet(model: model) }
        .sheet(isPresented: $model.isColumnChangeSheetOpen) { ColumnChangeSheet(model: model) }
        .sheet(isPresented: $model.isIndexChangeSheetOpen) { IndexChangeSheet(model: model) }
        // The routine first, matching the panes: while one is selected it is
        // what the window is about, and the table underneath is only what it
        // will go back to.
        .navigationTitle(
            model.selectedRoutine?.name ?? model.selectedSequence?.name ?? model.selected?.name
                ?? "DbClient"
        )
        // Nothing rather than the connection's name when no object is selected.
        // The name was here because, with no selection, the titlebar was the
        // only place saying which database the window was pointed at; the tab
        // strip says it now, and says it beside the tabs it can be switched to.
        // The routine first, matching the title above it. Reading `selected`
        // here while a routine was showing put "Materialized View" under a
        // function's name — the previous selection describing the current one.
        .navigationSubtitle(model.objectSubtitle)
    }

    /// The left column: the tree, the rail it collapses to, or — on a tab with
    /// nothing open on it — the saved connections.
    ///
    /// The width modifier is written twice rather than once on a `Group` around
    /// all three, because the rail's is a single number and the other two share
    /// a range. One expression covering both would have to be a range whose ends
    /// are equal, which is a way of writing 44 that reads as a mistake.
    ///
    /// The system's own sidebar button is taken away here, and it has to be
    /// exactly here. This window has two answers to the same question and only
    /// one of them can be in the toolbar: the system's hides the column
    /// outright, ours narrows it to the rail. Written on the split view instead,
    /// it silently does nothing — the item is contributed by the sidebar's
    /// content, so the content is the only place it can be removed from. And
    /// written *outside* the width modifier rather than inside it, the width
    /// stops arriving: the rail came back 150pt wide, which is the sidebar's
    /// system minimum and what a column with no width of its own falls to.
    /// Both of those were captures, in that order.
    @ViewBuilder private var sidebar: some View {
        if model.isSidebarCollapsed {
            SidebarRail(model: model)
                .toolbar(removing: .sidebarToggle)
                .navigationSplitViewColumnWidth(SidebarRail.width)
        } else {
            Group {
                if model.isShowingConnectionForm {
                    ConnectionListPane(model: model, focus: $focus)
                } else {
                    NavigatorView(model: model, focus: $focus)
                }
            }
            .toolbar(removing: .sidebarToggle)
            .navigationSplitViewColumnWidth(min: 200, ideal: 250, max: 420)
        }
    }

    @ToolbarContentBuilder
    private var toolbarContent: some ToolbarContent {
        // The chip that named the connection is gone, and so is the menu that
        // switched between them: `ConnectionTabBar` carries the colour, the
        // state and the name, and switching is a click on the tab that shows
        // them. What is left in the toolbar are the commands that act on
        // whichever connection the strip has in front.
        //
        // And nothing at all over the connection form. Every item here acts on
        // a pane, and a tab showing the form has none — the rule elsewhere in
        // this window is that a control which can never do anything is worse
        // than no control, and three permanently dim ones is that rule three
        // times. It cost nothing before this round because the form replaced
        // the whole shell, toolbar included.
        if !model.isShowingConnectionForm {
            paneCommands
        }
    }

    @ToolbarContentBuilder
    private var paneCommands: some ToolbarContent {
        // In the slot the system's sidebar button was taken out of, because it
        // is the same command as far as anybody reaching for it is concerned:
        // the leading end of the toolbar, directly above the column it acts on.
        ToolbarItem(placement: .navigation) {
            Button {
                model.toggleSidebar()
            } label: {
                Label(
                    model.isSidebarCollapsed ? "Expand Sidebar" : "Collapse Sidebar",
                    systemImage: "sidebar.leading")
            }
            .help(
                model.isSidebarCollapsed
                    ? "Show the object tree (⌥⌘S)"
                    : "Narrow the object tree to a rail (⌥⌘S)")
        }

        // Back and Forward at the navigation end, where every window that has
        // them puts them. Always present rather than appearing with the first
        // visit: a control that materialises under the pointer is worse than one
        // that is briefly dim.
        ToolbarItem(placement: .navigation) {
            HStack(spacing: 2) {
                Button {
                    model.goBack()
                } label: {
                    Label("Back", systemImage: "chevron.left")
                }
                .disabled(!model.canGoBack)
                .help("Back (⌘[)")
                Button {
                    model.goForward()
                } label: {
                    Label("Forward", systemImage: "chevron.right")
                }
                .disabled(!model.canGoForward)
                .help("Forward (⌘])")
            }
        }

        // Only where there is a transaction to control. Most of the databases
        // here run every statement on a connection borrowed from a pool, where
        // nothing could hold one open, and a Commit button that can never do
        // anything is worse than no button at all.
        ToolbarItem(placement: .primaryAction) {
            if model.canControlTransactions {
                TransactionControl(model: model)
            }
        }

        // One slot, two commands. While something is running, Run is disabled
        // anyway — the connection is serial — so the space it occupies is the
        // obvious place for the only command that is useful then. A separate
        // Stop button beside it would be dimmed for almost the whole life of the
        // window, and the way out of a statement that will not finish must not
        // be the control nobody has ever seen enabled.
        ToolbarItem(placement: .primaryAction) {
            if model.canCancel {
                Button {
                    model.cancelRunningStatement()
                } label: {
                    Label("Stop", systemImage: "stop.fill")
                }
                .help("Stop the running statement (⌘.)")
            } else {
                Button {
                    model.runCurrentQuery()
                } label: {
                    Label("Run", systemImage: "play.fill")
                }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(!model.canRun)
                .help("Run the current query (⌘R)")
            }
        }
    }
}

/// The transaction, in the toolbar: which mode the connection is in, whether
/// anything is uncommitted, and the two commands that end it.
///
/// The mode is a menu rather than a switch because it is the rarer choice of the
/// three controls and the destructive one to get wrong — a switch under the
/// pointer is a mode changed by a mis-click. Commit and Rollback appear only in
/// manual-commit mode, where they can mean something, and are dimmed until there
/// is work in the transaction.
///
/// Amber for an open transaction, and nothing at all for a closed one. The
/// colour is the whole point of the control: what a person needs to notice
/// without looking is that they have changed a database and not told it to keep
/// the change.
private struct TransactionControl: View {
    let model: AppModel

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            Menu {
                Button("Autocommit") { model.setAutocommit(true) }
                Button("Manual Commit") { model.setAutocommit(false) }
            } label: {
                Label(
                    model.transaction.autocommit ? "Autocommit" : "Manual",
                    systemImage: model.hasUncommittedWork
                        ? "circle.inset.filled" : "arrow.triangle.branch")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .foregroundStyle(
                (model.hasUncommittedWork ? Theme.Semantic.warning : Theme.Text.secondary).color
            )
            .help(help)

            if !model.transaction.autocommit {
                Button {
                    model.commit()
                } label: {
                    Label("Commit", systemImage: "checkmark")
                }
                .disabled(!model.hasUncommittedWork || model.isBusy)
                .help("Keep the changes in this transaction (⇧⌘C)")

                Button {
                    model.rollback()
                } label: {
                    Label("Rollback", systemImage: "arrow.uturn.backward")
                }
                .disabled(!model.hasUncommittedWork || model.isBusy)
                .help("Undo everything this transaction has done")
            }
        }
    }

    private var help: String {
        if model.transaction.autocommit {
            return "Every statement is kept as it runs. Switch to hold them until you commit."
        }
        return model.hasUncommittedWork
            ? "A transaction is open with work in it that the database has not been told to keep."
            : "Statements will be held until you commit."
    }
}

// MARK: - Navigator

struct NavigatorView: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?
    /// Open, and not remembered between launches: the database this connection
    /// is on is the one thing in this tree certain to be worth looking at, so a
    /// collapsed one is a state to be able to reach and not one to come back to.
    @State private var isDatabaseExpanded = true

    /// The schemas of whichever database this connection is open on.
    ///
    /// A property rather than two copies of the same block: it is drawn under
    /// the connection for an engine with no database level and under the current
    /// database for one that has, and two copies would be two places for the row
    /// styling to drift apart.
    @ViewBuilder private var schemaRows: some View {
        let first = firstDrawnSchema
        ForEach(model.visibleSchemas) { schema in
            let relations = model.visibleRelations(in: schema.name)
            let routines = model.visibleRoutines(in: schema.name)
            let sequences = model.visibleSequences(in: schema.name)
            if !relations.isEmpty || !routines.isEmpty || !sequences.isEmpty {
                DisclosureGroup(isExpanded: expansion(for: schema.name)) {
                    ForEach(relations) { relation in
                        NavigatorRow(relation: relation)
                            .tag(NavigatorNode.relation(relation))
                            // Only where the core writes the statements. The
                            // items are absent rather than present and refusing,
                            // which is the whole reason `changesRelations` is
                            // asked of the connection instead of the row.
                            .contextMenu {
                                if model.changesRelations {
                                    ForEach(TableChange.allCases, id: \.self) { change in
                                        Button(change.menuTitle) {
                                            model.prepareRelationChange(change, of: relation)
                                        }
                                    }
                                }
                            }
                            // The window's own selected tone, now that the
                            // system's is switched off below. A shade stronger
                            // than the grid's 0.18, which is read with a
                            // brighter cell cursor over it; this band is the
                            // only layer there is.
                            .listRowBackground(
                                model.navigatorSelection == .relation(relation)
                                    ? Theme.Accent.selection.opacity(0.22).color
                                    : Color.clear)
                    }
                    // The tables stay where they were, at the schema's own
                    // indent, and only the routines are behind a group. Two more
                    // groups for Tables and Views were the other shape, and they
                    // cost a click on the row every session opens with in order
                    // to separate two kinds the icon already tells apart. What
                    // earns this one is that a routine is not browsable at all:
                    // it is the row whose click does something else.
                    if !routines.isEmpty {
                        DisclosureGroup(isExpanded: routineExpansion(for: schema.name)) {
                            ForEach(routines) { routine in
                                RoutineRow(routine: routine)
                                    .tag(NavigatorNode.routine(routine))
                                    .listRowBackground(
                                        model.navigatorSelection == .routine(routine)
                                            ? Theme.Accent.selection.opacity(0.22).color
                                            : Color.clear)
                            }
                        } label: {
                            GroupLabel(
                                title: "Routines", symbol: "function", count: routines.count)
                        }
                    }
                    if !sequences.isEmpty {
                        DisclosureGroup(isExpanded: sequenceExpansion(for: schema.name)) {
                            ForEach(sequences) { sequence in
                                SequenceRow(sequence: sequence)
                                    .tag(NavigatorNode.sequence(sequence))
                                    .listRowBackground(
                                        model.navigatorSelection == .sequence(sequence)
                                            ? Theme.Accent.selection.opacity(0.22).color
                                            : Color.clear)
                            }
                        } label: {
                            GroupLabel(
                                title: "Sequences", symbol: "number", count: sequences.count)
                        }
                    }
                } label: {
                    SchemaLabel(
                        name: schema.name,
                        count: relations.count + routines.count + sequences.count,
                        noun: model.containerNoun
                    )
                    .background(highlightOff(ifFirst: schema.name == first))
                    // Only where this level *is* the database level. On
                    // PostgreSQL a schema is a namespace inside the database and
                    // `DROP DATABASE` aimed at one would name something that is
                    // not there; on MySQL these rows are the databases, and the
                    // tree already calls them that.
                    .contextMenu {
                        if model.changesDatabases, model.capabilities.schemaIsTheDatabase {
                            Button(DatabaseChange.drop.menuTitle) {
                                model.prepareDatabaseChange(.drop, named: schema.name)
                            }
                        }
                    }
                }
            }
        }
    }

    /// What the empty filter result lists, which is what the filter searched.
    ///
    /// Naming a level this connection does not have would send somebody looking
    /// for a row that was never going to be there.
    private var noMatchesHint: String {
        let levels =
            model.hasDatabaseLevel
            ? "database, schema or relation" : "relation or \(model.containerNoun)"
        return "No \(levels) is named like “\(model.navigatorFilter)”."
    }

    /// The first schema that draws a row, which is not always the first schema:
    /// one holding nothing visible is skipped entirely.
    private var firstDrawnSchema: String? {
        model.visibleSchemas.first { model.hasVisibleObjects(in: $0.name) }?.name
    }

    /// Where `ListSelectionHighlightOff` rides now that the connection root row
    /// it used to sit on is gone.
    ///
    /// The tree's first row is a database on engines that have them and a schema
    /// on the ones that do not, so both offer it and only the first of each
    /// accepts. The walk it performs is the same from any row in the table, and
    /// two carriers would set the style twice to the value it already has —
    /// which is why offering it from both is cheaper than deciding between them.
    @ViewBuilder private func highlightOff(ifFirst isFirst: Bool) -> some View {
        if isFirst { ListSelectionHighlightOff() }
    }

    var body: some View {
        VStack(spacing: 0) {
            SidebarFilterField(
                text: $model.navigatorFilter, focus: $focus, noun: model.containerNoun
            )
            .padding(.horizontal, Theme.Space.sm)
            .padding(.vertical, Theme.Space.sm)

            if model.visibleSchemas.isEmpty, !model.hasDatabaseLevel {
                EmptyState(
                    symbol: "server.rack",
                    title: "No \(model.containerNoun)s",
                    hint: "Nothing to browse on this connection yet.")
            } else if model.matchedObjectCount == 0, model.isFiltering,
                model.visibleDatabases.isEmpty
            {
                // A filtered tree that matched nothing has to say so. Left
                // blank it reads as a navigator that failed to load, which is
                // the one reading that would send someone looking for a bug
                // instead of for a shorter word.
                //
                // `visibleDatabases` rather than `hasDatabaseLevel`: a tree with
                // a database level is empty when no database name matched
                // either, and the condition that asked whether the level exists
                // could never reach this state on the engines that have one.
                EmptyState(
                    symbol: "magnifyingglass",
                    title: "No matches",
                    hint: noMatchesHint)
            } else if model.matchedObjectCount == 0, model.visibleDatabases.isEmpty {
                // Same shape, different fact: the schemas are there and hold
                // nothing. Saying "no matches" here would blame a filter that
                // is not switched on.
                EmptyState(
                    symbol: "square.stack.3d.up",
                    title: "No objects",
                    hint: "These \(model.containerNoun)s hold nothing to browse.")
            } else {
                // No row for the connection at the top. The strip across the
                // window names it, in the control that also switches between
                // them, and a root row here would be the same claim one indent
                // further in — paid for by every row below it, in a column this
                // narrow.
                List(selection: $model.navigatorSelection) {
                    if model.hasDatabaseLevel {
                        ForEach(Array(model.visibleDatabases.enumerated()), id: \.element.id) {
                            index, database in
                            if database.isCurrent {
                                DisclosureGroup(isExpanded: databaseExpansion) {
                                    schemaRows
                                } label: {
                                    DatabaseLabel(name: database.name, isCurrent: true)
                                        .background(highlightOff(ifFirst: index == 0))
                                }
                            } else {
                                // No disclosure triangle, because there is
                                // nothing behind it: this connection is open
                                // on one database and no schemas have been
                                // read for the others. A row that opened to
                                // nothing would read as an empty database
                                // rather than an unread one.
                                DatabaseLabel(name: database.name, isCurrent: false)
                                    .background(highlightOff(ifFirst: index == 0))
                                    // The whole row, not just the words: a label
                                    // is as wide as its text, and a double-click
                                    // that only counts over the name reads as a
                                    // row that sometimes works.
                                    .contentShape(Rectangle())
                                    // Double-click moves this tab, which is
                                    // what picking a database in a sidebar
                                    // means everywhere else. What it costs
                                    // depends on the connection — a `USE` where
                                    // the databases are containers on it, a
                                    // reconnect where a database *is* a
                                    // connection — and the expensive case is
                                    // why it stays a deliberate gesture rather
                                    // than following the selection. The tab it
                                    // moves is the one whose tree is being read.
                                    .onTapGesture(count: 2) {
                                        model.switchDatabase(to: database.name)
                                    }
                                    .contextMenu {
                                        Button("Switch to \(database.name)") {
                                            model.switchDatabase(to: database.name)
                                        }
                                        // Kept, and second. It is the answer
                                        // when the tab in front is holding
                                        // something worth keeping — a staged
                                        // edit, an open transaction, a result
                                        // being read — which is also when the
                                        // switch refuses.
                                        Button("Open in New Tab") {
                                            model.openDatabase(database.name)
                                        }
                                        // Last, and only on the rows this tab
                                        // is not connected to — which is every
                                        // row that draws this menu. The server
                                        // refuses to drop a database a session
                                        // is on, and the current row is drawn
                                        // by the branch above with no menu at
                                        // all.
                                        if model.changesDatabases {
                                            Divider()
                                            Button(DatabaseChange.drop.menuTitle) {
                                                model.prepareDatabaseChange(
                                                    .drop, named: database.name)
                                            }
                                        }
                                    }
                                    .help("Double-click to open \(database.name) in this tab")
                            }
                        }
                    } else {
                        schemaRows
                    }
                }
                .listStyle(.sidebar)
                // Dimmed rather than hidden behind a spinner. What somebody
                // reopening a connection wants is usually one table they were
                // already looking at, and a progress view would cover exactly
                // that. Dimming says the tree is provisional without taking it
                // away.
                .opacity(model.isTreeStale ? 0.5 : 1)
                // And not clickable while it is. Picking a relation starts a
                // browse, and there is no connection yet for one to run down —
                // the row would answer with an empty grid and no reason for it,
                // which is a worse answer than a row that waits.
                .disabled(model.isTreeStale)
            }
        }
        // Opaque by default, which costs the sidebar its system translucency and
        // buys back a navigator that is showing only the navigator.
        // `NavigationSplitView` lets the detail column's backgrounds run under
        // the sidebar and the sidebar's vibrancy samples them, so every
        // full-width band on the right is smeared across the tree at its own
        // height — the Structure tab's section strip drew a visible lighter
        // stripe through the middle of the object list, at the exact y of a
        // control in a different pane.
        //
        // This is the leak `ScriptOutcomeList` documents further down, where it
        // was answered by choosing a tone that did not show through. A strip
        // cannot take that answer, since being a band is what it is, so the
        // sampling has to stop instead. Nothing this window needs is lost: it
        // overrides the system appearance and takes every other surface from
        // `Theme`, so the sidebar is the one place a colour nobody chose could
        // still appear.
        //
        // A setting, because a translucent sidebar is a strong Mac signal and
        // wanting it back is a defensible taste — with the smear as its stated
        // price. `Color.clear` rather than no modifier at all so both branches
        // are one expression: what changes is whether anything is drawn over the
        // material, and that reads better than a conditional modifier.
        .background(
            model.preferences.usesTranslucentSidebar ? Color.clear : Theme.Surface.canvas.color
        )
        .safeAreaInset(edge: .bottom) {
            // Object count belongs where the objects are, not in the main status
            // bar, which describes the result set.
            HStack {
                Text(countLabel)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.tertiary.color)
                Spacer()
                // The pair every navigator has at the bottom left, and until
                // there was a `CREATE TABLE` to put behind the first of them
                // there was nothing here but the count. Minus is the drop sheet
                // the row's own menu opens — the same statement, reached from
                // the selection rather than from a right-click — so nothing here
                // acts on a click: both open something that shows the statement.
                Button {
                    model.prepareNewTable()
                } label: {
                    Image(systemName: "plus")
                        .font(.system(size: 10, weight: .medium))
                        .frame(width: 18, height: 16)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(
                    model.makesTables ? Theme.Text.secondary.color : Theme.Text.tertiary.color
                )
                .disabled(!model.makesTables)
                .help("Make a table in this \(model.containerNoun)")
                .accessibilityLabel("New table")

                Button {
                    if let relation = model.selected {
                        model.prepareRelationChange(.drop, of: relation)
                    }
                } label: {
                    Image(systemName: "minus")
                        .font(.system(size: 10, weight: .medium))
                        .frame(width: 18, height: 16)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(
                    model.canDropSelected ? Theme.Text.secondary.color : Theme.Text.tertiary.color
                )
                .disabled(!model.canDropSelected)
                .help(
                    model.selected.map { "Drop \($0.schema).\($0.name)" }
                        ?? "Select an object to drop it"
                )
                .accessibilityLabel("Drop the selected object")

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
                    model.canRefresh ? Theme.Text.secondary.color : Theme.Text.tertiary.color
                )
                .disabled(!model.canRefresh)
                .help("Reload \(model.containerNoun)s and objects from the database (⇧⌘R)")
                .accessibilityLabel("Refresh objects")
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.xs + 2)
            .background(Theme.Surface.raised.color)
            .overlay(alignment: .top) {
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            }
        }
    }

    private var countLabel: String {
        let matched = model.matchedObjectCount
        let total = model.totalObjectCount
        let counted =
            matched == total
            ? AppModel.pluralized(total, "object")
            : "\(matched) of \(AppModel.pluralized(total, "object"))"
        // Words for what the dimming means, because dimming on its own reads as
        // a control that is switched off. The difference between "you may not
        // touch this" and "this is what was here last time" is the whole of what
        // somebody needs before trusting a table name they can see.
        return model.isTreeStale ? "\(counted) · from last time" : counted
    }

    /// The current database's disclosure, which the filter takes over.
    ///
    /// Same arrangement as `expansion(for:)` one level down, and for the same
    /// reason: matches inside a collapsed group are matches nobody can see, and
    /// a collapse written while filtering would edit the arrangement the field
    /// is supposed to hand back untouched.
    private var databaseExpansion: Binding<Bool> {
        Binding(
            get: { model.isFiltering || isDatabaseExpanded },
            set: { isOpen in
                guard !model.isFiltering else { return }
                isDatabaseExpanded = isOpen
            })
    }

    /// The same arrangement as `expansion(for:)`, one level further in.
    private func routineExpansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { model.isRoutineGroupExpanded(schema) },
            set: { isOpen in
                guard !model.isFiltering else { return }
                if isOpen {
                    model.expandedRoutineGroups.insert(schema)
                } else {
                    model.expandedRoutineGroups.remove(schema)
                }
            })
    }

    /// The same arrangement one group over.
    private func sequenceExpansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { model.isSequenceGroupExpanded(schema) },
            set: { isOpen in
                guard !model.isFiltering else { return }
                if isOpen {
                    model.expandedSequenceGroups.insert(schema)
                } else {
                    model.expandedSequenceGroups.remove(schema)
                }
            })
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

/// Switches off the system's selection fill on the `List` this sits inside.
///
/// SwiftUI offers no way to colour a `List`'s selection, and both of the
/// supported approaches were tried against a screenshot before this one.
/// `.tint` reaches only the emphasized fill — the one drawn while the list is
/// first responder — and leaves untouched the unemphasized grey a sidebar
/// wears for most of its life, which is exactly the state this one is in
/// whenever somebody is reading the pane beside it. `.listRowBackground` draws
/// *behind* the system fill rather than instead of it, so both appeared at
/// once, which read worse than the grey alone.
///
/// Placed in the background of a row rather than of the `List`: a row's backing
/// view sits inside the table's own scroll view, so `enclosingScrollView` is a
/// short and certain walk, while a background on the `List` is a sibling of
/// that scroll view and would have to go hunting for it. The root row is the
/// one that is always there to carry it.
///
/// The cost is a reach into SwiftUI's own view hierarchy, and a macOS release
/// that changes the shape of it stops this finding the table. It fails by doing
/// nothing — the sidebar goes back to the grey it has today — which is what
/// makes the reach affordable at all.
///
/// What goes with the system fill is its second signal: grey meant "selected,
/// but the keyboard is somewhere else" and blue meant "selected, and the arrow
/// keys move here". One indigo says only the first half. Traded deliberately —
/// a tone belonging to no palette in the window was the louder problem.
private struct ListSelectionHighlightOff: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { NSView() }

    /// Deferred by a turn of the run loop: `updateNSView` can run before this
    /// view is in the hierarchy, and there is no scroll view above it to find
    /// until it is. Applied on every update rather than once, so a table
    /// SwiftUI has rebuilt underneath is caught rather than missed.
    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async {
            guard let table = view.enclosingScrollView?.documentView as? NSTableView else {
                return
            }
            table.selectionHighlightStyle = .none
        }
    }
}

/// One database on the server, above the schemas.
///
/// The one this connection is open on is told apart by tone rather than by a
/// word, because it is the row that will be expanded and the others will not —
/// the shape already says it, and a "(current)" suffix would say it again in a
/// sidebar that is short of width.
private struct DatabaseLabel: View {
    let name: String
    let isCurrent: Bool

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: isCurrent ? "cylinder.fill" : "cylinder")
                .font(.system(size: 10))
                .foregroundStyle(
                    isCurrent ? Theme.Accent.selection.color : Theme.Text.tertiary.color)
            Text(name)
                .font(Theme.Typography.bodyEmphasis)
                .foregroundStyle(isCurrent ? Theme.Text.primary.color : Theme.Text.secondary.color)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            isCurrent ? "Database \(name), open" : "Database \(name), not open")
    }
}

/// The level above the relations, whatever this engine calls it.
///
/// One row rather than a second view for the engines whose schemas are their
/// databases: the row is the same row — a name, a count, and the relations
/// behind it — and only the noun and the icon differ. `noun` comes from
/// `AppModel.containerNoun`, which reads the capability rather than the scheme.
///
/// The database icon is the unfilled `cylinder`, the same one `DatabaseLabel`
/// draws for a database this connection is not on. There is nothing to fill
/// here: on these engines every one of these rows is open at once, so a mark
/// for "the current one" would be a distinction the tree does not make.
private struct SchemaLabel: View {
    let name: String
    let count: Int
    let noun: String

    private var isDatabase: Bool { noun == "database" }

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: isDatabase ? "cylinder" : "square.stack.3d.up")
                .font(.system(size: 10))
                .foregroundStyle(Theme.Text.secondary.color)
            Text(name)
                .font(Theme.Typography.bodyEmphasis)
            Spacer(minLength: Theme.Space.xs)
            Text("\(count)")
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "\(noun.capitalized) \(name), \(AppModel.pluralized(count, "object"))")
    }
}

struct NavigatorRow: View {
    let relation: RelationInfo

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: relation.kind.symbol)
                .font(.system(size: 11))
                .foregroundStyle(
                    relation.kind == .table
                        ? Theme.Accent.selection.color : Theme.Text.secondary.color
                )
                .frame(width: 14)

            Text(relation.name)
                .font(Theme.Typography.body)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.xs)

            if let rows = relation.rowsLabel {
                // Marked as approximate because it is: pg_class.reltuples is
                // whatever the last ANALYZE saw, and every write since has
                // drifted from it. The status bar already writes "~1,000,000"
                // for the same number, and a bare figure here would leave the
                // navigator as the one place claiming an exact count.
                Text(rows)
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.Text.tertiary.color)
            }
        }
        .padding(.vertical, 1)
        .help("\(relation.kind.label)" + (relation.rowsLabel.map { " · \($0) rows" } ?? ""))
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "\(relation.name), \(relation.kind.label)"
                + (relation.estimatedRows.map { ", about \(AppModel.formatted($0)) rows" } ?? ""))
    }
}

/// A group inside a schema: a word, a glyph, and how many are behind it.
///
/// Not a `SchemaLabel` with a different noun. That row is a place — it has a
/// name somebody typed in a CREATE statement — and this one is a heading this
/// application invented, so it is set in the secondary tone rather than in the
/// emphasis a name gets.
private struct GroupLabel: View {
    let title: String
    let symbol: String
    let count: Int

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: symbol)
                .font(.system(size: 10))
                .foregroundStyle(Theme.Text.tertiary.color)
            Text(title)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.Text.secondary.color)
            Spacer(minLength: Theme.Space.xs)
            Text("\(count)")
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(title), \(count)")
    }
}

/// One function or procedure.
///
/// The parameters are on the row and not only in the tooltip, because they are
/// what tells two overloads apart: a list showing `age` twice is a list with a
/// bug in it as far as anybody reading it can tell.
struct RoutineRow: View {
    let routine: RoutineInfo

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: routine.kind.symbol)
                .font(.system(size: 11))
                .foregroundStyle(Theme.Text.secondary.color)
                .frame(width: 14)

            Text(routine.signature)
                .font(Theme.Typography.body)
                .lineLimit(1)
                // From the middle, so the name at the front and the closing
                // parenthesis both survive: truncating the tail of
                // `settle_invoice(uuid, numeric, …` leaves every overload of a
                // long name looking identical.
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.xs)
        }
        .padding(.vertical, 1)
        .help(help)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(routine.signature), \(routine.kind.label)")
    }

    private var help: String {
        var parts = [routine.kind.label]
        if let returns = routine.returns { parts.append("returns \(returns)") }
        if let language = routine.language { parts.append(language) }
        return parts.joined(separator: " · ")
    }
}

/// The Structure tab for a function or procedure: what it is, then its source.
///
/// No `VSplitView`. The relation pane splits because its two halves are both
/// lists somebody scrolls independently; here the head is four short facts and
/// the body is one block of text, so a draggable divider would be a control
/// whose only use is to hide the four lines.
struct RoutineStructureView: View {
    let routine: RoutineInfo
    let source: String?
    let isLoading: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            body_
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Theme.Surface.canvas.color)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(routine.signature)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
                .lineLimit(2)
            HStack(spacing: Theme.Space.md) {
                fact(routine.kind.label)
                // Only what the database said. A procedure returns nothing and a
                // driver that does not report the language says nothing, and a
                // dash in either slot would be this window inventing an answer.
                if let returns = routine.returns { fact("returns \(returns)") }
                if let language = routine.language { fact(language) }
                Spacer(minLength: 0)
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }

    private func fact(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.tertiary.color)
    }

    /// Named with the underscore because `body` is taken by the protocol.
    @ViewBuilder private var body_: some View {
        if let source, !source.isEmpty {
            ScrollView([.vertical, .horizontal]) {
                Text(source)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .textSelection(.enabled)
                    .fixedSize()
                    .padding(.horizontal, Theme.Space.md)
                    .padding(.vertical, Theme.Space.sm)
            }
            // The anchor `ddlText` needs, for the reason it needs it: a
            // two-axis scroll view centres content smaller than its viewport.
            .defaultScrollAnchor(.topLeading)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .accessibilityLabel("\(routine.kind.label) source")
        } else if isLoading {
            RunningPane()
        } else {
            // Three ways to arrive here and one sentence for all of them: a
            // driver with no source to give, a read that failed — the banner
            // above says so — and a routine whose body the server keeps to
            // itself. None of the three is something the user can act on, and
            // guessing which it was would be worse than not saying.
            EmptyState(
                symbol: "doc.text",
                title: "No source",
                hint: "This connection did not return a definition for \(routine.name).")
        }
    }
}

/// One sequence. The number it is at, on the row, because that is the one thing
/// anybody opens a sequence to see and it fits.
struct SequenceRow: View {
    let sequence: SequenceInfo

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "number")
                .font(.system(size: 11))
                .foregroundStyle(Theme.Text.secondary.color)
                .frame(width: 14)

            Text(sequence.name)
                .font(Theme.Typography.body)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.xs)

            // Nothing where the server would not say, rather than a dash or a
            // zero. Zero is a value a sequence can be at.
            if let last = sequence.lastValue {
                Text(last)
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.Text.tertiary.color)
            }
        }
        .padding(.vertical, 1)
        .help(help)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibility)
    }

    private var help: String {
        var parts = ["Sequence", "by \(sequence.increment)", sequence.range]
        if sequence.cycles { parts.append("cycles") }
        return parts.joined(separator: " · ")
    }

    private var accessibility: String {
        guard let last = sequence.lastValue else { return "\(sequence.name), sequence" }
        return "\(sequence.name), sequence, at \(last)"
    }
}

/// The Structure tab for a sequence: six facts and nothing else.
///
/// A table of label and value rather than the routine pane's header-and-body,
/// because there is no body — a sequence is entirely its settings, and there is
/// no second call that could add anything to this.
struct SequenceStructureView: View {
    let sequence: SequenceInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(sequence.name)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
                .padding(.horizontal, Theme.Space.md)
                .padding(.vertical, Theme.Space.sm)
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)

            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                // First, because it is the one that changes and the one anybody
                // opened this to read.
                // Both reasons or neither. The server answers null for a
                // sequence nothing has drawn from and for one this login may
                // not read, and naming one of them would be right about half
                // the sequences anybody looks at.
                row(
                    "Current value",
                    sequence.lastValue ?? "not taken from yet, or not readable by this login")
                row("Increment", sequence.increment)
                row("Minimum", sequence.minValue)
                row("Maximum", sequence.maxValue)
                // Spelled out rather than a checkmark: "Cycle ✓" needs the
                // reader to know which way the tick points.
                row("At the end", sequence.cycles ? "starts over" : "stops")
                if let cache = sequence.cache {
                    row("Cache", "\(cache) per session")
                }
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.sm)

            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Theme.Surface.canvas.color)
    }

    private func row(_ label: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.md) {
            Text(label)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
                // A fixed column so the values line up; a sequence's fields are
                // read down the numbers, not across the labels.
                .frame(width: 110, alignment: .leading)
            Text(value)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(label), \(value)")
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
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)

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

            if model.isFindingInGrid {
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                GridFindBar(model: model, focus: $focus)
            }

            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            StatusBar(model: model)
        }
        .background(Theme.Surface.canvas.color)
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
    /// First because it describes the relation itself rather than a list of
    /// things attached to it. Offered only where the engine said something; see
    /// `AppModel.structureSections`.
    case info = "Info"
    case indexes = "Indexes"
    case foreignKeys = "Foreign keys"
    case referencedBy = "Referenced by"
    case constraints = "Constraints"
    case triggers = "Triggers"
    /// Offered only where the core can write one; see `AppModel.structureSections`.
    case ddl = "DDL"

    var id: String { rawValue }
}

extension View {
    /// The Structure pane's six tables, dressed alike.
    ///
    /// Striping off: AppKit paints the alternating background across the
    /// table's whole height, so the area past the last row renders as a stack
    /// of empty bars that read as rows the table failed to fill.
    ///
    /// The scroll view's own background hidden, for the reason `ValueViewer`
    /// gives about `TextEditor`: an AppKit control left alone draws the
    /// system's control colour, a neutral near-black, while every other
    /// surface in this window is the palette's blue-tinted `Surface.canvas`. A
    /// table in it reads as a panel borrowed from another application and
    /// dropped into the pane.
    ///
    /// And the surface named rather than inherited, because what is behind
    /// these two is a `VSplitView` rather than the pane, and a table that is
    /// merely transparent is a table whose colour is whatever the splitter
    /// decides.
    ///
    /// One modifier rather than the same line written six times, which is what
    /// it was — and five of those six carried no reason with them.
    fileprivate func structureTableSurface() -> some View {
        self
            .tableStyle(.inset(alternatesRowBackgrounds: false))
            .scrollContentBackground(.hidden)
            .background(Theme.Surface.canvas.color)
    }
}

struct StructurePane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?
    @State private var detail: StructureDetail = .indexes

    var body: some View {
        if let routine = model.selectedRoutine {
            // Ahead of every branch below, all of which describe a relation.
            // `selected` is still set underneath — the table this routine was
            // reached from — and asking about its columns first would draw that
            // table's structure under a function's name.
            RoutineStructureView(
                routine: routine, source: model.routineSource,
                isLoading: model.isLoadingRoutineSource)
        } else if let sequence = model.selectedSequence {
            SequenceStructureView(sequence: sequence)
        } else if model.columns.isEmpty {
            // Three states rather than one. The sentence below was shown for
            // all three, including the wait between picking a relation and its
            // columns arriving — a pane instructing the user to do the thing
            // they have just done. That window is a blink against the benchmark
            // server and is not a blink over a slow link.
            if model.isLoadingStructure {
                RunningPane()
            } else if model.selected == nil {
                EmptyState(
                    symbol: "list.bullet.rectangle",
                    title: "No structure to show",
                    hint: "Choose a table or view in the sidebar.")
            } else {
                // Reached when the read failed: the banner above says why, and
                // repeating "choose a table" here would blame the user for it.
                EmptyState(
                    symbol: "list.bullet.rectangle",
                    title: "No structure to show",
                    hint: "The columns of \(model.selected?.name ?? "this relation") did not load.")
            }
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
        // The same question the pane above asks, one read later. The sections
        // arrive after the columns, so the split opens with all of them empty
        // and "No indexes" is a statement about a table nobody has asked yet —
        // the one reading of an empty section that is not an answer. Asked once
        // here rather than in each case below, because the six come back
        // together.
        if model.isLoadingRelationDetail {
            RunningPane()
        } else {
            switch section {
            case .info:
                // Like DDL below: `section` only names this where there are
                // fields, so the fallback is for the switch's sake.
                table(model.tableInfo, empty: "Nothing else to report") { infoTable }
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
            case .ddl:
                // `section` only names this where there is a statement, so the
                // fallback is unreachable — it exists because the switch must be
                // total, not because a blank DDL section is a state.
                if let sql = model.ddl {
                    ddlText(sql)
                } else {
                    emptyLine("No DDL")
                }
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
            .foregroundStyle(Theme.Text.tertiary.color)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// The statements that would recreate the relation, verbatim.
    ///
    /// The one section that is prose rather than rows, so it gets a scroll view
    /// instead of a `Table`: the statement runs to tens of lines and the strip
    /// is a few hundred points tall. Scrolls both ways and does not wrap —
    /// re-flowing SQL destroys the indentation that the server put there to make
    /// it readable, and a wrapped line reads as part of the one below it.
    /// Selectable because the useful thing to do with a statement is paste it
    /// into the Query tab.
    private func ddlText(_ sql: String) -> some View {
        ScrollView([.vertical, .horizontal]) {
            Text(sql)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
                .fixedSize()
                .padding(.horizontal, Theme.Space.md)
                .padding(.vertical, Theme.Space.sm)
        }
        // A two-axis scroll view centres content smaller than its viewport, so
        // a short statement lands in the middle of the pane like a caption.
        // The anchor puts it where reading starts.
        .defaultScrollAnchor(.topLeading)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityLabel("Relation DDL")
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
                        .foregroundStyle(Theme.Semantic.warning.color)
                        .help("Primary key")
                        .accessibilityLabel("Primary key")
                }
            }
            .width(18)

            TableColumn("Column") { column in
                Text(column.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
            }

            TableColumn("Type") { column in
                Text(column.dataType)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
            }

            TableColumn("Null") { column in
                // Words, not a checkmark: "NO" is the constraint a reader
                // is scanning for, and a bare glyph makes them guess which
                // direction it means.
                Text(column.nullable ? "YES" : "NO")
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(
                        column.nullable
                            ? Theme.Text.tertiary.color : Theme.Text.primary.color)
            }
            .width(48)

            TableColumn("Default") { column in
                Text(column.defaultValue ?? "—")
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
            }
        }
        .structureTableSurface()
        // The three column changes, on the rows they act on. A right-click on
        // nothing still offers Add: a column that does not exist yet has no row
        // to click, which is the same reason New Database lives in a menu.
        .contextMenu(forSelectionType: ColumnInfo.ID.self) { positions in
            columnMenu(for: positions)
        }
        // A focus target so this pane has somewhere for focus to be.
        // Clearing focus is not enough — SwiftUI then falls back to the
        // only text field on screen, which is the sidebar's filter, and the
        // tab opens with a ring on a control in a different pane.
        .focusable()
        .focused($focus, equals: .structureTable)
    }

    /// Add, and — on a row — rename and drop.
    ///
    /// Nothing is drawn where the core writes no statement for this database,
    /// rather than drawn and refusing whichever is clicked: the rule the
    /// navigator's own row menu follows, and the reason `changesColumns` is a
    /// capability rather than something worked out per click.
    @ViewBuilder
    private func columnMenu(for positions: Set<ColumnInfo.ID>) -> some View {
        if model.changesColumns, let relation = model.selected {
            Button(ColumnChange.add(NewTableColumn()).menuTitle) {
                model.prepareColumnChange(.add(NewTableColumn()), of: relation)
            }
            // One row only. Two of these statements name a single column and the
            // third carries one, so a menu acting on several would be several
            // statements — and the sheet shows one.
            if positions.count == 1,
                let column = model.columns.first(where: { positions.contains($0.id) })
            {
                Divider()
                Button(ColumnChange.rename(name: column.name, to: column.name).menuTitle) {
                    // Opens with the name it already has, so the field says what
                    // is being changed from and the button waits for a different
                    // one. The rule the relation rename sheet follows.
                    model.prepareColumnChange(
                        .rename(name: column.name, to: column.name), of: relation)
                }
                Button(ColumnChange.drop(name: column.name).menuTitle) {
                    model.prepareColumnChange(.drop(name: column.name), of: relation)
                }
            }
        }
        // Its own capability and its own `if`, rather than a fourth item inside
        // the block above: SQLite adds, drops and renames a column and alters
        // none, so on SQLite the first three are drawn and this one is not.
        if model.altersColumns, let relation = model.selected, positions.count == 1,
            let column = model.columns.first(where: { positions.contains($0.id) })
        {
            Divider()
            Button(ColumnChange.alter(ColumnAlteration(column)).menuTitle) {
                model.prepareColumnChange(.alter(ColumnAlteration(column)), of: relation)
            }
        }
    }

    /// The engine's own description of the relation, label beside value.
    ///
    /// A table like the five sections beside it rather than a form, so switching
    /// between them does not change the shape of the pane. The value is
    /// selectable and the label is not: a size or an owner is a thing to paste
    /// somewhere, while "Owner" is a word this window wrote.
    private var infoTable: some View {
        Table(model.tableInfo) {
            TableColumn("Field") { field in
                Text(field.label)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.secondary.color)
            }
            // Fixed, unlike the other sections' columns. Two columns share the
            // pane's whole width, so a flexible label column takes half of it
            // and puts every value a third of the way across the window from
            // the word it belongs to; a range wide enough to stop that left the
            // longest label truncated instead.
            .width(140)

            TableColumn("Value") { field in
                Text(field.value)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .textSelection(.enabled)
            }
        }
        .structureTableSurface()
    }

    private var indexesTable: some View {
        Table(model.indexes) {
            TableColumn("") { index in
                if index.isPrimary {
                    Image(systemName: "key.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(Theme.Semantic.warning.color)
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
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
                    .help(index.name)
            }

            TableColumn("Keys") { index in
                // The predicate is part of what the index covers, so it rides
                // with the keys rather than being dropped: a partial index
                // shown as a plain one claims coverage it lacks.
                Text(Self.keyLabel(index))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
                    .help(Self.keyLabel(index))
            }

            TableColumn("Kind") { index in
                Text(index.kindLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
            }
            .width(min: 70, ideal: 96)
        }
        .contextMenu(forSelectionType: IndexInfo.ID.self) { indexMenu(for: $0) }
        .structureTableSurface()
    }

    /// The indexes table's row menu, drawn only where the core writes these
    /// statements — the rule the column table's menu follows above.
    @ViewBuilder
    private func indexMenu(for names: Set<IndexInfo.ID>) -> some View {
        if model.changesIndexes, let relation = model.selected {
            Button(IndexChange.create(NewIndex()).menuTitle) {
                model.prepareIndexChange(.create(NewIndex()), of: relation)
            }
            // One row only: the statement names one index, and the sheet shows
            // one statement.
            if names.count == 1, let index = model.indexes.first(where: { names.contains($0.id) }) {
                Divider()
                Button(IndexChange.drop(name: index.name).menuTitle) {
                    model.prepareIndexChange(.drop(name: index.name), of: relation)
                }
                // An index the primary key is made of goes with the constraint
                // and not on its own. Every one of these servers refuses it, so
                // the item is drawn shut rather than drawn and refused — the row
                // already carries the key icon that says why.
                .disabled(index.isPrimary)
            }
        }
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
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
            }

            TableColumn("References") { key in
                Text(key.otherLabel(sameSchemaAs: model.selected?.schema ?? ""))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }

            TableColumn("On") { key in
                Text(key.actionLabel.isEmpty ? "—" : key.actionLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
            }
            .width(min: 80, ideal: 150)
        }
        .structureTableSurface()
    }

    private var referencedByTable: some View {
        Table(model.referencedBy) {
            // The referencing table leads, because the question this section
            // answers is "who depends on me", not "through which of my
            // columns".
            TableColumn("From") { key in
                Text(key.otherLabel(sameSchemaAs: model.selected?.schema ?? ""))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
            }

            TableColumn("To columns") { key in
                Text(key.localColumns.joined(separator: ", "))
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }

            TableColumn("On") { key in
                // ON DELETE CASCADE on an inbound key is the one that decides
                // what happens to other people's rows when you delete yours.
                Text(key.actionLabel.isEmpty ? "—" : key.actionLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(
                        key.onDelete == "CASCADE"
                            ? Theme.Semantic.warning.color : Theme.Text.tertiary.color
                    )
                    .lineLimit(1)
            }
            .width(min: 80, ideal: 150)
        }
        .structureTableSurface()
    }

    private var constraintsTable: some View {
        Table(model.constraints) {
            TableColumn("Kind") { constraint in
                Text(constraint.kind.label)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.Text.tertiary.color)
            }
            .width(min: 56, ideal: 66)

            TableColumn("Name") { constraint in
                Text(constraint.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.primary.color)
                    .lineLimit(1)
                    .help(constraint.name)
            }

            TableColumn("Definition") { constraint in
                Text(constraint.definition)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
                    .help(constraint.definition)
            }
        }
        .structureTableSurface()
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
                        .foregroundStyle(Theme.Text.tertiary.color)
                        .help("Disabled")
                        .accessibilityLabel("Disabled")
                }
            }
            .width(18)

            TableColumn("Name") { trigger in
                Text(trigger.name)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(
                        trigger.enabled ? Theme.Text.primary.color : Theme.Text.tertiary.color
                    )
                    .lineLimit(1)
                    .help(trigger.name)
            }

            TableColumn("When") { trigger in
                Text(trigger.whenLabel)
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }

            TableColumn("Runs") { trigger in
                Text(trigger.runsLabel)
                    .font(Theme.Typography.mono)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
                    .help(trigger.definition ?? trigger.runsLabel)
            }
        }
        .structureTableSurface()
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
                                        ? Theme.Text.secondary.color : Theme.Text.tertiary.color)
                        }
                    }
                    .padding(.horizontal, Theme.Space.sm)
                    .frame(height: 20)
                    .background(
                        RoundedRectangle(cornerRadius: Theme.Radius.control, style: .continuous)
                            .fill(
                                selected == section
                                    ? Theme.Surface.overlay.color : Color.clear)
                    )
                    .foregroundStyle(
                        selected == section ? Theme.Text.primary.color : Theme.Text.secondary.color)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(count.map { "\(section.rawValue), \($0)" } ?? section.rawValue)
                // Which section is open is carried only by a fill, so without
                // this the strip reads to VoiceOver as six identical buttons
                // and there is no way to hear which one you are looking at.
                // The tab bar above and the outcome list below both say it.
                .accessibilityAddTraits(selected == section ? [.isSelected] : [])
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, Theme.Space.sm)
        .frame(height: 26)
        .background(Theme.Surface.raised.color)
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
        }
    }
}

struct ContentPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(spacing: 0) {
            if let routine = model.selectedRoutine {
                // Reached by switching here deliberately: picking a routine while
                // this tab is showing moves to Structure instead. The rows of the
                // table underneath are still loaded and one click away, which is
                // what the hint is pointing at.
                EmptyState(
                    symbol: routine.kind.symbol,
                    title: "\(routine.kind.label)s have no rows",
                    hint: "\(routine.signature) is code, not a table. "
                        + "Its source is on the Structure tab.")
            } else if let sequence = model.selectedSequence {
                EmptyState(
                    symbol: "number",
                    title: "Sequences have no rows",
                    hint: "\(sequence.name) is a counter, not a table. "
                        + "What it is set to do is on the Structure tab.")
            } else if model.selected == nil {
                EmptyState(
                    symbol: "tablecells",
                    title: "Nothing selected",
                    hint: "Choose a table in the sidebar to browse its rows.")
            } else if model.isRecordViewOpen, model.canShowRecord {
                // The grid goes, rather than sharing the height with it. A
                // sixty-column row is unreadable across, which is the whole
                // reason this view exists; keeping both would leave neither
                // enough of the pane to be read in.
                RecordPane(model: model)
            } else {
                MetalGridView(
                    table: model.browseResult.table,
                    generation: model.browseResult.generation,
                    rowCount: model.browseResult.rowCount,
                    declaredTypes: model.declaredColumnTypes,
                    hidden: model.hiddenBrowseColumns,
                    selection: $model.browseSelection,
                    claimsInitialFocus: true,
                    sort: model.gridSort,
                    pending: model.pendingCells,
                    deleted: model.deletedRows,
                    drafts: model.draftRows,
                    onSortColumn: { model.toggleSort(column: $0) },
                    onFilter: { model.filterByCell($0) },
                    onCopyAsInsert: { model.copyRowsAsInsert($0) },
                    jumpsAtCell: { model.jumps(atColumn: $0, in: $1) },
                    onJump: { model.jump($0) },
                    // Nil where nothing can be written, which is what keeps the
                    // item off a view and off a table with no key: `editObstacle`
                    // is the same sentence `CellEditorRow` shows under the grid.
                    onEditValue: model.editObstacle == nil ? { model.editSelectedValue() } : nil,
                    onStageEdit: { model.stageEdit($0) },
                    editSeed: { model.inlineEditSeed },
                    onFindAction: { model.takeFindAction($0) },
                    hasFindText: !model.gridFindText.isEmpty,
                    revealCount: model.browseResult.revealCount
                )
                .overlay { LoadingVeil(isVisible: model.browseResult.isVeiled) }

                CellInspector(cell: model.inspectedCell(in: model.browseResult), editing: model)
            }

            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            FilterBar(model: model, focus: $focus)
        }
    }
}

/// One row read down the page instead of across it.
///
/// A table wide enough to need this is a table where the grid has stopped
/// working: sixty columns is a horizontal scroll for every value after the
/// fourth, and comparing two of them means scrolling between them. Listed down
/// the page they are all on screen at once, which is the whole trade — one row
/// instead of many.
///
/// It has no cursor of its own. Picking a field moves the grid's, so the strip
/// underneath describes the field that was picked and the value box writes to
/// it: this is a second way to draw the selection, not a second selection. That
/// is also why the pane keeps the `CellInspector` below it rather than replacing
/// it — a record view you cannot edit from would be a worse grid.
struct RecordPane: View {
    @Bindable var model: AppModel

    /// The name column's width. Fixed rather than sized to the longest name:
    /// the values are what is being read, and a column that resized itself per
    /// table would move every value sideways on each selection.
    private static let nameWidth: CGFloat = 168
    private static let rowHeight: CGFloat = 22

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(model.recordFields) { field in
                        Button {
                            model.focusRecordField(field.column)
                        } label: {
                            row(field)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Theme.Surface.canvas.color)
        .overlay { LoadingVeil(isVisible: model.browseResult.isVeiled) }
        // The arrow keys move between rows, which is what the grid they replaced
        // did with them. `focusable` is what makes them arrive at all: without it
        // the pane is a stack of buttons and the key goes to whichever one was
        // clicked last.
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.upArrow) {
            model.stepRecord(by: -1)
            return .handled
        }
        .onKeyPress(.downArrow) {
            model.stepRecord(by: 1)
            return .handled
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Record view")
    }

    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text("Record")
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.secondary.color)

            if let position = model.recordPosition {
                // Counted from one and against the total, because the question
                // this answers is "where am I", and a bare row number answers
                // half of it.
                Text(
                    "\(AppModel.formatted(position.row)) of \(AppModel.formatted(position.of))"
                )
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
            }

            Spacer(minLength: Theme.Space.sm)

            Button {
                model.stepRecord(by: -1)
            } label: {
                Image(systemName: "chevron.up")
                    .font(.system(size: 9, weight: .semibold))
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.Text.secondary.color)
            .help("Previous row (↑)")
            .accessibilityLabel("Previous row")

            Button {
                model.stepRecord(by: 1)
            } label: {
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.Text.secondary.color)
            .help("Next row (↓)")
            .accessibilityLabel("Next row")

            Button {
                model.isRecordViewOpen = false
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .bold))
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.Text.secondary.color)
            .help("Back to the grid (⌃⌘R)")
            .accessibilityLabel("Back to the grid")
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.Surface.overlay.color)
    }

    private func row(_ field: RecordField) -> some View {
        let focused = model.browseSelection?.column == field.column
        return HStack(alignment: .top, spacing: Theme.Space.sm) {
            VStack(alignment: .leading, spacing: 0) {
                Text(field.name)
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
                    .truncationMode(.tail)
                if !field.type.isEmpty {
                    Text(field.type)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.Text.tertiary.color)
                        .lineLimit(1)
                }
            }
            .frame(width: Self.nameWidth, alignment: .leading)

            // NULL keeps the tertiary tone the strip gives it, so the word is
            // visibly not a value somebody typed.
            Text(field.value)
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(
                    field.isNull ? Theme.Text.tertiary.color : Theme.Text.primary.color
                )
                .lineLimit(1)
                .truncationMode(.tail)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(minHeight: Self.rowHeight, alignment: .leading)
        .background(focused ? Theme.Surface.overlay.color : Color.clear)
        .overlay(alignment: .leading) {
            // The same 2pt rule the grid draws down a selected row, so which
            // field the strip below is describing is answerable at a glance.
            Rectangle()
                .fill(focused ? Theme.Accent.selection.color : Color.clear)
                .frame(width: 2)
        }
        .contentShape(Rectangle())
        // One line each, however long the value is. The strip and the value
        // viewer under the pane are where a long one is read in full.
        .help(field.value)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(field.name), \(field.value)")
    }
}

/// Where a cell is changed, and where the changes are sent or thrown away.
///
/// A field under the grid rather than typing into the cell itself. Inline
/// editing in a Metal-drawn grid means a text view floating over a scrolling
/// surface and staying attached to a moving row, and none of that difficulty is
/// about the database. This row buys the whole of the feature — see the value,
/// change it, see that it is pending, send it — and leaves the cell to be typed
/// into by a later version that has earned the complication.
///
/// The two buttons that end it sit here as well, and only appear with something
/// to act on: a Save that is dimmed for the whole of a session is a control
/// nobody reads twice.
///
/// The cell is optional because Add Row is here too, and the moment it is most
/// wanted — a table with nothing in it yet — is the moment there is no cell to
/// be selected.
private struct CellEditorRow: View {
    @Bindable var model: AppModel
    let cell: AppModel.InspectedCell?
    @State private var typed = ""
    /// Owned here rather than threaded down from the pane, unlike every other
    /// field in this window. Nothing outside this row ever hands focus to the
    /// value field — there is no menu item and no shortcut that puts the caret
    /// in it — so a binding passed through two views would exist only to be
    /// read back by the field that wrote it.
    @FocusState private var focus: FocusArea?

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            if let obstacle = model.editObstacle {
                // Said rather than hidden. An absent field reads as a feature
                // this build does not have, and one of the two reasons is
                // something the user can do something about.
                // Secondary rather than the tertiary label tone: this is a
                // sentence explaining why the controls beside it are missing,
                // which is something the reader has to read rather than glance
                // at — and tertiary does not clear 4.5:1 on this bar.
                Text(obstacle)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            } else {
                if let cell {
                    // `FieldLabel`'s own text, without its uppercasing. That
                    // caption style is for the words this window chose —
                    // Custom, Order by, Filters — and a column name is not one
                    // of them: on the engines here `sku` and `SKU` are two
                    // identifiers, and a bar that prints the second while the
                    // grid header above it prints the first is naming a column
                    // that may not exist.
                    Text(cell.column)
                        .font(Theme.Typography.micro.weight(.semibold))
                        .foregroundStyle(Theme.Text.tertiary.color)
                        .accessibilityHidden(true)
                    // The window's own field rather than a bare `TextField`.
                    // This is the one control in the application that writes to
                    // the database and it was drawn with no border, no fill and
                    // no focus ring — indistinguishable from the static label
                    // beside it, and from the value the inspector strip prints
                    // directly above. The WHERE box two rows below it, which
                    // only builds a query, looked more like something you could
                    // type into than this did.
                    CompactField(
                        placeholder: "", text: $typed, area: .cellValue, focus: $focus,
                        onSubmit: { model.stageEdit(typed) }
                    )
                    .help("Return stages the change; nothing is sent until Save")
                    .accessibilityLabel("Value of \(cell.column)")
                    Button("Set") { model.stageEdit(typed) }
                        .help("Hold this value for the selected cell")
                    Button("NULL") { model.stageEdit(nil) }
                        .help("Hold NULL for the selected cell, which is not an empty string")
                }
                if let title = model.deleteRowsTitle {
                    Button(title) { model.toggleDeleteSelectedRows() }
                        .disabled(model.isBusy)
                        .help("Mark the selected rows to be deleted when Save is pressed")
                }
                if model.canAddRow {
                    Button("Add Row") { model.addDraftRow() }
                        .help(
                            "Add a row after the last one; columns left alone take the "
                                + "table's defaults")
                }
                if model.canDuplicateRow {
                    Button("Duplicate Row") { model.duplicateSelectedRow() }
                        .help(
                            "Add a copy of the selected row; key columns are left to the table's "
                                + "defaults")
                }
            }

            Spacer()

            if model.hasPendingEdits {
                Text(AppModel.pluralized(model.staged.count, "change"))
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Semantic.warning.color)
                Button("Revert") { model.revertEdits() }
                    .help("Throw the pending changes away; the rows on screen are unchanged")
                Button("Save") { model.applyEdits() }
                    .keyboardShortcut("s", modifiers: .command)
                    .disabled(model.isBusy)
                    .help("Send the changes and read the rows back (⌘S)")
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs)
        .background(Theme.Surface.raised.color)
        // Seeded from whichever cell is selected, and re-seeded when that moves:
        // a field still holding the last cell's text is one keystroke away from
        // writing it into this one.
        .onChange(of: identity) { typed = seed }
        .task(id: identity) { typed = seed }
    }

    /// Which cell the field is showing, as one string to watch.
    private var identity: String {
        cell.map { $0.address + "\u{1}" + $0.column } ?? ""
    }

    /// A NULL seeds an empty field rather than the word: the field's contents
    /// are what would be written, and "NULL" typed into a text column is four
    /// characters. A draft column nobody has filled in seeds an empty field for
    /// the same reason — DEFAULT is what it will do, not what it holds.
    private var seed: String {
        guard let cell, !cell.isNull else { return "" }
        return cell.value
    }
}

/// WHERE and ORDER BY, the two filters a browse pane actually needs. They build
/// a query rather than filtering in memory, so they work on results larger than
/// what has been fetched.
struct FilterBar: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            HStack(spacing: Theme.Space.sm) {
                let hint = model.filterHint

                // Absent rather than disabled where the core named no columns:
                // a database this build writes no statements for, or a relation
                // whose columns have not arrived. A disclosure that opened onto
                // an empty list would be an offer that is not one.
                if !model.filterColumns.isEmpty {
                    FilterRowsToggle(model: model)
                }

                FieldLabel(text: "Custom")
                // Relabelled from "Where" because it is no longer the only way
                // to filter — it is the escape hatch, and the rows are the way
                // in. While there are rows it is theirs: greyed, and showing the
                // WHERE they compiled to, so that what is running can be read
                // without a second control to read it in.
                CompactField(
                    placeholder: model.isCustomFilterEditable ? hint.where : mirrored,
                    text: $model.whereClause,
                    area: .whereField, focus: $focus, onSubmit: model.applyFilters
                )
                .disabled(!model.isCustomFilterEditable)
                .help(
                    model.isCustomFilterEditable
                        ? "" : "Remove the filter rows to write this by hand")

                FieldLabel(text: "Order by")
                CompactField(
                    placeholder: hint.order, text: $model.orderClause,
                    area: .orderField, focus: $focus, onSubmit: model.applyFilters
                )
                .frame(maxWidth: 190)

                Button("Apply") { model.applyFilters() }
                    .controlSize(.small)
                    .disabled(model.selected == nil || model.isBusy)
                    // No `.keyboardShortcut(.return)`. The ↩ in the help text is
                    // the one `CompactField` sends through `onSubmit`, from the
                    // fields on this bar. A window-level binding would take it
                    // from the cell editor a few points below, where ↩ means
                    // commit this value and pressing it would instead re-run the
                    // browse the edit has not been staged into yet.
                    .help("Re-run the browse query with these filters (↩)")
            }
            if model.isFilterRowsOpen, !model.filterColumns.isEmpty {
                FilterRows(model: model, focus: $focus)
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
        .background(Theme.Surface.raised.color)
    }

    /// What the Custom field shows while the rows own it.
    ///
    /// Before the first Apply there is no clause yet, and saying so is better
    /// than an empty box that reads as an unfiltered browse — the rows are on
    /// screen but nothing has been sent for them.
    private var mirrored: String {
        model.compiledClause.isEmpty ? "the rows below, once applied" : model.compiledClause
    }
}

/// The control that opens the filter rows, and says how many are running.
///
/// The count is drawn whether the list is open or shut, rather than only when
/// shut. Open, it is the one place the number appears at all — the rows
/// themselves have to be counted — and a badge that vanished on the way down
/// would read as the filter being let go of.
///
/// The count is not decoration. These rows compile into the browse's WHERE, so a
/// closed disclosure would otherwise be a filter with nothing on screen saying
/// it is there — the failure the greyed Custom field also guards against, from
/// the other side.
struct FilterRowsToggle: View {
    @Bindable var model: AppModel

    var body: some View {
        Button {
            model.isFilterRowsOpen.toggle()
        } label: {
            HStack(spacing: Theme.Space.xs) {
                Image(systemName: model.isFilterRowsOpen ? "chevron.down" : "chevron.right")
                    .font(Theme.Typography.micro)
                FieldLabel(text: "Filters")
                if !model.filterRules.isEmpty {
                    Text("\(model.filterRules.count)")
                        .font(Theme.Typography.micro.weight(.semibold))
                        .foregroundStyle(Theme.Surface.raised.color)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Theme.Accent.selection.color))
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Theme.Text.tertiary.color)
        .help("Build the filter out of this table's columns")
        .accessibilityLabel(
            model.filterRules.isEmpty ? "Filters" : "Filters, \(model.filterRules.count) active")
    }
}

/// The list itself: a row per rule, and the button that adds one.
struct FilterRows: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            // Identified by position rather than by the rule. Two rows asking
            // the same thing of the same column are a state somebody can reach
            // by pressing Add twice, and identity by value would collapse them
            // into one row that then edits both.
            ForEach(Array(model.filterRules.enumerated()), id: \.offset) { index, rule in
                FilterRuleRow(model: model, index: index, rule: rule, focus: $focus)
            }
            Button {
                if let rule = model.newFilterRule { model.addFilterRule(rule) }
            } label: {
                Label("Add Filter", systemImage: "plus")
            }
            .controlSize(.small)
            .disabled(model.newFilterRule == nil)
            .help("Add a row to the filter")
        }
    }
}

/// One row: a column, an operator, and what to compare against.
///
/// Reads its rule out of the model by index and writes every change back through
/// `updateFilterRule` rather than binding into the array. That is what keeps
/// `FilterRule.settled` between each popup and the stored row — moving a row to
/// a column that cannot answer its operator has to correct the operator, and a
/// binding straight into the array would go around the correction.
struct FilterRuleRow: View {
    @Bindable var model: AppModel
    let index: Int
    let rule: FilterRule
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Picker("", selection: binding(\.column)) {
                ForEach(model.filterColumns) { column in
                    Text(column.name).tag(column.name)
                }
            }
            .labelsHidden()
            .frame(width: 150)
            .accessibilityLabel("Filter column")

            Picker("", selection: binding(\.op)) {
                ForEach(operators, id: \.self) { op in
                    Text(op.label).tag(op)
                }
            }
            .labelsHidden()
            .frame(width: 130)
            .accessibilityLabel("Filter operator")

            // No field at all for the operators that compare against nothing,
            // rather than a disabled one. There is nothing to type, and an empty
            // box beside IS NULL reads as a value somebody forgot to fill in.
            if rule.op != .isNull, rule.op != .isNotNull {
                CompactField(
                    placeholder: "value", text: text(\.value),
                    area: .filterValue(index), focus: $focus, onSubmit: model.applyFilters
                )
                .frame(maxWidth: 180)
                .accessibilityLabel("Filter value")
            }
            if rule.op == .between {
                FieldLabel(text: "and")
                CompactField(
                    placeholder: "value", text: text(\.second),
                    area: .filterSecond(index), focus: $focus, onSubmit: model.applyFilters
                )
                .frame(maxWidth: 180)
                .accessibilityLabel("Filter range end")
            }

            Button {
                model.removeFilterRule(at: index)
            } label: {
                Image(systemName: "minus.circle")
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.Text.tertiary.color)
            .help("Remove this row")
            .accessibilityLabel("Remove filter row")

            Spacer(minLength: 0)
        }
    }

    /// The operators this row's column can answer.
    ///
    /// A column the relation does not have — a restored filter after the table
    /// changed underneath it — offers only what the row already says. The popup
    /// has to be able to draw the row's own operator or the row would appear on
    /// screen saying something other than what it will send.
    private var operators: [FilterOperator] {
        model.filterColumns.first { $0.name == rule.column }?.operators ?? [rule.op]
    }

    /// A binding onto one field of the row, routed back through the model.
    private func binding<T>(_ key: WritableKeyPath<FilterRule, T>) -> Binding<T> {
        Binding(
            get: { rule[keyPath: key] },
            set: { new in
                var edited = rule
                edited[keyPath: key] = new
                model.updateFilterRule(at: index, to: edited)
            })
    }

    /// The same, for the two optional text fields.
    ///
    /// A row holds `nil` for "nothing to compare against" and a field holds "",
    /// so the two are mapped onto each other here: emptying the box is the same
    /// as never having filled it, and the core refuses both by name rather than
    /// comparing against an empty string.
    private func text(_ key: WritableKeyPath<FilterRule, String?>) -> Binding<String> {
        Binding(
            get: { rule[keyPath: key] ?? "" },
            set: { new in
                var edited = rule
                edited[keyPath: key] = new.isEmpty ? nil : new
                model.updateFilterRule(at: index, to: edited)
            })
    }
}

struct QueryPane: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        @Bindable var result = model.queryResult
        // The buffer strip is outside the split: it names which text the editor
        // is showing, so dragging the editor's height must not be able to hide
        // it or to give it room it has no use for.
        return VStack(spacing: 0) {
            QueryBufferBar(model: model)
            VSplitView {
                ZStack(alignment: .bottomTrailing) {
                    // The selection binding is what makes ⌘R mean "this statement":
                    // without it the pane knows the text and not where in it the
                    // user is standing.
                    // The scheme goes with them: it is how the core knows which
                    // database's rules to read the buffer by, and reading MySQL as
                    // PostgreSQL mis-colours it and splits it in the wrong places.
                    // The offers come through a closure for the same reason the
                    // scheme comes through a string: the editor is handed what it
                    // needs to do its job, not the connection it is being done
                    // against.
                    SQLEditor(
                        text: $model.queryText, selection: $model.querySelection,
                        scheme: model.scheme,
                        fontSize: model.preferences.editorFontSize,
                        typing: model.preferences.editorTyping,
                        theme: model.preferences.editorTheme,
                        offers: { text, caret, then in
                            model.completions(in: text, caret: caret, then: then)
                        }
                    )
                    .padding(.horizontal, Theme.Space.md)
                    .padding(.vertical, Theme.Space.sm)
                    // The theme's background, not the window's: this is the
                    // margin around the text view, and a themed editor inside
                    // a strip of the old colour would name the seam.
                    .background(model.preferences.editorTheme.background.color)
                    .focused($focus, equals: .editor)
                    .accessibilityLabel("SQL editor")

                    HStack(spacing: Theme.Space.sm) {
                        // Says which statement is about to run, before it runs. A
                        // buffer of five makes ⌘R a guess otherwise, and the wrong
                        // guess is a statement the user did not mean to execute.
                        Text(model.runTarget?.hint ?? "nothing to run")
                            .font(Theme.Typography.micro)
                            .foregroundStyle(Theme.Text.tertiary.color)
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
                            model.isHistoryOpen
                                ? Theme.Accent.selection.color : Theme.Text.secondary.color
                        )
                        .help("Statements this window has run, and the ones you kept (⇧⌘H)")
                        .accessibilityLabel("Query panel")
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
                        QueryPanel(model: model)
                        Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                    }

                    // Only for a run of several. A ⌘R over one statement has one
                    // outcome and the grid is already showing it; a list of one
                    // would be chrome charged to the common case to describe the
                    // rare one.
                    if model.scriptSteps.count > 1 {
                        ScriptOutcomeList(model: model)
                        Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                    }

                    // Until this pane has run something there is nothing to show.
                    // It used to fall back to the browse's grid, which put rows
                    // under a statement that had not produced them.
                    if let step = model.selectedScriptStep {
                        // A plan is drawn instead of the grid, not beside it. The
                        // rows it was read from are one document in one cell or
                        // four columns of ids — the switch is there for checking
                        // the tree against them, which is a thing somebody does
                        // once, not a second pane to keep on screen.
                        if step.plan != nil {
                            PlanSwitch(model: model)
                            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                        }
                        if let plan = step.plan, model.showsPlanTree {
                            PlanTree(plan: plan)
                        } else if step.outcome.hasGrid {
                            MetalGridView(
                                table: model.queryResult.table,
                                generation: model.queryResult.generation,
                                rowCount: model.queryResult.rowCount,
                                selection: $result.selection,
                                name: "Query result grid",
                                onFindAction: { model.takeFindAction($0) },
                                hasFindText: !model.gridFindText.isEmpty,
                                revealCount: model.queryResult.revealCount
                            )
                            .overlay { LoadingVeil(isVisible: model.queryResult.isLoading) }

                            CellInspector(cell: model.inspectedCell(in: model.queryResult))
                        } else if model.queryResult.isLoading {
                            RunningPane()
                        } else {
                            // A statement that returned no rows still has an answer,
                            // and an empty grid with no columns is not it — that
                            // reads as a query that broke rather than as an UPDATE
                            // that worked.
                            StatementNote(step: step)
                        }
                    } else if model.queryResult.isLoading {
                        RunningPane()
                    } else {
                        EmptyState(
                            symbol: "terminal",
                            title: "No results yet",
                            hint: "Press ⌘R to run the statement above, ⌥⌘R for all of them."
                        )
                    }
                }
                .frame(minHeight: 160)
            }
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
private struct QueryPanel: View {
    @Bindable var model: AppModel
    /// Set by the Clear button, cleared by either answer. Clearing is
    /// irreversible, so it is asked in the panel's own header rather than
    /// through an alert: a modal would take the window away from the thing it
    /// is about, which is the objection `InlineBanner` already carries.
    @State private var confirmingClear = false
    /// Set by Save Query, cleared by either answer. In the header for the same
    /// reason the Clear confirmation is, and with one more of its own: the
    /// statement being kept is printed beside the field, so the name is typed
    /// against something on screen rather than against something remembered.
    @State private var naming = false
    @State private var typedName = ""
    @State private var hovered: UUID?

    /// Seven rows before it scrolls, two more than the outcome list gets: this
    /// is a list someone is searching, where that one is a run being read.
    private static let rowHeight: CGFloat = 22
    private static let maxRows = 7

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            switch model.queryPanelTab {
            case .history:
                if model.history.entries.isEmpty {
                    // Says what fills the list rather than that it is empty. This
                    // is where someone who found the panel before they needed it
                    // is standing.
                    note("Nothing has run yet — ⌘R sends the statement the caret is in.")
                } else if model.shownHistory.isEmpty {
                    // A different sentence, because a different thing is true.
                    // Statements have run and something here is hiding them, so
                    // each of the two narrowings says how to undo itself —
                    // "nothing has run yet" over a store of two hundred would be
                    // the panel lying about itself.
                    note(
                        model.historyFilter.isEmpty
                            ? "Nothing typed is left — turn on All to see what the window sent."
                            : "No statement here matches that.")
                } else {
                    list
                }
            case .favorites:
                if model.offeredFavorites.isEmpty {
                    note("Nothing kept yet — Save Query keeps the statement ⌘R would send.")
                } else {
                    favoritesList
                }
            }
        }
        // `Surface.canvas`, not `Surface.raised`, and that is the seam rather
        // than a preference. `Grid.header` is also the raised tone, so the
        // panel's last row and the result's column headers met as one continuous
        // field: the 1pt `Border.hairline` between them is white at 0.08 alpha
        // over two identical mid tones, which at that size is below anything an
        // eye resolves. Nothing said where the list of statements ended and the
        // rows began. Taking the body down a step also puts the panel behind its
        // own `Surface.overlay` header, which is the direction that pair is drawn
        // everywhere else in this window.
        .background(Theme.Surface.canvas.color)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            model.queryPanelTab == .history ? "Query history" : "Saved queries")
    }

    /// What an empty list says. Both of them name what would fill it, because
    /// somebody reading either one has found the panel before they needed it.
    private func note(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.tertiary.color)
            .frame(maxWidth: .infinity)
            .frame(height: Self.rowHeight * 2)
    }

    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            if confirmingClear {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.Semantic.dangerText.color)
                Text(
                    "Delete all \(AppModel.pluralized(model.history.entries.count, "statement"))? "
                        + "This cannot be undone."
                )
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.primary.color)

                Spacer(minLength: Theme.Space.sm)

                Button("Cancel") { confirmingClear = false }
                    .controlSize(.small)
                Button("Delete") {
                    model.history.clear()
                    confirmingClear = false
                }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
                .tint(Theme.Semantic.danger.color)
            } else if naming {
                Text("Save as")
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.Text.secondary.color)

                TextField("", text: $typedName)
                    .textFieldStyle(.roundedBorder)
                    .controlSize(.small)
                    .frame(width: 160)
                    .onSubmit { keep() }
                    .accessibilityLabel("Name for this query")

                // The statement about to be filed, so that the name is typed
                // against something on screen. Without it this is a text field
                // asking you to label something you have to remember.
                Text(model.savedQuery ?? "")
                    .font(Theme.Typography.monoSmall)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)

                Button("Cancel") {
                    naming = false
                    typedName = ""
                }
                .controlSize(.small)
                Button("Save") { keep() }
                    .controlSize(.small)
                    .buttonStyle(.borderedProminent)
                    // The window's accent rather than the system's, which is
                    // whatever the person picked in System Settings and is not
                    // in this palette. The Stage button in the value editor
                    // carries the same pair.
                    .tint(Theme.Accent.selection.color)
                    .disabled(typedName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            } else {
                // A segmented control rather than two buttons: these are two
                // readings of one panel, and exactly one of them is showing.
                Picker("", selection: $model.queryPanelTab) {
                    ForEach(AppModel.QueryPanelTab.allCases) { tab in
                        Text(tab.title).tag(tab)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .controlSize(.small)
                .frame(width: 156)
                .accessibilityLabel("Which list to show")

                // The count of what is drawn, not of what is stored. A header
                // promising two hundred statements over a list showing four
                // would be describing the store rather than the panel.
                Text(
                    model.queryPanelTab == .history
                        ? AppModel.pluralized(model.shownHistory.count, "statement")
                        : AppModel.pluralized(model.offeredFavorites.count, "query")
                )
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)

                if model.queryPanelTab == .history {
                    // The plain field the Save-as row above uses, rather than
                    // the `CompactField` the filter bar uses. That one draws its
                    // focus ring from a `FocusArea` and this panel is handed no
                    // focus binding — and matching the header it sits in beats
                    // matching a bar on another tab.
                    TextField("Filter", text: $model.historyFilter)
                        .textFieldStyle(.roundedBorder)
                        .controlSize(.small)
                        .frame(width: 150)
                        .accessibilityLabel("Filter the statements")

                    Toggle("All", isOn: $model.showsAllStatements)
                        .toggleStyle(.checkbox)
                        .controlSize(.small)
                        .font(Theme.Typography.caption)
                        .help("Show the browses and the edits, not only what was typed")
                        .accessibilityLabel("Show all statements")
                }

                Spacer(minLength: Theme.Space.sm)

                // Against the store rather than against what is drawn, because
                // that is what it deletes. Clear is not "clear these four".
                if model.queryPanelTab == .history, !model.history.entries.isEmpty {
                    Button("Clear…") { confirmingClear = true }
                        .controlSize(.small)
                        .help("Delete every statement in the history")
                }

                // Offered only with something to keep, rather than dimmed for
                // the whole of a session the way a permanent one would be on the
                // Content tab.
                if model.queryPanelTab == .favorites, model.savedQuery != nil {
                    Button("Save Query…") {
                        typedName = ""
                        naming = true
                    }
                    .controlSize(.small)
                    .help("Keep the statement ⌘R would send, under a name")
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
                .foregroundStyle(Theme.Text.secondary.color)
                .help("Hide the history (⇧⌘H)")
                .accessibilityLabel("Hide query history")
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.Surface.overlay.color)
    }

    private var list: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(model.shownHistory) { entry in
                    Button {
                        model.recall(entry)
                    } label: {
                        row(entry)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        // Sized from what is drawn, so narrowing the list shrinks the panel
        // rather than leaving four rows floating in seven rows of space.
        .frame(
            height: Self.rowHeight
                * CGFloat(min(model.shownHistory.count, Self.maxRows))
        )
    }

    /// Keeps what is in the editor under the typed name, and puts the header
    /// back. Silent where nothing was kept, because the only way that happens is
    /// an empty name — and the Save button is already disabled for one.
    ///
    /// The panel stays open, unlike every other thing that finishes in it. The
    /// list underneath is the only confirmation this action has, and closing on
    /// success would hide the one thing the user is looking for.
    private func keep() {
        guard model.saveQuery(named: typedName) else { return }
        naming = false
        typedName = ""
    }

    private var favoritesList: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(model.offeredFavorites) { favorite in
                    Button {
                        model.recall(favorite)
                    } label: {
                        favoriteRow(favorite)
                    }
                    .buttonStyle(.plain)
                    // Outside the button's own label rather than inside it: a
                    // button nested in a button's label never receives the click
                    // that was meant for it, and forgetting a query by missing
                    // the trash by two points is the wrong way to find that out.
                    .overlay(alignment: .trailing) {
                        if hovered == favorite.id {
                            Button {
                                model.favorites.remove(favorite.id)
                            } label: {
                                Image(systemName: "trash")
                                    .font(.system(size: 9))
                                    .frame(width: 18, height: 18)
                                    .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(Theme.Text.secondary.color)
                            .help("Forget this query")
                            .accessibilityLabel("Forget \(favorite.name)")
                            .padding(.trailing, Theme.Space.md)
                        }
                    }
                }
            }
        }
        .frame(
            height: Self.rowHeight
                * CGFloat(min(model.offeredFavorites.count, Self.maxRows))
        )
    }

    private func favoriteRow(_ favorite: QueryFavorite) -> some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "star.fill")
                .font(.system(size: 9))
                .foregroundStyle(Theme.Accent.selection.color)
                .frame(width: 12)

            // The name leads, because it is what this list is searched by. The
            // statement beside it is how somebody confirms they picked the one
            // they meant before pressing ⌘R on it.
            Text(favorite.name)
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)

            Text(favorite.sql.split(whereSeparator: \.isWhitespace).joined(separator: " "))
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(Theme.Text.secondary.color)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)

            // Where the trash button is drawn. Reserved rather than overlapped,
            // so the statement is not read through the icon that sits over it.
            Color.clear.frame(width: 18)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: Self.rowHeight)
        .background(hovered == favorite.id ? Theme.Surface.overlay.color : Color.clear)
        .contentShape(Rectangle())
        // Every row is one line of a statement that may be twenty; the tooltip
        // is what makes the rest of it reachable without recalling it first.
        .help(favorite.sql)
        .onHover { inside in
            if inside {
                hovered = favorite.id
            } else if hovered == favorite.id {
                hovered = nil
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(favorite.name), \(favorite.sql)")
    }

    /// What the server took, or nothing at all where nobody measured it.
    ///
    /// Blank rather than "0 ms". A zero means unmeasured — an edit's statements
    /// are not timed one by one — and printing it would make the row nobody
    /// timed the fastest-looking one on the list.
    ///
    /// Milliseconds up to a second and seconds past it, which is where the digit
    /// that matters moves: "1,240 ms" is read by counting commas and "1.24 s" is
    /// read at a glance.
    private static func took(_ entry: QueryHistoryEntry) -> String {
        guard entry.milliseconds > 0 else { return "" }
        return entry.milliseconds < 1000
            ? "\(Int(entry.milliseconds)) ms"
            : String(format: "%.2f s", entry.milliseconds / 1000)
    }

    private func row(_ entry: QueryHistoryEntry) -> some View {
        let failed = entry.outcome.isFailure
        return HStack(spacing: Theme.Space.sm) {
            // Shape as well as colour, for the reason `StatusDot` carries one:
            // the row still reads as a failure without colour vision.
            Image(systemName: failed ? "exclamationmark.triangle.fill" : "checkmark.circle")
                .font(.system(size: 9))
                .foregroundStyle(
                    failed ? Theme.Semantic.dangerText.color : Theme.Accent.execute.color
                )
                .frame(width: 12)

            // The statement keeps the content tone whatever happened to it. It
            // is what the row is being scanned for, and a red line of SQL reads
            // as the text itself being the problem.
            Text(entry.preview)
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)

            // Only while everything is shown. With the list narrowed to what was
            // typed every row would say "query", which is a column of one word
            // repeated taking space from the statement beside it.
            if model.showsAllStatements {
                Text(entry.origin.rawValue)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .frame(width: 44, alignment: .leading)
            }

            Text(entry.outcome.label)
                .font(Theme.Typography.digits)
                .foregroundStyle(
                    failed ? Theme.Semantic.dangerText.color : Theme.Text.secondary.color
                )
                .lineLimit(1)

            Text(Self.took(entry))
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
                .frame(width: 52, alignment: .trailing)

            Text(Self.age(of: entry))
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
                .frame(width: 62, alignment: .trailing)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: Self.rowHeight)
        .background(hovered == entry.id ? Theme.Surface.overlay.color : Color.clear)
        .overlay(alignment: .leading) {
            // The same 2pt rule `InlineBanner` wears, so a failure is findable
            // by running an eye down the edge rather than by reading four
            // columns of every row.
            Rectangle()
                .fill(failed ? Theme.Semantic.danger.color : Color.clear)
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
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(Array(model.scriptSteps.enumerated()), id: \.element.id) {
                        index, step in
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
        }
        .background(Theme.Surface.raised.color)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Statement outcomes")
    }

    /// Says which list this is.
    ///
    /// Without it, opening the history above a script's outcomes stacked two
    /// unlabelled-looking lists of the same statements with near-identical
    /// counts down the right — the same three lines twice, reading as one list
    /// that had somehow repeated itself. The hairline between them is not enough
    /// to carry that on its own, and the header is what makes the repetition
    /// obviously deliberate: one is everything this window has run, the other is
    /// what this run just did.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text("This run")
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.secondary.color)
            Text(AppModel.pluralized(model.scriptSteps.count, "statement"))
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
            Spacer(minLength: Theme.Space.sm)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.Surface.overlay.color)
    }

    private func row(_ step: ScriptStep, isSelected: Bool) -> some View {
        HStack(spacing: Theme.Space.sm) {
            // The ordinal, because the status bar and the editor's corner both
            // count statements and this is the same numbering.
            Text("\(step.id)")
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.tertiary.color)
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
        .background(isSelected ? Theme.Surface.overlay.color : Color.clear)
        .overlay(alignment: .leading) {
            // The accent bar carries the selection where the fill is subtle by
            // design, and puts it at the edge the eye runs down when scanning a
            // list of ordinals.
            Rectangle()
                .fill(isSelected ? Theme.Accent.selection.color : Color.clear)
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
        if case .notRun = step.outcome { return Theme.Text.tertiary.color }
        return isSelected ? Theme.Text.primary.color : Theme.Text.secondary.color
    }

    private func outcomeTone(_ outcome: StatementOutcome) -> Color {
        switch outcome {
        case .failed: return Theme.Semantic.dangerText.color
        // Neither red nor dimmed. A cancelled statement is not a fault and did
        // not fail to happen — it was stopped, and the row should read as a
        // statement of fact rather than as a warning or as absence.
        case .cancelled: return Theme.Text.secondary.color
        case .notRun: return Theme.Text.tertiary.color
        case .rows, .completed: return Theme.Text.secondary.color
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
                .foregroundStyle(Theme.Text.secondary.color)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
                .textSelection(.enabled)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.Surface.canvas.color)
        .accessibilityElement(children: .combine)
    }

    private var symbol: String {
        switch step.outcome {
        case .failed: return "exclamationmark.triangle"
        case .cancelled: return "stop.circle"
        case .notRun: return "minus.circle"
        case .rows, .completed: return "checkmark.circle"
        }
    }

    private var tint: Color {
        switch step.outcome {
        case .failed: return Theme.Semantic.dangerText.color
        case .cancelled: return Theme.Text.secondary.color
        case .notRun: return Theme.Text.tertiary.color
        case .rows, .completed: return Theme.Accent.execute.color
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
    /// The model, where this inspector is over rows that can be written to.
    ///
    /// Nil in the Query pane, which is what keeps the editor off a result whose
    /// rows belong to no one relation — and the reason this is a parameter
    /// rather than something read from the environment: which of the two panes
    /// this is drawn in is exactly the question being answered.
    var editing: AppModel? = nil

    var body: some View {
        // Rendered once and handed to both halves. The strip's descriptor and
        // the pane are two readings of the same work, and doing it twice would
        // re-indent a document twice on every arrow key.
        let rendered = cell.flatMap { $0.isExpanded ? RenderedValue.make(from: $0) : nil }
        // Nil in the Query pane, where nothing may be written at all.
        let offer = editing.flatMap { $0.editedValue }
        VStack(spacing: 0) {
            strip(rendered, offer: offer)
            if let rendered {
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                if let editing, editing.isEditingValue, case .editable(let seed)? = offer {
                    CellValueEditor(model: editing, seed: seed)
                } else {
                    CellValueViewer(rendered: rendered)
                }
            }
            if let editing {
                Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
                // Without a cell as well, because a table with no rows in it is
                // where Add Row is worth most and there is nothing to select.
                CellEditorRow(model: editing, cell: cell)
            }
        }
        // A box still open over a cell it was not opened for is one keystroke
        // away from writing this value into that one — the hazard
        // `CellEditorRow` re-seeds its field against, and worse here, because
        // the box holds a whole document rather than a line.
        .onChange(of: identity) { editing?.isEditingValue = false }
    }

    /// Which cell the pane is over, as one string to watch.
    ///
    /// Not whether the pane is open, which this used to carry as well: opening
    /// it changes the string too, so an edit begun in the same turn as the
    /// pane — which is what `--edit-value` does, and the only way a capture can
    /// reach the box at all — was ended before it could be drawn. Closing is
    /// where that rule belonged, and `AppModel.toggleValueViewer` is where it
    /// now is.
    private var identity: String {
        cell.map { $0.address + "\u{1}" + $0.column } ?? ""
    }

    private func strip(_ rendered: RenderedValue?, offer: ValueEdit?) -> some View {
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
                .foregroundStyle(Theme.Text.secondary.color)
                .help(cell.isExpanded ? "Hide the value (⌥⌘V)" : "Show the value in full (⌥⌘V)")
                .accessibilityLabel(cell.isExpanded ? "Hide value" : "Show value in full")

                Text(cell.column)
                    .font(Theme.Typography.captionEmphasis)
                    .foregroundStyle(Theme.Text.secondary.color)

                if !cell.type.isEmpty {
                    Text(cell.type)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.Text.tertiary.color)
                }

                if let rendered {
                    Text(rendered.descriptor)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.Text.tertiary.color)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text(cell.value)
                        .font(Theme.Typography.monoSmall)
                        .foregroundStyle(
                            cell.isNull ? Theme.Text.tertiary.color : Theme.Text.primary.color
                        )
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }

                // Only where something may be written, only while the pane is
                // open — a pencil beside a closed strip would open a box over a
                // value nobody has looked at — and not while it already is the
                // box, which is its own affordance.
                if let offer, let editing, cell.isExpanded, !editing.isEditingValue {
                    Button {
                        editing.isEditingValue = true
                    } label: {
                        Image(systemName: "square.and.pencil")
                            .font(.system(size: 10))
                            .frame(width: 20, height: 18)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    // Tertiary here is a dimmed control rather than a contrast
                    // failure: a disabled button is meant to read as disabled,
                    // and the reason is already on screen — one line below in
                    // `CellEditorRow` for a row that cannot be written, and in
                    // the descriptor beside this for a value that cannot:
                    // "hex dump · 200 bytes", "first 131,072 of 400,000
                    // characters".
                    .foregroundStyle(
                        offer.isEditable ? Theme.Text.secondary.color : Theme.Text.tertiary.color
                    )
                    .disabled(!offer.isEditable)
                    .help(offer.refusal ?? "Edit this value in a box")
                    .accessibilityLabel("Edit cell value")
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
                .foregroundStyle(Theme.Text.secondary.color)
                .help("Copy value (⌘C)")
                .accessibilityLabel("Copy cell value")
            } else {
                // Secondary rather than tertiary: this strip is the only thing
                // telling a reader what the row under the grid is for, and it
                // sits on `Surface.overlay`, the lightest surface in the window
                // and the one tertiary has the least contrast against.
                Text("Select a cell to inspect its value")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 26)
        .background(Theme.Surface.overlay.color)
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
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

/// What the Query pane shows while a statement runs and there is nothing behind
/// it worth keeping.
///
/// Not `LoadingVeil`. That exists to dim stale rows so they stay readable as
/// context while being replaced, and it puts its own label in the middle of what
/// it covers — which is fine over a grid and unreadable over anything else
/// centred. Over the empty state, "Running…" landed exactly on top of "No
/// results yet" and each made the other illegible. Neither of the two things it
/// replaces is worth preserving: an empty state and a one-sentence note are
/// rebuilt the instant the statement lands, unlike a result set.
struct RunningPane: View {
    var body: some View {
        LoadingVeil(isVisible: true)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Theme.Surface.canvas.color)
    }
}

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
                Theme.Surface.canvas.opacity(0.55).color
                VStack(spacing: Theme.Space.sm) {
                    ProgressView().controlSize(.small)
                    Text("Running…")
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.Text.secondary.color)
                }
            }
        }
        .allowsHitTesting(false)
        .animation(Theme.Motion.ease(reduceMotion), value: isVisible)
        .accessibilityHidden(!isVisible)
        .accessibilityLabel("Running query")
    }
}

// MARK: - Find in the grid

/// The bar ⌘F opens under the grid.
///
/// A bar of its own rather than a field squeezed into the status line, which is
/// where this was first drawn. The status line is what a search most needs
/// beside it — "first 100,000 of ~1,000,000 rows" is the sentence that makes
/// this bar's own sentence mean something — and taking its place to say "no
/// match" would have hidden the count that explains the answer.
///
/// It appears and disappears, which does move the grid. That is the one place
/// the argument in `ValueViewer` about not moving the grid does not apply: this
/// is a mode somebody entered on purpose and leaves with ⎋, not something that
/// happens as a side effect of moving the cursor.
struct GridFindBar: View {
    @Bindable var model: AppModel
    @FocusState.Binding var focus: FocusArea?

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 10))
                .foregroundStyle(Theme.Text.tertiary.color)

            CompactField(
                placeholder: "Find in fetched rows", text: $model.gridFindText,
                area: .gridFind, focus: $focus,
                // Return finds the next one, which is what Return means in every
                // find bar. The search does not run on every keystroke: it walks
                // whatever is loaded, and a hundred thousand rows per character
                // typed is not something a field can afford.
                onSubmit: { model.findInGrid() }
            )
            .frame(width: 220)

            // Every column, or one. A restricted search is the difference
            // between finding an id in the id column and finding it inside a
            // JSON document three columns over.
            Picker("", selection: columnBinding) {
                Text("All columns").tag("")
                ForEach(model.gridFindColumns, id: \.self) { Text($0).tag($0) }
            }
            .labelsHidden()
            .frame(maxWidth: 160)
            .help("Look in one column instead of all of them")

            Button("Previous") { model.findInGrid(backwards: true) }
                .buttonStyle(.link)
                .font(Theme.Typography.micro)
                .disabled(model.gridFindText.isEmpty)
                .help("The match before this one (⇧⌘G)")
            Button("Next") { model.findInGrid() }
                .buttonStyle(.link)
                .font(Theme.Typography.micro)
                .disabled(model.gridFindText.isEmpty)
                .help("The next match (⌘G)")

            if !model.gridFindReport.isEmpty {
                Text(model.gridFindReport)
                    .font(Theme.Typography.digits)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }

            Spacer(minLength: Theme.Space.sm)

            // The sentence the plan asks for, and the reason this feature is
            // honest. What is searched is what has been fetched; the whole table
            // is what the filter rows are for, and they run on the server.
            Text("searches fetched rows only")
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
                .help("Use the filter rows above to search the whole table on the server")

            Button {
                model.closeGridFind()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .semibold))
                    .frame(width: 20, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.Text.secondary.color)
            .help("Close the find bar (⎋)")
            .accessibilityLabel("Close find bar")
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
        .background(Theme.Surface.overlay.color)
        // Opened by a menu command from a grid that has the keyboard, so the
        // field has to take focus itself — otherwise ⌘F draws a bar and the next
        // thing typed goes to the grid underneath it.
        .task { focus = .gridFind }
        .onKeyPress(.escape) {
            model.closeGridFind()
            return .handled
        }
        // ⇧Return steps backwards, because ⇧⌘G cannot reach here: the field has
        // the keyboard while the bar is being typed in, and the Edit menu's
        // find commands go to whichever responder holds it. From the grid, where
        // the cursor is the rest of the time, both shortcuts work.
        .onKeyPress(.return, phases: .down) { press in
            guard press.modifiers.contains(.shift) else { return .ignored }
            model.findInGrid(backwards: true)
            return .handled
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Find in fetched rows")
    }

    /// The picker speaks in strings because `nil` is not a `Hashable` tag worth
    /// inventing a case for; the empty one is "all columns".
    private var columnBinding: Binding<String> {
        Binding(
            get: { model.gridFindColumn ?? "" },
            set: { model.gridFindColumn = $0.isEmpty ? nil : $0 })
    }
}

// MARK: - Status bar

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            // Which database this is, at the end of the window the eye rests on
            // when the grid is full width. The toolbar chip carries the same
            // name, and that is the point: the chip is at the far corner, and a
            // status line that describes a result without saying whose result
            // it is has left the most expensive mistake unmarked.
            Text(model.connectionLabel)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
                .lineLimit(1)
            Rectangle()
                .fill(Theme.Border.hairline.color)
                .frame(width: 1, height: 10)

            // A truncated result is worth catching out of the corner of an eye,
            // not only on a careful read of the count.
            if model.current.capped && model.activeTab != .structure {
                Image(systemName: "rectangle.compress.vertical")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.Semantic.warning.color)
                    .help(truncationHelp)
                    .accessibilityLabel("Result truncated")
            }

            // The text stays neutral. A partial view is the normal state for a
            // large table, not a warning, and an amber status line that is
            // always on becomes wallpaper.
            Text(model.statusLine)
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.Text.secondary.color)
                .lineLimit(1)

            // Attached to the sentence it acts on, like Stop and Load more
            // below: "Disconnected — …" is the status line's sentence about
            // this tab's connection, and the button that dials it again
            // belongs beside the sentence rather than in a menu. The one way
            // back besides ⌘K, and never pressed by anything but a person —
            // there is no automatic reconnection anywhere.
            if model.canRedial {
                Button("Reconnect") { model.redial() }
                    .buttonStyle(.link)
                    .font(Theme.Typography.micro)
                    .help("Dial the same server again — same bastion, timeout and keep-alive")
            }

            // Attached to the sentence it acts on, for the reason Load more is
            // below: "8,192 rows → orders on staging…" is a sentence about
            // something still happening, and the button that ends it belongs
            // beside it rather than in a menu. The export gets the same button
            // because it is the same situation — a long job with a running
            // commentary and, until now, no way to change your mind.
            if model.isTransferring {
                Button("Stop") { model.stopTransfer() }
                    .buttonStyle(.link)
                    .font(Theme.Typography.micro)
                    .help("Stop the transfer. The rows already sent stay where they are.")
            } else if model.isExporting {
                Button("Stop") { model.cancelExport() }
                    .buttonStyle(.link)
                    .font(Theme.Typography.micro)
                    .help("Stop the export. The file keeps the rows already written.")
            } else if model.isImporting {
                Button("Stop") { model.stopImport() }
                    .buttonStyle(.link)
                    .font(Theme.Typography.micro)
                    .help("Stop reading the file. The rows already read stay in the table.")
            }

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
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .help(obstacle.detail)
            }

            Spacer(minLength: Theme.Space.sm)

            if model.activeTab != .structure {
                if let cell = model.inspectedCell(in: model.current) {
                    Text(cell.address)
                        .font(Theme.Typography.digits)
                        .foregroundStyle(Theme.Text.tertiary.color)
                }

                if !model.current.table.columns.isEmpty {
                    Text(AppModel.pluralized(model.current.table.columns.count, "col"))
                        .font(Theme.Typography.digits)
                        .foregroundStyle(Theme.Text.tertiary.color)
                }
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 24)
        .background(Theme.Surface.raised.color)
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
