import AppKit
import UniformTypeIdentifiers

/// The application's menu bar.
///
/// Not decoration. Without a menu, AppKit routes none of the standard key
/// equivalents: ⌘Q does not quit, and — more damaging here — ⌘C/⌘V/⌘Z do not
/// reach the field editor, so the SQL editor cannot be pasted into. The menu is
/// what installs those bindings.
///
/// Deliberately thin: only what the application actually does. A menu listing
/// commands that are not implemented is worse than a short one.
enum AppMenu {
    /// Target of the File menu's export items. `NSMenuItem.target` is a weak
    /// reference, so the menu cannot be the thing that keeps this alive.
    private static var exportCommands: ExportCommands?
    /// Target of the File menu's Connect item, held for the same reason.
    private static var connectionCommand: ConnectionCommand?
    /// Target of the application menu's Settings item, held for the same reason.
    private static var settingsCommand: SettingsCommand?
    /// Target of the View menu's Refresh item, held for the same reason.
    private static var refreshCommand: RefreshCommand?
    /// Target of the View menu's value-viewer item, held for the same reason.
    private static var valueViewerCommand: ValueViewerCommand?
    /// Target of the View menu's object-filter item, held for the same reason.
    private static var navigatorCommand: NavigatorCommand?
    private static var goToCommand: GoToCommand?
    private static var historyNavCommand: HistoryCommand?
    /// Target of the View menu's record item, held for the same reason.
    private static var recordCommand: RecordCommand?
    /// Target of the View menu's three pane items, held for the same reason.
    private static var tabCommand: TabCommand?
    /// Target of the Query menu's items, held for the same reason.
    private static var queryCommands: QueryCommands?
    /// Target of the Query menu's Stop item, held for the same reason.
    private static var stopCommand: StopCommand?
    /// Target of the Query menu's history item, held for the same reason.
    private static var historyCommand: QueryHistoryCommand?
    /// Target of the Query menu's transaction items, held for the same reason.
    private static var transactionCommands: TransactionCommands?
    /// Target of the Query menu's Format item, held for the same reason.
    private static var formatCommand: FormatCommand?
    private static var explainCommand: ExplainCommand?

    @MainActor
    static func install(into app: NSApplication, model: AppModel) {
        // `CFBundleName` when running as a bundle, which is what the menu should
        // say; the process name is the fallback for the unbundled dev binary.
        let name =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String
            ?? ProcessInfo.processInfo.processName
        let commands = ExportCommands(model: model)
        exportCommands = commands
        let connection = ConnectionCommand(model: model)
        connectionCommand = connection
        let settings = SettingsCommand(preferences: model.preferences)
        settingsCommand = settings
        let refresh = RefreshCommand(model: model)
        refreshCommand = refresh
        let valueViewer = ValueViewerCommand(model: model)
        valueViewerCommand = valueViewer
        let navigator = NavigatorCommand(model: model)
        navigatorCommand = navigator
        let goTo = GoToCommand(model: model)
        goToCommand = goTo
        let record = RecordCommand(model: model)
        recordCommand = record
        let historyNav = HistoryCommand(model: model)
        historyNavCommand = historyNav
        let tabs = TabCommand(model: model)
        tabCommand = tabs
        let query = QueryCommands(model: model)
        queryCommands = query
        let stop = StopCommand(model: model)
        stopCommand = stop
        let queryHistory = QueryHistoryCommand(model: model)
        historyCommand = queryHistory
        let transactions = TransactionCommands(model: model)
        transactionCommands = transactions
        let formatting = FormatCommand(model: model)
        formatCommand = formatting
        let explain = ExplainCommand(model: model)
        explainCommand = explain
        let main = NSMenu()
        main.addItem(appMenu(named: name, settings: settings))
        main.addItem(fileMenu(connection: connection, export: commands))
        main.addItem(editMenu())
        main.addItem(
            viewMenu(
                target: refresh, valueViewer: valueViewer, navigator: navigator, tabs: tabs,
                goTo: goTo, historyNav: historyNav, record: record))
        main.addItem(
            queryMenu(
                target: query, stop: stop, history: queryHistory, transactions: transactions,
                formatting: formatting, explain: explain))
        main.addItem(windowMenu(for: app))
        app.mainMenu = main
    }

    /// Settings sits under About and above Hide, with ⌘,, because that is where
    /// every Mac application has kept it for twenty years — it is the one item
    /// here a user looks for without reading the menu.
    private static func appMenu(named name: String, settings: SettingsCommand) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu()
        menu.addItem(
            withTitle: "About \(name)",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: "")
        menu.addItem(.separator())

