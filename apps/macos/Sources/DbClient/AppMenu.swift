import AppKit

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
    /// Target of the View menu's Refresh item, held for the same reason.
    private static var refreshCommand: RefreshCommand?
    /// Target of the View menu's value-viewer item, held for the same reason.
    private static var valueViewerCommand: ValueViewerCommand?
    /// Target of the View menu's object-filter item, held for the same reason.
    private static var navigatorCommand: NavigatorCommand?
    /// Target of the Query menu's items, held for the same reason.
    private static var queryCommands: QueryCommands?

    @MainActor
    static func install(into app: NSApplication, model: AppModel) {
        // `CFBundleName` when running as a bundle, which is what the menu should
        // say; the process name is the fallback for the unbundled dev binary.
        let name =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String
            ?? ProcessInfo.processInfo.processName
        let commands = ExportCommands(model: model)
        exportCommands = commands
        let refresh = RefreshCommand(model: model)
        refreshCommand = refresh
        let valueViewer = ValueViewerCommand(model: model)
        valueViewerCommand = valueViewer
        let navigator = NavigatorCommand(model: model)
        navigatorCommand = navigator
        let query = QueryCommands(model: model)
        queryCommands = query
        let main = NSMenu()
        main.addItem(appMenu(named: name))
        main.addItem(fileMenu(target: commands))
        main.addItem(editMenu())
        main.addItem(viewMenu(target: refresh, valueViewer: valueViewer, navigator: navigator))
        main.addItem(queryMenu(target: query))
        main.addItem(windowMenu(for: app))
        app.mainMenu = main
    }

    private static func appMenu(named name: String) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu()
        menu.addItem(
            withTitle: "About \(name)",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: "")
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

    /// Getting a result out of the window.
    ///
    /// ⌘C is the only other way, and it goes to the pasteboard — which means
    /// anything larger than what the next application will accept as a paste is
    /// stuck here. ⇧⌘E rather than ⌘S: nothing in this window is a document
    /// with unsaved changes, and binding Save to something that is not one
    /// teaches the wrong reflex.
    ///
    /// Two items rather than one item with a format popup in the panel's
    /// accessory view: the popup is a control nobody looks for, and the menu is
    /// where a user goes to find out what an application can do.
    private static func fileMenu(target: ExportCommands) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "File")

        let csv = menu.addItem(
            withTitle: "Export Result as CSV…",
            action: #selector(ExportCommands.exportCSV(_:)), keyEquivalent: "e")
        csv.keyEquivalentModifierMask = [.command, .shift]
        csv.target = target

        let tsv = menu.addItem(
            withTitle: "Export Result as TSV…",
            action: #selector(ExportCommands.exportTSV(_:)), keyEquivalent: "")
        tsv.target = target

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
    private static func viewMenu(
        target: RefreshCommand, valueViewer: ValueViewerCommand, navigator: NavigatorCommand
    ) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "View")
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

        menu.addItem(.separator())
        // Titled for the closed state; `validateMenuItem` rewrites it.
        let value = menu.addItem(
            withTitle: "Show Value in Full",
            action: #selector(ValueViewerCommand.toggleValueViewer(_:)), keyEquivalent: "v")
        value.keyEquivalentModifierMask = [.command, .option]
        value.target = valueViewer

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
    private static func queryMenu(target: QueryCommands) -> NSMenuItem {
        let item = NSMenuItem()
        let menu = NSMenu(title: "Query")
        let script = menu.addItem(
            withTitle: "Run Script",
            action: #selector(QueryCommands.runScript(_:)), keyEquivalent: "r")
        script.keyEquivalentModifierMask = [.command, .option]
        script.target = target
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

    @objc func toggleValueViewer(_ sender: Any?) { model.isValueViewerOpen.toggle() }

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

/// The File menu's export items, as something a menu can send to.
///
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

    /// Greys both items out while there is nothing to write, so the panel is
    /// never opened for a result that does not exist yet.
    func validateMenuItem(_ item: NSMenuItem) -> Bool { model.canExport }

    private func present(_ format: DelimitedFormat) {
        guard model.canExport else { return }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [format.contentType]
        panel.nameFieldStringValue = model.exportFilename(format)
        // The panel is the last surface between the user and a file that will
        // be read without them, so it is where the result gets described.
        panel.message = model.exportMessage
        panel.canCreateDirectories = true

        let write: (NSApplication.ModalResponse) -> Void = { [model] response in
            guard response == .OK, let url = panel.url else { return }
            model.exportCurrentResult(to: url, format: format)
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
}
