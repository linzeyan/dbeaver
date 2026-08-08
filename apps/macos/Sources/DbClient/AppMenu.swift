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

    @MainActor
    static func install(into app: NSApplication, model: AppModel) {
        // `CFBundleName` when running as a bundle, which is what the menu should
        // say; the process name is the fallback for the unbundled dev binary.
        let name =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String
            ?? ProcessInfo.processInfo.processName
        let commands = ExportCommands(model: model)
        exportCommands = commands
        let main = NSMenu()
        main.addItem(appMenu(named: name))
        main.addItem(fileMenu(target: commands))
        main.addItem(editMenu())
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