        let settingsItem = menu.addItem(
            withTitle: "Settings…",
            action: #selector(SettingsCommand.showSettings(_:)), keyEquivalent: ",")
        settingsItem.keyEquivalentModifierMask = .command
        settingsItem.target = settings
        menu.addItem(.separator())

        let hide = menu.addItem(
            withTitle: "Hide \(name)",
            action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        hide.keyEquivalentModifierMask = .command

        let hideOthers = menu.addItem(
            withTitle: "Hide Others",
            action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]

        menu.addItem(
            withTitle: "Show All",
            action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        menu.addItem(
            withTitle: "Quit \(name)",
            action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        item.submenu = menu
        return item
    }

    /// Choosing what this window is looking at, and getting a result out of it.
    ///
    /// Connect… leads, because without it the application can only ever reach
    /// the database it was launched against: changing database would mean
    /// quitting. ⌘K is what Finder binds Connect to Server to, which is the
    /// nearest thing on the platform to what this does, and nothing in this
    /// window has a claim on it.
    ///
    /// The exports: ⌘C is the only other way to get rows out, and it goes to
    /// the pasteboard — which means anything larger than what the next
    /// application will accept as a paste is stuck here. ⇧⌘E rather than ⌘S:
    /// nothing in this window is a document with unsaved changes, and binding
    /// Save to something that is not one teaches the wrong reflex.
    ///
    /// One item per format rather than one item with a format popup in the
    /// panel's accessory view: the popup is a control nobody looks for, and the
    /// menu is where a user goes to find out what an application can do. The
    /// panel's accessory view is spent on the one question the menu cannot ask
    /// — how much of the result to write — and only when there is more of it
    /// than the window is showing.
    private static func fileMenu(connection: ConnectionCommand, export: ExportCommands)
        -> NSMenuItem
    {
        let item = NSMenuItem()
        let menu = NSMenu(title: "File")

        let connect = menu.addItem(
            withTitle: "Connect…",
            action: #selector(ConnectionCommand.presentConnection(_:)), keyEquivalent: "k")
        connect.keyEquivalentModifierMask = .command
        connect.target = connection

        // ⌘W, which was bound to nothing at all. That is not only a missing
        // convenience: the Settings panel is a second window with a close
        // button and no other way out, so without this item there is no
        // keyboard dismissal for it — on a platform where every window on
        // screen has closed with ⌘W for forty years.
        //
        // No target, so it walks the responder chain to whichever window is
        // key, which is what makes one item serve both windows. Grouped with
        // Connect… above rather than put in its own block, because Connect… is
        // this application's Open and Close belongs beside it.
        let close = menu.addItem(
            withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        close.keyEquivalentModifierMask = .command
        menu.addItem(.separator())

        let csv = menu.addItem(
            withTitle: "Export Result as CSV…",
            action: #selector(ExportCommands.exportCSV(_:)), keyEquivalent: "e")
        csv.keyEquivalentModifierMask = [.command, .shift]
        csv.target = export

        let tsv = menu.addItem(
            withTitle: "Export Result as TSV…",
            action: #selector(ExportCommands.exportTSV(_:)), keyEquivalent: "")
        tsv.target = export

        // Only CSV keeps a shortcut. The other four are picked from the menu by
        // somebody who already knows which file they want, and four more
        // modified letters would be spent on choices nobody makes twice a day.
        let json = menu.addItem(
            withTitle: "Export Result as JSON Lines…",
            action: #selector(ExportCommands.exportJSON(_:)), keyEquivalent: "")
        json.target = export

        let parquet = menu.addItem(
            withTitle: "Export Result as Parquet…",
            action: #selector(ExportCommands.exportParquet(_:)), keyEquivalent: "")
        parquet.target = export

        let sql = menu.addItem(
            withTitle: "Export Result as SQL…",
            action: #selector(ExportCommands.exportSQL(_:)), keyEquivalent: "")
        sql.target = export

        menu.addItem(.separator())

        // Below a rule, because it is the only item here that changes the
        // database. Everything above it writes a file.
        let importItem = menu.addItem(
            withTitle: "Import into Table…",
            action: #selector(ExportCommands.importFile(_:)), keyEquivalent: "")
        importItem.target = export

        menu.addItem(.separator())

        // Their own group, under a rule of their own. The five above export a
        // result and the one above them reads rows into a table; these two move
        // the statements somebody keeps, which is neither the data nor the
        // database. No shortcuts: this is done when a Mac is set up or handed
        // over, not twice a day.
        let exportQueries = menu.addItem(
            withTitle: "Export Saved Queries…",
            action: #selector(ExportCommands.exportFavorites(_:)), keyEquivalent: "")
        exportQueries.target = export

        let importQueries = menu.addItem(
            withTitle: "Import Saved Queries…",
            action: #selector(ExportCommands.importFavorites(_:)), keyEquivalent: "")
        importQueries.target = export

        menu.addItem(.separator())

        // Under a rule of its own, below the two that move saved queries. Those
        // two carry a list somebody curates in both directions; this one only
        // goes out, and what it writes is a record nobody edits.
        let exportLog = menu.addItem(
            withTitle: "Export Statement Log…",
            action: #selector(ExportCommands.exportStatements(_:)), keyEquivalent: "")
        exportLog.target = export

        item.submenu = menu
        return item
    }

    /// Standard editing commands, sent down the responder chain. The field
    /// editor implements every one of these; the menu exists only to bind them.
    private static func editMenu() -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "Edit")

        menu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = menu.addItem(
            withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(.separator())

        menu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        menu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        menu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        menu.addItem(
            withTitle: "Select All",
            action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")

        menu.addItem(.separator())
        // The editor opens its list of names by itself while a name is being
        // typed; this is how a user asks for one anywhere else — after `FROM `,
        // where the automatic trigger deliberately stays quiet. ⌥⎋ because that
        // is what AppKit has always bound `complete:` to, and the item is what
        // makes the keystroke findable: nothing else in the interface can
        // announce it.
        let complete = menu.addItem(
            withTitle: "Complete", action: #selector(NSResponder.complete(_:)),
            keyEquivalent: "\u{1b}")
        complete.keyEquivalentModifierMask = .option

        item.submenu = menu
        return item
    }

    /// Bringing the window back in line with the database.
    ///
    /// The object tree and the Structure pane are read once and then believed,
    /// so a CREATE or a DROP from anywhere — including this window's own Query
    /// tab — leaves them describing a database that has moved on. Without this
    /// item the only way to see current metadata is to quit and relaunch.
    ///
    /// ⇧⌘R rather than ⌘R, which the Run button already owns: running a
    /// statement happens a hundred times an hour and reloading the tree a few
    /// times a session, so the plain shortcut belongs to the frequent one. View
    /// rather than File, because View is where a Mac user looks for a reload —
    /// it is where Safari keeps one — and File is about getting things in and
    /// out of the window.
    ///
    /// The value viewer is here for the same reason: it changes what this window
    /// shows without changing anything in the database. ⌥⌘V because ⌘V is Paste
    /// and always will be, and ⇧⌘V is the paste variant every other application
    /// puts there — a value viewer bound to either would be a trap in a window
    /// whose main pane is a text editor. The item is what makes the shortcut
    /// findable at all; nothing else in the interface can announce a keystroke.
    ///
    /// Filter Objects is here on the same grounds — it narrows what this window
    /// lists without touching the database — and takes ⌥⌘F rather than ⌘F. The
    /// main pane is a text editor, and a plain ⌘F in an editor means find in the
    /// text; binding it to the sidebar would claim a key this application will
    /// want for the obvious thing later, and teach the wrong reflex until then.
    /// ⌥⌘F is also where Sequel Ace, which this window's layout follows, keeps
    /// its own table filter.
    ///
    /// The three panes lead it, because which one the window is showing is the
    /// largest thing View decides. ⌘1/⌘2/⌘3 were declared on the tab buttons
    /// themselves until now: they worked, and the only way to find out they
    /// existed was to hover a tab and read its tooltip, while the menu a Mac user
    /// opens to learn what a window can do said nothing about them. They are
    /// declared here and nowhere else — see the Run note in `queryMenu` for why
    /// two declarations of one key equivalent is not an option — and the
    /// checkmark is what makes the menu say which pane is open, which is the
    /// second thing a menu item can do that a bare shortcut cannot.
    private static func viewMenu(
        target: RefreshCommand, valueViewer: ValueViewerCommand, navigator: NavigatorCommand,
        tabs: TabCommand, goTo: GoToCommand, historyNav: HistoryCommand, record: RecordCommand
    ) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "View")

        // In the tab bar's own order, so the numbers on screen and the numbers
        // in the menu are the same numbers.
        for (index, tab) in DetailTab.allCases.enumerated() {
            let pane = menu.addItem(
                withTitle: tab.rawValue,
                action: #selector(TabCommand.selectTab(_:)), keyEquivalent: "\(index + 1)")
            pane.keyEquivalentModifierMask = .command
            pane.tag = index
            pane.target = tabs
        }
        menu.addItem(.separator())

        let refresh = menu.addItem(
            withTitle: "Refresh",
            action: #selector(RefreshCommand.refresh(_:)), keyEquivalent: "r")
        refresh.keyEquivalentModifierMask = [.command, .shift]
        refresh.target = target

        let filter = menu.addItem(
            withTitle: "Filter Objects",
            action: #selector(NavigatorCommand.focusFilter(_:)), keyEquivalent: "f")
        filter.keyEquivalentModifierMask = [.command, .option]
        filter.target = navigator

        // ⇧⌘O, which is what every editor with this command binds it to, and
        // which nothing else in this window takes. It sits beside Filter Objects
        // because the two are the same errand at different speeds: one narrows
        // the tree to look through, the other skips the looking.
        let goToItem = menu.addItem(
            withTitle: "Go to Table…",
            action: #selector(GoToCommand.showGoTo(_:)), keyEquivalent: "o")
        goToItem.keyEquivalentModifierMask = [.command, .shift]
        goToItem.target = goTo

        // ⌘[ and ⌘], which is where a decade of browsers and Xcode put Back and
        // Forward, and which nothing in this window takes. Beside Go to Table
        // because all three are the same errand: getting to a table without
        // hunting for it in the tree.
        let back = menu.addItem(
            withTitle: "Back", action: #selector(HistoryCommand.goBack(_:)), keyEquivalent: "[")
        back.keyEquivalentModifierMask = .command
        back.target = historyNav
        let forward = menu.addItem(
            withTitle: "Forward", action: #selector(HistoryCommand.goForward(_:)),
            keyEquivalent: "]")
        forward.keyEquivalentModifierMask = .command
        forward.target = historyNav

        menu.addItem(.separator())
        // Titled for the closed state; `validateMenuItem` rewrites it.
        let value = menu.addItem(
            withTitle: "Show Value in Full",
            action: #selector(ValueViewerCommand.toggleValueViewer(_:)), keyEquivalent: "v")
        value.keyEquivalentModifierMask = [.command, .option]
        value.target = valueViewer

        // ⌃⌘R, and not the ⌥⌘R this was planned with: ⌥⌘R is Run Script, ⇧⌘R is
        // Refresh and ⌘R is Run, so every modified R in this application is
        // already spoken for. The letter is kept because it is the one somebody
        // reaches for — "record" — and ⌃⌘ is the one combination free of both
        // this window and the system's own bindings.
        //
        // Beside Show Value in Full because the two are the same errand at
        // different sizes: one value too wide for its cell, or one row too wide
        // for the window. Titled for the state it is not in; `validateMenuItem`
        // rewrites it.
        let recordItem = menu.addItem(
            withTitle: "Show as Record",
            action: #selector(RecordCommand.toggleRecordView(_:)), keyEquivalent: "r")
        recordItem.keyEquivalentModifierMask = [.command, .control]
        recordItem.target = record

        item.submenu = menu
        return item
    }

    /// Running more than the statement the caret is in.
    ///
    /// ⌥⌘R rather than a shortcut of its own: it is ⌘R with more of the buffer,
    /// and the modifier says so. Run itself is not repeated here — it belongs to
    /// the toolbar's Run button, which is where the eye already goes and which
    /// declares ⌘R for it. Two declarations of one key equivalent is a race
    /// between AppKit's menu and SwiftUI's shortcut that nobody would win twice
    /// in a row.
    ///
    /// The history is here rather than under View, which is where Refresh and
    /// the value viewer sit: those change what the window shows about the
    /// database, and this is a list of what was sent to it — the same subject as
    /// the two items above it. ⇧⌘H because ⌘H is Hide and ⌥⌘H is Hide Others on
    /// every Mac and always will be, which leaves the shift variant as the
    /// nearest free key that still spells History; nothing else in this window
    /// binds it. The item opens the panel and does not close it: a list that
    /// closes when a statement is picked, and carries its own dismiss button,
    /// has no question left for a menu to answer.
    ///
    /// Stop takes ⌘., which has meant "stop what you are doing" on this platform
    /// since long before any of the alternatives, and it is here as well as on
    /// the toolbar because the toolbar button only exists while a statement is
    /// running: a command discoverable only during the seconds you need it is
    /// not discoverable. It stops metadata reads and browses too, not just the
    /// Query pane, which is why it sits above the separator with Run Script
    /// rather than being scoped to one tab.
    ///
    /// The transaction items are at the bottom because they are about the
    /// statements that have already been sent rather than about sending one.
    /// Commit takes ⇧⌘C — ⌘C is Copy and always will be — and Rollback takes no
    /// key at all: it destroys work, and a shortcut is exactly how it would get
    /// pressed by somebody reaching for another one. Manual Commit is a
    /// checkmark rather than two items, because it is one connection setting
    /// with two values and a pair of items would let the window show both as off.
    private static func queryMenu(
        target: QueryCommands, stop: StopCommand, history: QueryHistoryCommand,
        transactions: TransactionCommands, formatting: FormatCommand, explain: ExplainCommand
    ) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "Query")
        let script = menu.addItem(
            withTitle: "Run Script",
            action: #selector(QueryCommands.runScript(_:)), keyEquivalent: "r")
        script.keyEquivalentModifierMask = [.command, .option]
        script.target = target

        // ⌥⌘E. ⇧⌘E is Export Result as CSV, and ⌥⌘ is the modifier Run Script
        // already uses for "run something other than the plain ⌘R", which is
        // what asking for a plan is.
        let explainItem = menu.addItem(
            withTitle: "Explain Statement",
            action: #selector(ExplainCommand.explainStatement(_:)), keyEquivalent: "e")
        explainItem.keyEquivalentModifierMask = [.command, .option]
        explainItem.target = explain

        let stopItem = menu.addItem(
            withTitle: "Stop Running Statement",
            action: #selector(StopCommand.stopRunningStatement(_:)), keyEquivalent: ".")
        stopItem.keyEquivalentModifierMask = .command
        stopItem.target = stop

        // ⌃⌥F, which is what every editor that has this command uses and what
        // upstream binds. It is offered while a statement is running, unlike
        // everything above it: laying the buffer out never reaches the server.
        let formatItem = menu.addItem(
            withTitle: "Format Statement",
            action: #selector(FormatCommand.formatQuery(_:)), keyEquivalent: "f")
        formatItem.keyEquivalentModifierMask = [.control, .option]
        formatItem.target = formatting

        menu.addItem(.separator())
        let recent = menu.addItem(
            withTitle: "Query History",
            action: #selector(QueryHistoryCommand.showQueryHistory(_:)), keyEquivalent: "h")
        recent.keyEquivalentModifierMask = [.command, .shift]
        recent.target = history

        menu.addItem(.separator())
        let manual = menu.addItem(
            withTitle: "Manual Commit",
            action: #selector(TransactionCommands.toggleManualCommit(_:)), keyEquivalent: "")
        manual.target = transactions

        let commit = menu.addItem(
            withTitle: "Commit",
            action: #selector(TransactionCommands.commit(_:)), keyEquivalent: "c")
        commit.keyEquivalentModifierMask = [.command, .shift]
        commit.target = transactions

        let rollback = menu.addItem(
            withTitle: "Rollback",
            action: #selector(TransactionCommands.rollback(_:)), keyEquivalent: "")
        rollback.target = transactions

        item.submenu = menu
        return item
    }

    private static func windowMenu(for app: NSApplication) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "Window")
        menu.addItem(
            withTitle: "Minimize",
            action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        menu.addItem(
            withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        item.submenu = menu
        app.windowsMenu = menu
        return item
    }
}

/// The File menu's Connect item, as something a menu can send to.
///
/// Its own object rather than another action on `ExportCommands`, for the reason
/// `RefreshCommand` is one: `validateMenuItem` answers for every item that
/// targets it, and this item is available in states where an export is not.
@MainActor
final class ConnectionCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func presentConnection(_ sender: Any?) { model.presentConnection() }

    /// Greyed out only while an attempt is in flight. Unlike every other command
    /// here it does not need a connection — it is what a window with no
    /// connection is for — and it stays available over a working session,
    /// because changing database is the thing it exists to do.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { !model.isConnecting }
}

/// The application menu's Settings item, as something a menu can send to.
///
/// Holds the preferences rather than the model, which is the only target here
/// that does not: nothing in this window has to exist for a setting to be
/// changed, and the item stays available with no connection for that reason —
/// which is also why it declares no `validateMenuItem`.
@MainActor
final class SettingsCommand: NSObject {
    private let preferences: Preferences
    private let window = SettingsWindow()

    init(preferences: Preferences) {
        self.preferences = preferences
        super.init()
    }

    @objc func showSettings(_ sender: Any?) { window.present(preferences) }
}

/// The View menu's Refresh item, as something a menu can send to.
///
/// Its own object rather than a second action on `ExportCommands`, because
/// `validateMenuItem` answers for every item that targets it: one target per
/// menu is what keeps that answer a sentence instead of a switch over selectors.
@MainActor
final class RefreshCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func refresh(_ sender: Any?) { model.refresh() }

    /// Greyed out before the connection is up and while something is already
    /// running, so the item is never offered when pressing it would only queue
    /// a second read behind the first and land looking like nothing happened.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canRefresh }
}

/// The View menu's value-viewer item, as something a menu can send to.
///
/// Its own object rather than a second action on `RefreshCommand`, for the
/// reason that class gives: `validateMenuItem` answers for every item that
/// targets it, and these two answer differently.
@MainActor
final class ValueViewerCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func toggleValueViewer(_ sender: Any?) { model.toggleValueViewer() }

    /// Greyed out while no cell is selected, and re-titled to say what pressing
    /// it will do. A toggle whose title never changes leaves the reader working
    /// out which way it points from a pane they may not be able to see, and this
    /// is the only place the shortcut is written down.
    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        item.title = model.isValueViewerOpen ? "Hide Value" : "Show Value in Full"
        return model.canInspectValue
    }
}

/// The View menu's object-filter item, as something a menu can send to.
///
/// Its own object for the reason `RefreshCommand` gives: `validateMenuItem`
/// answers for every item pointed at it, and these answer differently.
@MainActor
final class NavigatorCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    /// Puts the caret in the sidebar's filter field. Deliberately does not
    /// clear it: someone reaching for this while a filter is already on is
    /// nearly always about to edit the word they typed, not to start over — and
    /// Escape is right there for starting over.
    @objc func focusFilter(_ sender: Any?) { model.focusNavigatorFilter() }

    /// Greyed out until the tree has something in it. Focusing a field that can
    /// only ever filter nothing is a command that does nothing.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canFilterObjects }
}

/// The View menu's three pane items, as something a menu can send to.
///
/// One target for the three, unlike the rest of this file: they are one choice
/// with three values, their `validateMenuItem` answer is the same sentence, and
/// splitting them would let two panes show a checkmark at once. Which item is
/// which is carried in `tag` rather than in three selectors, so adding a pane to
/// `DetailTab` adds it here without touching this class.
@MainActor
final class TabCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func selectTab(_ sender: NSMenuItem) {
        guard let tab = Self.tab(for: sender) else { return }
        model.activeTab = tab
    }

    /// Checkmarked for the pane on screen, and greyed out while the connection
    /// form has replaced the shell: there are no panes to switch between then,
    /// and ⌘1 landing on a hidden tab bar would leave the form showing over a
    /// window that had silently changed underneath it.
    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        item.state = Self.tab(for: item) == model.activeTab ? .on : .off
        return !model.isPresentingConnection
    }

    private static func tab(for item: NSMenuItem) -> DetailTab? {
        let tabs = DetailTab.allCases
        return tabs.indices.contains(item.tag) ? tabs[item.tag] : nil
    }
}

/// The Query menu's items, as something a menu can send to.
///
/// Its own object rather than a second action on an existing target, for the
/// reason `RefreshCommand` is one: `validateMenuItem` answers for every item
/// pointed at it, and one target per menu keeps that answer a sentence.
@MainActor
final class QueryCommands: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func runScript(_ sender: Any?) { model.runScript() }

    /// Greyed out away from the Query tab, while a run is in flight, and over a
    /// buffer with nothing runnable in it — a buffer holding only comments has
    /// text and no statements, which is why this asks the model rather than
    /// measuring the string.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canRunScript }
}

/// The Query menu's Format item, as something a menu can send to.
///
/// Its own object for the reason the others are: one target per answer, so
/// `validateMenuItem` stays a sentence. This one's answer differs from every
/// other item in that menu — a buffer holding only comments can be laid out,
/// and so can one on a connection that is busy.
@MainActor
final class FormatCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func formatQuery(_ sender: Any?) { model.formatQuery() }

    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canFormatQuery }
}

/// The View menu's Back and Forward items, as something a menu can send to.
///
/// Two items on one target, unlike the others in this file, so `validateMenuItem`
/// has to ask which item is asking. Splitting them into an object each to match
/// the rest would be a class per menu item for a uniformity nothing reads.
@MainActor
final class HistoryCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func goBack(_ sender: Any?) { model.goBack() }

    @objc func goForward(_ sender: Any?) { model.goForward() }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        item.action == #selector(goForward(_:)) ? model.canGoForward : model.canGoBack
    }
}

/// The View menu's Go to Table item, as something a menu can send to.
///
/// Its own object for the reason the others are: one target per answer, so
/// `validateMenuItem` stays a sentence. This one's differs from
/// `NavigatorCommand`'s in what it asks about — the filter field exists as soon
/// as there are schemas, and there is nothing to go *to* until relations have
/// been read.
@MainActor
final class GoToCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func showGoTo(_ sender: Any?) { model.isGoToOpen = true }

    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canGoTo }
}

/// The Query menu's Explain item, as something a menu can send to.
///
/// Its own object for the reason the others are: one target per answer, so
/// `validateMenuItem` stays a sentence. This one's answer is `QueryCommands`'
/// plus a question about the database rather than about the buffer — a
/// connection whose dialect has no prefix greys this out and leaves Run Script
/// alone.
@MainActor
final class ExplainCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func explainStatement(_ sender: Any?) { model.explainCurrentStatement() }

    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canExplainStatement }
}

/// The Query menu's Stop item, as something a menu can send to.
///
/// Its own object because its answer is the negation of `QueryCommands`': Run
/// Script is offered only when nothing is running, and this only when something
/// is. Pointed at one target they would light up together, which for the one
/// command that exists to interrupt the other is not a subtlety.
@MainActor
final class StopCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func stopRunningStatement(_ sender: Any?) { model.cancelRunningStatement() }

    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canCancel }
}

/// The Query menu's history item, as something a menu can send to.
///
/// Its own object rather than a second action on `QueryCommands`, for the reason
/// `RefreshCommand` is one: `validateMenuItem` answers for every item pointed at
/// it, and these two answer differently — a run in flight greys out Run Script
/// and has no bearing at all on reading what already ran.
@MainActor
final class QueryHistoryCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func showQueryHistory(_ sender: Any?) { model.isHistoryOpen = true }

    /// Greyed out away from the Query tab, where the panel is drawn. Offered
    /// with an empty history on purpose: the panel then says what fills it,
    /// which is the only way a user finds out the feature exists before they
    /// need it.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canShowHistory }
}

/// The Query menu's transaction items, as something a menu can send to.
///
/// One target for the three because they are one subject and their answers are
/// written against the same two facts — whether this connection has a
/// transaction to control, and whether anything is in it. Split across objects,
/// the pair that must never be enabled together could be.
@MainActor
final class TransactionCommands: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func toggleManualCommit(_ sender: Any?) {
        model.setAutocommit(!model.transaction.autocommit)
    }

    @objc func commit(_ sender: Any?) { model.commit() }

    @objc func rollback(_ sender: Any?) { model.rollback() }

    /// The mode item carries a checkmark and stays available with nothing
    /// running; the other two are offered only while there is work to act on.
    ///
    /// Leaving the mode item enabled while a transaction is open is deliberate:
    /// the core refuses that switch, and the refusal names what to do first.
    /// Greying it out would leave someone looking for a mode they can see is on
    /// and cannot find the way out of.
    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        if item.action == #selector(toggleManualCommit(_:)) {
            item.state = model.transaction.autocommit ? .off : .on
            return model.canControlTransactions && !model.isBusy
        }
        return model.hasUncommittedWork && !model.isBusy
    }
}

/// The File menu's export items, as something a menu can send to.
///
/// Swapping the grid for one row read down the page.
///
/// An `NSObject` for the reason every command object here is one: a menu action
/// and `validateMenuItem` both need one, and `AppModel` is `@Observable` and
/// cannot be.
@MainActor
final class RecordCommand: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func toggleRecordView(_ sender: Any?) {
        model.isRecordViewOpen.toggle()
    }

    /// Offered only where there is a row to lay out, and titled for what
    /// pressing it does rather than for the state the window is in — which is
    /// what the value viewer's item beside it does with the same problem.
    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        item.title = model.isRecordViewOpen ? "Show as Grid" : "Show as Record"
        return model.canShowRecord
    }
}

/// An `NSObject` because that is what a menu action and `validateMenuItem` need;
/// `AppModel` is an `@Observable` class and cannot be one. It holds the model
/// rather than reaching for a global, so there is exactly one place the menu
/// learns which result it is exporting.
@MainActor
final class ExportCommands: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc func exportCSV(_ sender: Any?) { present(.csv) }

