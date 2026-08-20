import Foundation

/// Executable checks for the record view's arithmetic, run by `--verify-record`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
///
/// One clause of `canShowRecord` is **not** checked here and cannot be: that the
/// record view is offered on the Content tab and nowhere else. Proving it needs
/// a model whose other two conditions hold — a relation selected and rows under
/// it — which needs an Arrow table, which needs a server. A check written
/// against a model with no connection passes whether the clause is there or not,
/// which is worse than no check: it reads as coverage. Deleting the clause from
/// `canShowRecord` must fail something, and the only thing it can fail is
/// somebody opening the Query tab and looking.
enum RecordChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAHiddenColumnIsLeftOutWithoutMovingTheRest()
        checkAColumnThatCannotBeDescribedIsDroppedNotBlanked()
        checkSteppingStopsAtEitherEndRatherThanWrapping()
        checkACursorLeftOverFromABiggerResultLandsSomewhereReal()
        checkAResultWithNoRowsHasNowhereToStep()
        MainActor.assumeIsolated {
            checkAWindowWithNoTableOpenHasNoRecordToShow()
            checkMovingAroundAnEmptyRecordChangesNothing()
        }
        if failures == 0 {
            fputs("record: all checks passed\n", stderr)
        } else {
            fputs("record: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Which columns are listed

    /// The record view lists what the grid draws. A hidden column is left out —
    /// and every field still carries its own index into the result, because that
    /// index is what an edit is written through.
    private static func checkAHiddenColumnIsLeftOutWithoutMovingTheRest() {
        let fields = Record.fields(count: 4, hidden: [1]) { column in
            RecordField(
                column: column, name: "c\(column)", type: "text", value: "v\(column)",
                isNull: false)
        }
        expect(fields.map(\.name), ["c0", "c2", "c3"], "the hidden column is not listed")
        expect(
            fields.map(\.column), [0, 2, 3],
            "and the ones after it still address their own column, not their place in the list")
    }

    /// A column the result cannot describe is dropped whole. Listing it blank
    /// would offer a field that writes nowhere; shifting the ones after it would
    /// be worse still.
    private static func checkAColumnThatCannotBeDescribedIsDroppedNotBlanked() {
        let fields = Record.fields(count: 3, hidden: []) { column in
            guard column != 1 else { return nil }
            return RecordField(
                column: column, name: "c\(column)", type: "", value: "", isNull: true)
        }
        expect(fields.map(\.column), [0, 2], "the undescribable column is gone, the rest are not")
    }

    // MARK: - Moving between rows

    /// Holding an arrow key at the last row stays there. Wrapping to the first
    /// reads as having lost your place rather than as the end of the table.
    private static func checkSteppingStopsAtEitherEndRatherThanWrapping() {
        expect(Record.row(0, steppedBy: 1, rowCount: 3), 1, "down from the first is the second")
        expect(Record.row(2, steppedBy: 1, rowCount: 3), 2, "and down from the last stays there")
        expect(Record.row(0, steppedBy: -1, rowCount: 3), 0, "up from the first stays there too")
        expect(
            Record.row(1, steppedBy: 20, rowCount: 3), 2,
            "a page down past the end lands on the end")
    }

    /// A selection outlives the result it was made in: filtering a table down to
    /// three rows leaves a cursor pointing at row nine hundred, and the next
    /// arrow key has to land on a row that exists.
    private static func checkACursorLeftOverFromABiggerResultLandsSomewhereReal() {
        expect(Record.row(900, steppedBy: 0, rowCount: 3), 2, "the stale row is clamped as read")
        expect(Record.row(900, steppedBy: -1, rowCount: 3), 2, "and so is a step taken from it")
    }

    /// With nothing to show there is nothing to step to, and answering row zero
    /// would put the view on a row the result does not have.
    private static func checkAResultWithNoRowsHasNowhereToStep() {
        expect(Record.row(0, steppedBy: 0, rowCount: 0) == nil, true, "no rows, no row")
        expect(Record.row(0, steppedBy: 1, rowCount: 0) == nil, true, "stepping does not make one")
    }

    // MARK: - What the window offers

    /// With nothing browsed there is no row, so the view is not offered and the
    /// list is empty rather than a list of columns with nothing under them.
    @MainActor private static func checkAWindowWithNoTableOpenHasNoRecordToShow() {
        guard let model = makeModel() else { return }
        expect(model.canShowRecord, false, "nothing is selected, so there is nothing to lay out")
        expect(model.recordFields.isEmpty, true, "and no fields to list")
        expect(model.recordPosition == nil, true, "and no position to print")
    }

    /// Moving around a record that is not there does nothing at all, rather than
    /// putting the cursor somewhere the result does not go.
    @MainActor private static func checkMovingAroundAnEmptyRecordChangesNothing() {
        guard let model = makeModel() else { return }
        model.stepRecord(by: 1)
        expect(model.browseSelection == nil, true, "stepping did not invent a cursor")
        model.focusRecordField(3)
        expect(model.browseSelection == nil, true, "and neither did focusing a field")
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
        let suite = { UserDefaults(suiteName: "dev.dbclient.verify-record.\(UUID())")! }
        return AppModel(
            history: QueryHistory(defaults: suite()),
            favorites: QueryFavorites(defaults: suite()),
            preferences: Preferences(store: suite()))
    }

    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-record-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            failures += 1
            fputs("record FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("record FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
