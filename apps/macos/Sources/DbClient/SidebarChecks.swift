import Foundation

/// Executable checks for the collapsed sidebar, run by `--verify-sidebar`.
///
/// What is pinned here is the two ways a rail can strand somebody: a command
/// that focuses a field the rail does not have, and a rail drawn over the one
/// state where the left column is not the object tree. Neither fails to
/// compile, and neither is visible in the state a capture opens on.
///
/// What the rail draws is not pinned and cannot be from here: that needs a
/// window, which is what `--collapse-sidebar` is for.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum SidebarChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        MainActor.assumeIsolated {
            checkFilteringObjectsPutsTheTreeBack()
            checkFilteringObjectsStillAsksWhenTheTreeIsAlreadyThere()
            checkTheRailIsNotDrawnOverTheForm()
        }
        if failures == 0 {
            fputs("sidebar: all checks passed\n", stderr)
        } else {
            fputs("sidebar: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// ⌥⌘F against a rail has to bring the tree back, because the field it puts
    /// the caret in is in the tree. Without this the command is a keystroke that
    /// moves focus to a control which is not on screen — nothing visibly
    /// happens, and the next thing typed goes wherever focus actually was.
    @MainActor private static func checkFilteringObjectsPutsTheTreeBack() {
        guard let model = makeModel() else { return }
        model.wantsSidebarRail = true
        model.focusNavigatorFilter()
        expect(model.wantsSidebarRail, false, "the tree is back")
        expect(model.filterFocusRequests, 1, "and the caret was asked for")
    }

    /// The other half of the same command: putting the tree back must not have
    /// replaced asking for the caret. A version that only un-collapsed would
    /// leave ⌥⌘F doing nothing at all in the state it is used in most.
    @MainActor private static func checkFilteringObjectsStillAsksWhenTheTreeIsAlreadyThere() {
        guard let model = makeModel() else { return }
        model.focusNavigatorFilter()
        model.focusNavigatorFilter()
        expect(model.filterFocusRequests, 2, "each press asks again")
        expect(model.wantsSidebarRail, false, "and the tree stayed")
    }

    /// A tab with nothing open on it shows the saved connections in the left
    /// column, and those are the whole of how that tab gets used. A rail over
    /// them would be a window whose only way forward is 44pt of chrome that
    /// filters and refreshes an object tree it does not have.
    ///
    /// The state is kept rather than cleared: somebody who collapsed the tree,
    /// opened a second connection and came back should find it as they left it.
    @MainActor private static func checkTheRailIsNotDrawnOverTheForm() {
        guard let model = makeModel() else { return }
        expect(model.isShowingConnectionForm, true, "a model with no database shows the form")
        model.wantsSidebarRail = true
        expect(model.isSidebarCollapsed, false, "the rail is not drawn over it")
        expect(model.canToggleSidebar, false, "and the menu item is greyed out")
        model.toggleSidebar()
        expect(model.wantsSidebarRail, true, "the item that is greyed out does nothing")
    }

    // MARK: - Fixture

    /// A model on scratch stores throughout, with the config redirected.
    ///
    /// The redirect is not optional: without it the model reads the user's saved
    /// connections and asks the Keychain for the first one's password, which in
    /// a process with no GUI session blocks forever — so the symptom is not a
    /// failed check but a `make test-swift` that never returns.
    @MainActor private static func makeModel() -> AppModel? {
        guard let directory = scratchDirectory() else { return nil }
        setenv("XDG_CONFIG_HOME", directory.path, 1)
        return AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-sidebar")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-sidebar")),
            preferences: Preferences(store: ScratchDefaults.store("verify-sidebar")))
    }

    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-sidebar-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            failures += 1
            fputs("sidebar FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("sidebar FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
