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
            checkTheEnginesOwnSchemasAreHiddenUntilAskedFor()
            checkTheCountsAndGoToFollowTheSetting()
        }
        if failures == 0 {
            fputs("sidebar: all checks passed\n", stderr)
        } else {
            fputs("sidebar: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The default is off, and turning it on shows what was already fetched.
    ///
    /// The second half is the design being pinned. `schemas` holds everything
    /// the driver reported and `visibleSchemas` narrows it, so the setting takes
    /// effect at once — an arrangement that would be indistinguishable from
    /// filtering at the fetch until somebody flipped the switch and waited for a
    /// reconnect that never came.
    @MainActor private static func checkTheEnginesOwnSchemasAreHiddenUntilAskedFor() {
        guard let model = makeModel() else { return }
        model.sessions[0].schemas = [
            SchemaInfo(name: "public", isSystem: false),
            SchemaInfo(name: "pg_catalog", isSystem: true)
        ]

        expect(
            model.visibleSchemas.map(\.name), ["public"],
            "the tree opens on the schemas somebody made, not on the server's")
        expect(
            model.schemas.count, 2,
            "and both are held, because hiding is not the same as never having asked")

        model.preferences.showsSystemSchemas = true
        expect(
            model.visibleSchemas.map(\.name), ["public", "pg_catalog"],
            "turning the setting on draws what was already fetched, with no reconnect")

        model.preferences.showsSystemSchemas = false
        expect(model.visibleSchemas.count, 1, "and turning it off puts it back")
    }

    /// Everything that walks the schema list has to walk the same one.
    ///
    /// The count in the footer and the Go To palette read the object
    /// dictionaries, which are keyed by every schema the driver reported. Left
    /// to read those directly they would say "4 objects" over a tree drawing
    /// two, and ⇧⌘O would offer a table the tree does not list.
    @MainActor private static func checkTheCountsAndGoToFollowTheSetting() {
        guard let model = makeModel() else { return }
        model.sessions[0].schemas = [
            SchemaInfo(name: "public", isSystem: false),
            SchemaInfo(name: "pg_catalog", isSystem: true)
        ]
        model.sessions[0].relations = [
            "public": [
                RelationInfo(schema: "public", name: "orders", kind: .table, estimatedRows: nil)
            ],
            "pg_catalog": [
                RelationInfo(
                    schema: "pg_catalog", name: "pg_class", kind: .table, estimatedRows: nil)
            ]
        ]

        expect(model.totalObjectCount, 1, "the footer counts what the tree draws")
        expect(model.matchedObjectCount, 1, "and so does the figure the filter narrows")
        expect(
            model.goToTargets.contains { $0.name == "pg_class" }, false,
            "and Go To does not offer a table the tree is not listing")

        model.preferences.showsSystemSchemas = true
        expect(model.totalObjectCount, 2, "asking for them adds them to the count")
        expect(
            model.goToTargets.contains { $0.name == "pg_class" }, true,
            "and puts them within reach of the palette")
    }

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