    @objc func exportTSV(_ sender: Any?) { present(.tsv) }

    @objc func exportJSON(_ sender: Any?) { present(.jsonl) }

    @objc func exportParquet(_ sender: Any?) { present(.parquet) }

    @objc func exportSQL(_ sender: Any?) { present(.sql) }

    /// Writes the saved queries out.
    ///
    /// No format menu and no scope control, unlike the result exports above:
    /// there is one format and there is one list, so both controls would be a
    /// question with a single answer.
    @objc func exportFavorites(_ sender: Any?) {
        guard model.canExportFavorites else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.json]
        panel.nameFieldStringValue = model.favoritesFilename
        let write: (NSApplication.ModalResponse) -> Void = { [model] response in
            guard response == .OK, let url = panel.url else { return }
            model.exportFavorites(to: url)
        }
        if let window = NSApp.keyWindow ?? NSApp.mainWindow {
            panel.beginSheetModal(for: window, completionHandler: write)
        } else {
            panel.begin(completionHandler: write)
        }
    }

    /// Writes the statement log out.
    ///
    /// No scope control, for a reason the export above it does not have: this
    /// panel already has two, and what gets written is whatever they are leaving
    /// on screen.
    @objc func exportStatements(_ sender: Any?) {
        guard model.canExportHistory else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [ExportFormat.sql.contentType]
        panel.nameFieldStringValue = model.historyFilename
        // Said here because the scope is set somewhere this sheet covers up. A
        // person who narrowed the list an hour ago and exports now would
        // otherwise find out from the file.
        panel.message = """
            The statements the history panel is showing are written, newest first. Turn on All, \
            or clear the filter, to write more of them.
            """
        let write: (NSApplication.ModalResponse) -> Void = { [model] response in
            guard response == .OK, let url = panel.url else { return }
            model.exportHistory(to: url)
        }
        if let window = NSApp.keyWindow ?? NSApp.mainWindow {
            panel.beginSheetModal(for: window, completionHandler: write)
        } else {
            panel.begin(completionHandler: write)
        }
    }

    /// Reads a file of saved queries into the list.
    @objc func importFavorites(_ sender: Any?) {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.json]
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        // Said before the file is chosen, because the answer changes which file
        // somebody picks: this adds to the list rather than becoming it.
        panel.message = """
            Queries in this file are added to the ones already saved. A query the file has \
            already been imported from is replaced rather than duplicated.
            """
        let read: (NSApplication.ModalResponse) -> Void = { [model] response in
            guard response == .OK, let url = panel.url else { return }
            model.importFavorites(from: url)
        }
        if let window = NSApp.keyWindow ?? NSApp.mainWindow {
            panel.beginSheetModal(for: window, completionHandler: read)
        } else {
            panel.begin(completionHandler: read)
        }
    }

    /// Asks for a file and reads it into the relation being browsed.
    ///
    /// An open panel and no format menu: the file's extension already says what
    /// it is, and a picker beside it would only offer a way to disagree.
    @objc func importFile(_ sender: Any?) {
        guard let table = model.importTableName else { return }
        let panel = NSOpenPanel()
        panel.allowedContentTypes = ExportFormat.allCases.filter(\.canImport).map(\.contentType)
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        // The two things the window cannot show. Rows landing in a table is not
        // undoable, and a user who learns either of these afterwards learns it
        // from a table that is already different.
        panel.message = """
            Rows are read into \(table), which must already exist — no table is created and no \
            column is added. An import that fails part way leaves behind the rows it had already \
            written.
            """

        let read: (NSApplication.ModalResponse) -> Void = { [model] response in
            guard response == .OK, let url = panel.url else { return }
            model.importFile(from: url)
        }
        // A sheet, so the panel is attached to the table it is describing. The
        // free-standing fallback covers the window not being key.
        if let window = NSApp.keyWindow ?? NSApp.mainWindow {
            panel.beginSheetModal(for: window, completionHandler: read)
        } else {
            panel.begin(completionHandler: read)
        }
    }

    /// Greys the items out while there is nothing to write, so the panel is
    /// never opened for a result that does not exist yet.
    ///
    /// Import asks a different question: it needs a table to read into, not a
    /// result to read out of, and those are true at different times.
    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        switch item.action {
        case #selector(importFile(_:)): return model.canImport
        case #selector(exportFavorites(_:)): return model.canExportFavorites
        case #selector(exportStatements(_:)): return model.canExportHistory
        // Always live. Reading a file in needs nothing to already be there —
        // an empty list is exactly when somebody imports one.
        case #selector(importFavorites(_:)): return true
        default: return model.canExport
        }
    }

    /// The scope control, kept alive between building the panel and reading the
    /// answer out of it. An `NSSavePanel` accessory view is not retained by
    /// anything else the completion handler can see.
    private var scopeControl: NSSegmentedControl?

    private func present(_ format: ExportFormat) {
        guard model.canExport else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [format.contentType]
        // Asked before the name is proposed, because the name says which scope
        // it is — a file called `-first-200000-rows` holding the whole table is
        // the same lie the suffix exists to prevent, pointing the other way.
        let control = model.exportScopeIsAChoice ? scopeChooser() : nil
        scopeControl = control
        panel.accessoryView = control.map(scopeAccessory)
        panel.nameFieldStringValue = model.exportFilename(format, scope: scope(from: control))
        // The panel is the last surface between the user and a file that will
        // be read without them, so it is where the result gets described.
        panel.message = model.exportMessage
        panel.canCreateDirectories = true

        let write: (NSApplication.ModalResponse) -> Void = { [model, weak self] response in
            let chosen = self?.scope(from: self?.scopeControl) ?? .wholeResult
            self?.scopeControl = nil
            guard response == .OK, let url = panel.url else { return }
            model.exportCurrentResult(to: url, format: format, scope: chosen)
        }
        // A sheet, so the panel is attached to the result it is describing.
        // The free-standing fallback covers the window not being key, which is
        // the case a headless run reaches.
        if let window = NSApp.keyWindow ?? NSApp.mainWindow {
            panel.beginSheetModal(for: window, completionHandler: write)
        } else {
            panel.begin(completionHandler: write)
        }
    }

    /// The two answers, with the cheap one selected.
    ///
    /// The rows already on screen are the default because they are what the
    /// window is showing and they cost nothing more to write. Re-reading a
    /// large table is a minutes-long operation, and defaulting somebody into
    /// one they did not ask for is the worse mistake of the two.
    private func scopeChooser() -> NSSegmentedControl {
        let rows = model.current.rowCount
        let control = NSSegmentedControl(
            labels: [
                "The \(AppModel.formatted(rows)) rows here", "The whole result"
            ], trackingMode: .selectOne, target: nil, action: nil)
        control.selectedSegment = 0
        return control
    }

    private func scope(from control: NSSegmentedControl?) -> ExportScope {
        guard let control else { return .wholeResult }
        return control.selectedSegment == 1
            ? .wholeResult : .firstRows(model.current.rowCount)
    }

    /// Wraps the control with a label, because a bare segmented control above a
    /// file name is a pair of buttons with no question attached to them.
    private func scopeAccessory(_ control: NSSegmentedControl) -> NSView {
        let label = NSTextField(labelWithString: "Rows to write:")
        let stack = NSStackView(views: [label, control])
        stack.orientation = .horizontal
        stack.spacing = 8
        stack.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        return stack
    }
}
