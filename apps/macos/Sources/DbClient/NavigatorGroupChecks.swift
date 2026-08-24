import Foundation

/// What the navigator does now that a schema holds two kinds of object.
///
/// The tree used to be one list per schema, and every question it answered was
/// about relations: whether to draw the schema at all, what the count beside it
/// means, what the filter narrows, what a selection is. Each of those had one
/// answer and now has two, and the ones worth a check here are the ones whose
/// wrong answer is invisible — a schema silently missing from the tree, a
/// selected function quietly replaced by the table it was reached from.
///
/// Behind a flag on the binary for the reason `SchemaMetadataChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum NavigatorGroupChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkASchemaHoldingOnlyRoutinesIsStillDrawn()
        checkTheCountsAddUpBothKinds()
        checkTheFilterReachesTheRoutines()
        checkSelectingARoutineLeavesTheRelationAlone()
        checkDeselectingARoutineGoesBackToTheRelation()
        checkContentGivesWayToStructureAndQueryDoesNot()
        checkTheSourceIsDroppedWhenTheSelectionMoves()
        checkAFilterThatHidesTheSelectedRoutineDoesNotClearIt()
        checkARefreshedTreeDoesNotReopenGroupsUnderSchemasThatAreGone()
        checkTheChromeDescribesWhicheverObjectIsShowing()
        if failures == 0 {
            fputs("navigator-groups: all checks passed\n", stderr)
        } else {
            fputs("navigator-groups: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The tree

    /// The condition that used to read `!relations.isEmpty`.
    ///
    /// A schema of nothing but functions — the shape every `*_util` schema has —
    /// was drawn by no branch of the navigator at all. Not an empty row: no row,
    /// which reads as a login that cannot see the schema rather than as one whose
    /// contents are all code.
    private static func checkASchemaHoldingOnlyRoutinesIsStillDrawn() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].schemas = [SchemaInfo(name: "util")]
            model.sessions[0].routines = ["util": [routine("slugify")]]
            expect(model.hasVisibleObjects(in: "util"), true, "a schema of functions is drawn")
            expect(
                model.visibleRelations(in: "util").isEmpty, true,
                "and it genuinely has no relations, which is what used to hide it")
        }
    }

    /// The figure the sidebar footer writes beside the word "object".
    ///
    /// Both kinds, because there is one tree and one word for what is in it. A
    /// count that left the functions out would disagree with the rows somebody
    /// can see, and the empty states read the same number — so a schema whose
    /// functions matched would be told "No matches" over a list of them.
    private static func checkTheCountsAddUpBothKinds() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].relations = ["public": [relation("orders"), relation("invoices")]]
            model.sessions[0].routines = ["public": [routine("settle"), routine("slugify")]]
            expect(model.totalObjectCount, 4, "two tables and two functions are four objects")
            expect(model.matchedObjectCount, 4, "and with no filter, all four are showing")
        }
    }

    /// One field, one rule. The filter narrowed the relations and left every
    /// routine in place, so typing a table's name answered with that table and
    /// the whole catalogue of functions beside it.
    private static func checkTheFilterReachesTheRoutines() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].relations = ["public": [relation("orders")]]
            model.sessions[0].routines = ["public": [routine("settle"), routine("slugify")]]

            model.navigatorFilter = "slug"
            expect(
                model.visibleRoutines(in: "public").map(\.name), ["slugify"],
                "a needle narrows the functions the way it narrows the tables")
            expect(model.visibleRelations(in: "public").isEmpty, true, "and the tables with them")
            expect(model.matchedObjectCount, 1, "one object matched")
            expect(
                model.isRoutineGroupExpanded("public"), true,
                "and the group holding it opens, since a match nobody can see is not a match")
            expect(
                model.expandedRoutineGroups.isEmpty, true,
                "without writing the arrangement the field has to hand back untouched")

            // The schema's own name keeps everything under it, which is the rule
            // `visibleRelations` follows: typing "public" is asking to see the
            // schema, not the subset of its objects that repeat its name.
            model.navigatorFilter = "public"
            expect(
                model.visibleRoutines(in: "public").count, 2,
                "a schema that matched keeps all of its functions")
        }
    }

    // MARK: - The selection

    /// The reason `selectedRoutine` sits beside `selected` instead of replacing
    /// it: a table carries browsed rows, a paging position and a filter bar, and
    /// glancing at a function should not be what throws them away.
    private static func checkSelectingARoutineLeavesTheRelationAlone() {
        MainActor.assumeIsolated {
            let model = makeModel()
            let orders = relation("orders")
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].relations = ["public": [orders]]
            model.sessions[0].selected = orders

            model.navigatorSelection = .routine(routine("settle"))
            expect(model.selectedRoutine?.name, "settle", "the routine is what is selected")
            expect(model.selected?.name, "orders", "and the table under it is still there")
            expect(
                model.navigatorSelection, .routine(routine("settle")),
                "the highlighted row is the routine, not the table it was reached from")
        }
    }

    /// What a `List` writing nil means when a routine is showing. Clearing both
    /// would leave the panes describing nothing while a perfectly good table sits
    /// one property away.
    private static func checkDeselectingARoutineGoesBackToTheRelation() {
        MainActor.assumeIsolated {
            let model = makeModel()
            let orders = relation("orders")
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].relations = ["public": [orders]]
            model.sessions[0].selected = orders
            model.navigatorSelection = .routine(routine("settle"))

            model.navigatorSelection = nil
            expect(model.selectedRoutine == nil, true, "the routine is let go of")
            expect(model.navigatorSelection, .relation(orders), "and the table comes back")

            model.navigatorSelection = nil
            expect(model.selected == nil, true, "a second nil is the one that clears the table")
        }
    }

    /// Content is rows of a relation, and a routine has none. Query is a
    /// statement somebody is in the middle of typing.
    private static func checkContentGivesWayToStructureAndQueryDoesNot() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].activeTab = .content
            model.navigatorSelection = .routine(routine("settle"))
            expect(
                model.activeTab, .structure,
                "picking a routine off the Content tab moves to the pane that can show one")

            let other = makeModel()
            other.sessions[0].activeTab = .query
            other.navigatorSelection = .routine(routine("settle"))
            expect(
                other.activeTab, .query,
                "and the editor is left alone, because looking up a signature is not a reason "
                    + "to move somebody out of a statement they are writing")
        }
    }

    /// A stale body under a fresh name is the one failure here that looks like
    /// working software.
    private static func checkTheSourceIsDroppedWhenTheSelectionMoves() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.navigatorSelection = .routine(routine("settle"))
            model.sessions[0].routineSource = "CREATE FUNCTION settle…"
            expect(model.routineSource != nil, true, "a source that arrived is held")

            model.navigatorSelection = .routine(routine("slugify"))
            expect(
                model.routineSource == nil, true,
                "and is dropped for the next routine rather than shown under its name")

            model.sessions[0].routineSource = "CREATE FUNCTION slugify…"
            model.navigatorSelection = .relation(relation("orders"))
            expect(model.routineSource == nil, true, "and dropped again on the way back to a table")
        }
    }

    /// The guard `filterHidesSelection` exists for, now that the thing being
    /// hidden can be a routine. Without it, typing in the filter field is what
    /// closes the pane describing the function — a field that changes what the
    /// window says about an object rather than which objects are listed.
    private static func checkAFilterThatHidesTheSelectedRoutineDoesNotClearIt() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].schemas = [SchemaInfo(name: "public")]
            model.sessions[0].routines = ["public": [routine("settle"), routine("slugify")]]
            model.navigatorSelection = .routine(routine("settle"))

            model.navigatorFilter = "slug"
            expect(
                model.filterHidesSelection, true,
                "the selected routine is not among the rows the list is showing")
            model.navigatorSelection = nil
            expect(
                model.selectedRoutine?.name, "settle",
                "so the nil the list writes is the list disowning a row, not a deselection")
        }
    }

    /// The group's arrangement is intersected with the schemas a refresh read,
    /// for the reason `expanded` is: a schema dropped and recreated should not
    /// come back with a group nobody on this connection ever opened.
    private static func checkARefreshedTreeDoesNotReopenGroupsUnderSchemasThatAreGone() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.expandedRoutineGroups = ["util", "public"]
            model.expandedRoutineGroups.formIntersection(["public"])
            expect(
                model.expandedRoutineGroups, ["public"],
                "the group under a schema that is gone is forgotten with it")
        }
    }

    /// The line under the window's title. It read `selected` on its own, so a
    /// selected function was described as "Materialized View · public" — the
    /// table it had been reached from, describing the thing that replaced it.
    private static func checkTheChromeDescribesWhicheverObjectIsShowing() {
        MainActor.assumeIsolated {
            let model = makeModel()
            expect(model.objectSubtitle, "", "nothing selected says nothing")

            model.sessions[0].selected = RelationInfo(
                schema: "public", name: "totals", kind: .materializedView, estimatedRows: nil)
            expect(
                model.objectSubtitle, "Materialized View · public",
                "a relation is described by its own kind")

            model.navigatorSelection = .routine(routine("settle"))
            expect(
                model.objectSubtitle, "Function · public",
                "and a routine by its, not by the kind of the table underneath it")
        }
    }

    // MARK: - Helpers

    private static func relation(_ name: String) -> RelationInfo {
        RelationInfo(schema: "public", name: name, kind: .table, estimatedRows: nil)
    }

    /// The id is the name here. A real one is a driver's own opaque token, and
    /// nothing in these checks hands it back over the FFI — `crates/ffi`'s
    /// conformance suite is where the round trip is pinned.
    private static func routine(_ name: String) -> RoutineInfo {
        RoutineInfo(
            schema: "public", name: name, kind: .function, id: name,
            arguments: "uuid", returns: "numeric", language: "plpgsql")
    }

    /// A model over throwaway suites, built the way `BrowseRestoreChecks` builds
    /// its own: running the checks must not read or write the defaults the user's
    /// windows share.
    @MainActor private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-navigator-groups"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-navigator-groups"))
        return AppModel(
            history: history, favorites: favorites,
            preferences: Preferences(store: ScratchDefaults.store("verify-navigator-groups")))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("navigator-groups FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
