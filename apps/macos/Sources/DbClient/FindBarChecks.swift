import AppKit
import SwiftUI

/// Executable checks for the editor's find bar, run by `--verify-find-bar`.
///
/// The bar itself is AppKit's — the case-insensitive contains matching, the
/// match count, the wrap-around — and restating any of that here would be a
/// second implementation of a search nobody wrote. What can go wrong quietly is
/// the wiring on this side, and it fails in ways no compiler sees: the four
/// menu items share one selector and mean four different commands only through
/// their tags, so a wrong tag is "Find Next" walking backwards; a target set on
/// any of them would pin the command to one view instead of the focused one;
/// and a text view that never said `usesFindBar` answers ⌘F with the floating
/// find *panel*, which works just well enough that nobody files the bug.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
@MainActor
enum FindBarChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheEditMenuBindsTheFourFindCommands()
        checkTheEditorWearsTheSystemFindBar()
        if failures == 0 {
            fputs("find-bar: all checks passed\n", stderr)
        } else {
            fputs("find-bar: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The Edit menu carries all four find commands, on the platform's own
    /// keys, each with the tag that names which one it is.
    ///
    /// The tag is the whole meaning: `performFindPanelAction(_:)` reads it at
    /// dispatch time and the compiler checks nothing about it. The target has
    /// to be nil on every one, because a target would stop the responder chain
    /// from finding the focused text view — which is both how the commands
    /// reach the editor and how they go quiet while the grid has the keyboard.
    private static func checkTheEditMenuBindsTheFourFindCommands() {
        let items = (AppMenu.editMenu().submenu?.items ?? []).filter {
            $0.action == #selector(NSTextView.performFindPanelAction(_:))
        }
        expect(
            items.map(describe),
            [
                "Find… ⌘f #\(NSFindPanelAction.showFindPanel.rawValue)",
                "Find Next ⌘g #\(NSFindPanelAction.next.rawValue)",
                "Find Previous ⇧⌘g #\(NSFindPanelAction.previous.rawValue)",
                "Use Selection for Find ⌘e #\(NSFindPanelAction.setFindString.rawValue)"
            ],
            "the four find commands, on their platform keys, with the tags AppKit dispatches by")
        expect(
            items.allSatisfy { $0.target == nil }, true,
            "no item names a target, so the responder chain picks the focused text view")
    }

    /// The editor's text view asks for the find bar, not the find panel.
    ///
    /// Both answer ⌘F, which is what makes the difference invisible in a menu
    /// check: the panel is a floating window that covers the editor it is
    /// searching and knows nothing about incremental matching. Checked through
    /// the same SwiftUI wrapper the window uses, because the flag is set in
    /// `makeNSView` and a hand-built text view would check a copy of the claim.
    private static func checkTheEditorWearsTheSystemFindBar() {
        let hosting = NSHostingView(
            rootView: SQLEditor(
                text: .constant("select 1"), selection: .constant(nil), scheme: "postgres",
                fontSize: 13,
                typing: EditorTyping.Rules(
                    tabWidth: 4, softTabs: false, autoIndent: true, autoPairs: true,
                    uppercasesKeywords: false),
                offers: { _, _, _ in }))
        hosting.frame = NSRect(x: 0, y: 0, width: 400, height: 200)
        hosting.layoutSubtreeIfNeeded()
        guard let textView = textView(under: hosting) else {
            fail("the hosted editor holds its text view once laid out")
            return
        }
        expect(textView.usesFindBar, true, "⌘F opens the bar inside the scroll view")
        expect(
            textView.isIncrementalSearchingEnabled, true,
            "and matches highlight while the search is still being typed")
    }

    // MARK: - Harness

    private static func textView(under view: NSView) -> EditorTextView? {
        if let found = view as? EditorTextView { return found }
        for child in view.subviews {
            if let found = textView(under: child) { return found }
        }
        return nil
    }

    /// One line per item — title, modifiers, key, tag — because that is the
    /// form a failure has to be read in: four items differing in one number.
    private static func describe(_ item: NSMenuItem) -> String {
        var keys = ""
        if item.keyEquivalentModifierMask.contains(.control) { keys += "⌃" }
        if item.keyEquivalentModifierMask.contains(.option) { keys += "⌥" }
        if item.keyEquivalentModifierMask.contains(.shift) { keys += "⇧" }
        if item.keyEquivalentModifierMask.contains(.command) { keys += "⌘" }
        return "\(item.title) \(keys)\(item.keyEquivalent) #\(item.tag)"
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("find-bar FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }

    private static func fail(_ what: String) {
        failures += 1
        fputs("find-bar FAIL: \(what)\n", stderr)
    }
}
