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
        checkASchemaOfNothingButSequencesIsDrawn()
        checkOnlyOneNonRelationIsEverSelected()
        checkASequenceCarriesItsNumbersAcross()
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
            model.sessions[0].schemas = [schema("util")]
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
            model.sessions[0].schemas = [schema("public")]
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
            model.sessions[0].schemas = [schema("public")]
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
            model.sessions[0].schemas = [schema("public")]
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
            model.sessions[0].schemas = [schema("public")]
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
            model.sessions[0].schemas = [schema("public")]
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

    /// The schema-row condition again, one kind further on. It was widened once
    /// for the routines and would have been wrong a second time by the same
    /// omission — a schema holding only sequences drawn by no branch at all.
    private static func checkASchemaOfNothingButSequencesIsDrawn() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.sessions[0].schemas = [schema("counters")]
            model.sessions[0].sequences = ["counters": [sequence("order_id_seq")]]
            expect(model.hasVisibleObjects(in: "counters"), true, "a schema of sequences is drawn")
            expect(model.totalObjectCount, 1, "and its one sequence is one object")

            model.navigatorFilter = "order"
            expect(
                model.visibleSequences(in: "counters").count, 1,
                "the filter reaches the sequences too")
            expect(
                model.isSequenceGroupExpanded("counters"), true,
                "and opens the group holding what matched")
        }
    }

    /// The invariant `navigatorSelection` exists to hold: `selectedRoutine` and
    /// `selectedSequence` are two properties and at most one of them is set.
    /// Both at once would put two panes' worth of claim on screen, and which one
    /// won would be whichever branch the view happened to test first.
    private static func checkOnlyOneNonRelationIsEverSelected() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.navigatorSelection = .routine(routine("settle"))
            expect(model.selectedSequence == nil, true, "a routine alone")

            model.navigatorSelection = .sequence(sequence("order_id_seq"))
            expect(model.selectedRoutine == nil, true, "and picking a sequence lets it go")
            expect(model.selectedSequence?.name, "order_id_seq", "leaving the sequence")
            expect(model.showsNonRelation, true, "which is still not a relation")

            model.navigatorSelection = .routine(routine("settle"))
            expect(model.selectedSequence == nil, true, "and back the other way")

            model.navigatorSelection = .relation(relation("orders"))
            expect(model.showsNonRelation, false, "a relation clears both")
        }
    }

    /// Every number a sequence shows is a string the server rendered, so a
    /// field reading its neighbour typechecks. The fixture is deliberately one
    /// where no two of them are equal.
    private static func checkASequenceCarriesItsNumbersAcross() {
        MainActor.assumeIsolated {
            let decoded: SequenceInfo? = decode(
                #"""
                {"schema":"public","name":"bench_batch_seq","last_value":null,
                 "increment":"10","min_value":"100","max_value":"900",
                 "cycles":true,"cache":"5"}
                """#)
            expect(decoded?.increment, "10", "the step is the step")
            expect(decoded?.minValue, "100", "the floor is the floor")
            expect(decoded?.maxValue, "900", "and the ceiling is not the floor")
            expect(decoded?.cycles, true, "cycling crosses as itself")
            expect(decoded?.cache, "5", "and so does the cache")
            expect(
                decoded?.lastValue == nil, true,
                "a null last value stays nil rather than becoming a zero somebody would read")
            expect(decoded?.range, "100 … 900", "and the range is written from both ends")

            // A key the core stopped writing is refused rather than defaulted,
            // for the reason `MetadataChecks` refuses one: a default here is an
            // answer nobody gave, and "increment 0" is a sequence that stands
            // still.
            let renamed: SequenceInfo? = decode(
                #"""
                {"schema":"public","name":"s","last_value":"1","increment_by":"1",
                 "min_value":"1","max_value":"9","cycles":false,"cache":null}
                """#)
            expect(renamed == nil, true, "a renamed key is not guessed at")
        }
    }

    // MARK: - Helpers

    private static func decode<T: Decodable>(_ json: String) -> T? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    private static func sequence(_ name: String) -> SequenceInfo {
        SequenceInfo(
            schema: "public", name: name, lastValue: "41", increment: "1", minValue: "1",
            maxValue: "9223372036854775807", cycles: false, cache: "1")
    }

    /// A schema of somebody's own. The system ones have a check of their own
    /// below, which is the only place that wants the other answer.
    private static func schema(_ name: String) -> SchemaInfo {
        SchemaInfo(name: name, isSystem: false)
    }

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
