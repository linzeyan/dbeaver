import Foundation

/// Executable checks for the filter rows, run by `--verify-filter-rows`.
///
/// What is pinned here is the rule the rows exist under and nothing else: they
/// and the Custom field are two ways of saying one thing, the browse sends
/// exactly one of them, and each table keeps its own. The compiling is not here
/// — that is `dbedit`'s, where a test walks every operator a column is offered
/// and writes each one, and a model with no connection cannot reach it. The one
/// case below that needs a compiled clause writes one by hand and says so.
///
/// No database is needed. `AppModel.run` returns without dispatching when there
/// is no connection, so selecting a relation performs the state half of
/// `selectionChanged` and none of the round trips.
enum FilterRowChecks {
    private static var failures = 0

    static func run() -> Bool {
        // Point the config at a scratch directory before building a model, the
        // way `BrowseRestoreChecks` does. Without it the model reads the user's
        // saved connections and asks the Keychain for the first one's password,
        // which blocks forever in a process with no GUI session — so the symptom
        // is not a failed check but a `make test-swift` that never returns.
        guard let scratch = scratchDirectory() else { return false }
        defer { try? FileManager.default.removeItem(at: scratch) }
        setenv("XDG_CONFIG_HOME", scratch.path, 1)

        failures = 0
        defer { ScratchDefaults.release() }
        checkARowTakesTheCustomFieldAway()
        checkTheLastRowLeavingGivesItBack()
        checkTheCustomFieldIsWhatIsSentWithNoRows()
        checkRowsComeBackWithTheirTable()
        checkTablesDoNotShareRows()
        checkAStateHoldingOnlyRowsIsWorthKeeping()
        checkAnIndexOutOfRangeChangesNothing()
        checkAnOperatorTheColumnCannotAnswerFallsBack()
        checkAnOperatorTheColumnCanAnswerIsLeftAlone()
        checkComparingAgainstNothingCarriesNothing()
        checkOnlyARangeKeepsItsFarEnd()
        checkAColumnNobodyKnowsIsLeftAlone()
        if failures == 0 {
            fputs("filter-rows: all checks passed\n", stderr)
        } else {
            fputs("filter-rows: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The rule the plan states outright: no merged half-SQL. A row arriving
    /// takes the field with it, because a filter still on screen but no longer
    /// sent is the one that gets blamed for the row count.
    private static func checkARowTakesTheCustomFieldAway() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.whereClause = "id > 10"
            model.addFilterRule(FilterRule(column: "qty", op: .greaterThan, value: "5"))
            expect(model.whereClause, "", "adding a row empties the Custom field")
            expect(model.isCustomFilterEditable, false, "and stops it being typed into")
        }
    }

    /// And gives it back. The rows are the easy way in and the field is the
    /// escape hatch; one that stays locked after the rows are gone is not one.
    private static func checkTheLastRowLeavingGivesItBack() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.addFilterRule(FilterRule(column: "qty", op: .isNull))
            model.removeFilterRule(at: 0)
            expect(model.isCustomFilterEditable, true, "the last row leaving unlocks the field")
            expect(model.compiledClause, "", "and takes the clause it compiled to with it")
        }
    }

    /// With no rows the field is what goes to the server, but for the whitespace
    /// the browse has always trimmed off it.
    private static func checkTheCustomFieldIsWhatIsSentWithNoRows() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.whereClause = "  id > 10  "
            expect(model.browsePredicate, "id > 10", "with no rows the field is the predicate")
        }
    }

    /// Rows are per-table state, the way the WHERE field became in 2.1. A→B→A is
    /// the loop this item exists to shorten, and rebuilding four rows by hand
    /// each time round would be worse than re-typing the SQL was.
    private static func checkRowsComeBackWithTheirTable() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            model.addFilterRule(FilterRule(column: "qty", op: .greaterThan, value: "5"))
            model.selected = regions
            expect(model.filterRules.count, 0, "a table opened for the first time has no rows")
            model.selected = orders
            expect(model.filterRules.count, 1, "coming back brings them")
            expect(model.filterRules.first?.column, "qty", "and they are the ones that were left")
        }
    }

    /// Each table keeps its own. One table's rows drawn over another's columns
    /// name columns it does not have, which is the mistake the old clearing
    /// prevented and which this must not reintroduce.
    private static func checkTablesDoNotShareRows() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.selected = orders
            model.addFilterRule(FilterRule(column: "qty", op: .isNull))
            model.selected = regions
            model.addFilterRule(FilterRule(column: "code", op: .isNotNull))
            model.selected = orders
            expect(model.filterRules.first?.column, "qty", "the first table's rows are its own")
            model.selected = regions
            expect(model.filterRules.first?.column, "code", "and so are the second's")
        }
    }

    /// The store drops a state with nothing in it, so rows had to start counting
    /// as something. Without this a table filtered by rows alone is forgotten the
    /// moment it is left.
    ///
    /// Driven through the store rather than a model because it is the one case
    /// that needs a compiled clause, and a model with no connection cannot ask
    /// for one. The clause below is written by hand and stands for whatever
    /// `dbedit::filter_clause` would have answered.
    private static func checkAStateHoldingOnlyRowsIsWorthKeeping() {
        let rule = FilterRule(column: "qty", op: .greaterThan, value: "5")
        let state = BrowseState(rules: [rule], compiledClause: "\"qty\" > 5")
        expect(state.isEmpty, false, "rows alone are worth remembering")
        var store = BrowseStore()
        store.save(state, for: "public.orders")
        expect(store.count, 1, "so the store keeps them")
        expect(
            store.state(for: "public.orders").compiledClause, "\"qty\" > 5",
            "with the clause they compiled to, which is what the browse sends")
    }

    /// A row that is no longer there can still be addressed: the views are
    /// rebuilt a frame after a Remove button is pressed, and each carries the
    /// index it was drawn at. Ignored, rather than a window that traps.
    private static func checkAnIndexOutOfRangeChangesNothing() {
        MainActor.assumeIsolated {
            let model = makeModel()
            model.addFilterRule(FilterRule(column: "qty", op: .isNull))
            model.updateFilterRule(at: 4, to: FilterRule(column: "sku", op: .isNull))
            model.removeFilterRule(at: 4)
            expect(model.filterRules.count, 1, "the row that is there is untouched")
            expect(model.filterRules.first?.column, "qty", "and still says what it said")
        }
    }

    /// The case the rule exists for: a row moved from a text column to a numeric
    /// one is still asking `contains`, and `LIKE` against a number is an error
    /// the server raises after somebody has typed a value.
    private static func checkAnOperatorTheColumnCannotAnswerFallsBack() {
        let rule = FilterRule(column: "qty", op: .contains, value: "5")
        expect(rule.settled(in: offered).op, .equals, "an impossible operator falls to the first")
    }

    /// And one it can answer is not touched. A row that changed its own operator
    /// every time a value was typed into it would be unusable.
    private static func checkAnOperatorTheColumnCanAnswerIsLeftAlone() {
        let rule = FilterRule(column: "sku", op: .startsWith, value: "AB")
        let settled = rule.settled(in: offered)
        expect(settled.op, .startsWith, "an operator the column offers stays")
        expect(settled.value, "AB", "and so does what was typed for it")
    }

    /// Text left behind by an operator that compares against nothing would go to
    /// the core at the next Apply, as part of a filter with nothing on screen
    /// describing it.
    private static func checkComparingAgainstNothingCarriesNothing() {
        let rule = FilterRule(column: "qty", op: .isNull, value: "5", second: "9")
        let settled = rule.settled(in: offered)
        expect(settled.value, nil, "IS NULL carries no value")
        expect(settled.second, nil, "and no far end")
    }

    /// Only a range has two ends. The second field is the one most likely to be
    /// left filled, because changing `BETWEEN` to `>` hides it rather than
    /// clearing it.
    private static func checkOnlyARangeKeepsItsFarEnd() {
        let narrowed = FilterRule(column: "qty", op: .greaterThan, value: "5", second: "9")
        expect(narrowed.settled(in: offered).second, nil, "a comparison drops the far end")
        let range = FilterRule(column: "qty", op: .between, value: "5", second: "9")
        let settled = range.settled(in: offered)
        expect(settled.value, "5", "a range keeps the near end")
        expect(settled.second, "9", "and the far one")
    }

    /// A row naming a column the relation does not have is left as it is. It
    /// happens to a restored filter when the table changed underneath it, and
    /// the core's error naming the column is a better answer than a silent move
    /// to a column nobody chose.
    private static func checkAColumnNobodyKnowsIsLeftAlone() {
        let rule = FilterRule(column: "gone", op: .contains, value: "x")
        let settled = rule.settled(in: offered)
        expect(settled.op, .contains, "an unknown column judges nothing")
        expect(settled.value, "x", "and keeps what was typed")
    }

    // MARK: - Fixture

    /// A directory of its own for the config this check must not read.
    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-filter-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            fputs("filter-rows FAIL: a scratch directory could not be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static let orders = RelationInfo(
        schema: "public", name: "orders", kind: .table, estimatedRows: nil)
    private static let regions = RelationInfo(
        schema: "sales", name: "regions", kind: .table, estimatedRows: nil)

    /// What the core would answer for a two-column table: a number that can be
    /// ordered and compared, and text that can also be searched. Written by hand
    /// because the answer is the core's and this check has no connection to ask
    /// — `crates/edit` is where the lists themselves are pinned, against a real
    /// dialect.
    private static let offered = [
        FilterColumn(
            name: "qty", dataType: "numeric",
            operators: [
                .equals, .notEquals, .isNull, .isNotNull, .lessThan, .lessOrEqual, .greaterThan,
                .greaterOrEqual, .between
            ]),
        FilterColumn(
            name: "sku", dataType: "text",
            operators: [
                .equals, .notEquals, .isNull, .isNotNull, .lessThan, .lessOrEqual, .greaterThan,
                .greaterOrEqual, .between, .contains, .startsWith, .endsWith
            ])
    ]

    /// A model with no connection, built the way `BrowseRestoreChecks` builds
    /// its own: a throwaway defaults suite, so that running the checks cannot
    /// read or write the history the user's windows share.
    @MainActor private static func makeModel() -> AppModel {
        let history = QueryHistory(defaults: ScratchDefaults.store("verify-filter-rows"))
        let favorites = QueryFavorites(defaults: ScratchDefaults.store("verify-filter-rows"))
        return AppModel(history: history, favorites: favorites, preferences: Preferences())
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("filter-rows FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
