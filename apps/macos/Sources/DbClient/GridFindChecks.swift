import AppKit

/// Executable checks for ⌘F over a grid, run by `--verify-grid-find`.
///
/// Two things are checked and they are the two that can be wrong without
/// anybody noticing. The first is where the cursor lands: `AppModel.nextMatch`
/// is a pure function of a needle, a starting cell and a way to read one, and
/// every rule about it — the order cells are visited in, wrapping once, a
/// restriction to one column, what a NULL does — is a case below. The second is
/// the menu wiring, which fails silently: the four find commands share one
/// selector and mean four different things only through their tags, so a grid
/// that read the tag wrongly would answer ⌘F by stepping to the next match.
///
/// What is not here is the scan over a real result. `nextMatch` takes a closure
/// rather than an `ArrowTable` precisely so the rule can be checked without
/// buffers, and restating the Arrow read in Swift would be checking a copy of
/// it. That half is `--find-in-grid`, against a table with a million rows in it.
@MainActor
enum GridFindChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheCursorLandsOnTheFirstCellHoldingTheText()
        checkTheSearchIsCaseInsensitiveAndMatchesPartOfACell()
        checkCellsAreVisitedAcrossARowBeforeGoingDown()
        checkTheNextMatchStartsAfterTheCursorRatherThanFindingItAgain()
        checkTheSearchWrapsOnceAndStops()
        checkBackwardsIsTheSameWalkInReverse()
        checkARestrictedSearchLooksInThatColumnAndNoOther()
        checkAColumnTheResultDoesNotHaveFindsNothing()
        checkANullCellIsNeverAMatch()
        checkAnEmptyNeedleFindsNothingRatherThanEverything()
        checkTheGridAnswersTheFourFindCommands()
        if failures == 0 {
            fputs("grid-find: all checks passed\n", stderr)
        } else {
            fputs("grid-find: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Where the cursor lands

    /// The plain case: nothing selected, and the first cell holding the text
    /// wins.
    private static func checkTheCursorLandsOnTheFirstCellHoldingTheText() {
        expect(
            find("carol", from: nil), GridSelection(row: 2, column: 1),
            "the one cell that holds it")
    }

    /// Case-insensitive, and a fragment counts.
    ///
    /// Both halves are what a reader means by "find" — nobody types the case of
    /// a value they are looking for, and nobody types all of it. A search that
    /// wanted the whole cell would find nothing in a `text` column, which is
    /// where searching is worth most.
    private static func checkTheSearchIsCaseInsensitiveAndMatchesPartOfACell() {
        expect(
            find("CAROL", from: nil), GridSelection(row: 2, column: 1),
            "typed in the case it was not stored in")
        expect(
            find("aro", from: nil), GridSelection(row: 2, column: 1),
            "and three letters out of the middle of it")
    }

    /// Across a row and then down, which is the order the rows are read in.
    ///
    /// The wrong order is not a crash and not visibly wrong on one screen: it
    /// finds *a* match, just not the one nearest the cursor. Row 1 holds the
    /// needle in its last column and row 2 in its middle one, so reading down
    /// the columns instead of across the rows would answer with row 2.
    private static func checkCellsAreVisitedAcrossARowBeforeGoingDown() {
        expect(
            find("x", from: nil), GridSelection(row: 1, column: 2),
            "the later column of the earlier row, not the earlier column of the later one")
    }

    /// Find Next moves. Starting the walk at the cursor rather than after it
    /// would find the cell already selected, and ⌘G would sit still.
    private static func checkTheNextMatchStartsAfterTheCursorRatherThanFindingItAgain() {
        let first = find("dave", from: nil)
        expect(first, GridSelection(row: 3, column: 1), "the first one")
        expect(
            find("dave", from: first), GridSelection(row: 4, column: 1),
            "and the next press moves to the second")
    }

    /// Off the end and back to the top, once.
    ///
    /// Wrapping is right here for a reason worth writing down: the last fetched
    /// row is wherever paging happened to stop, not the end of the table, so
    /// stopping there would present an accident as a boundary. Once, because a
    /// needle in no cell has to come back rather than walk forever — that is the
    /// second assertion.
    private static func checkTheSearchWrapsOnceAndStops() {
        expect(
            find("alice", from: GridSelection(row: 4, column: 1)),
            GridSelection(row: 0, column: 1),
            "past the last row, the search comes back to the first")
        expect(
            find("nobody", from: GridSelection(row: 2, column: 1)), nil,
            "and a needle in no cell is answered rather than walked for ever")
    }

    /// ⇧⌘G is the same walk read the other way.
    private static func checkBackwardsIsTheSameWalkInReverse() {
        expect(
            find("dave", from: GridSelection(row: 4, column: 1), backwards: true),
            GridSelection(row: 3, column: 1),
            "the match before the cursor")
        expect(
            find("dave", from: nil, backwards: true), GridSelection(row: 4, column: 1),
            "and with nothing selected, the last one — so the first press finds a match "
                + "either way rather than skipping the one at the end")
    }

    /// A search restricted to a column ignores every other one.
    ///
    /// The case this exists for: `4` is in the id column and inside the note
    /// three columns over, and somebody who narrowed the search to `id` asked
    /// not to be shown the note.
    private static func checkARestrictedSearchLooksInThatColumnAndNoOther() {
        expect(
            find("4", from: nil), GridSelection(row: 1, column: 2),
            "unrestricted, the note comes first because it is in an earlier row")
        expect(
            find("4", from: nil, only: "id"), GridSelection(row: 3, column: 0),
            "restricted, only the id column is looked at")
    }

    /// A column the result does not carry finds nothing rather than quietly
    /// widening back to all of them.
    ///
    /// The bar keeps what was last typed and what it was last restricted to, so
    /// a search narrowed to `email` and then pointed at a result with no such
    /// column is a real state. Answering out of some other column would be a
    /// true answer to a question nobody asked. (The model drops the restriction
    /// when the bar is opened over a result that has no such column; this is the
    /// rule underneath that, and it is what makes the drop safe rather than
    /// necessary.)
    private static func checkAColumnTheResultDoesNotHaveFindsNothing() {
        expect(find("alice", from: nil, only: "email"), nil, "no such column, no match")
    }

    /// A NULL is never a match.
    ///
    /// It is the absence of a value, not a value that looks like one, and the
    /// grid draws the word NULL because something has to be drawn. Searching for
    /// "null" and landing on every empty cell in the result would be finding
    /// this program's own word.
    private static func checkANullCellIsNeverAMatch() {
        expect(find("null", from: nil), nil, "the word the grid draws is not in the data")
    }

    /// An empty field finds nothing, rather than matching every cell.
    ///
    /// `contains("")` is true of every string, so without the guard the first
    /// press of Return over an empty field would move the cursor to the top-left
    /// cell and report a match.
    private static func checkAnEmptyNeedleFindsNothingRatherThanEverything() {
        expect(
            find("", from: GridSelection(row: 2, column: 1)), nil, "nothing typed, nothing found")
    }

    // MARK: - The menu wiring

    /// The grid answers the Edit menu's find commands, and answers Find Next
    /// only when there is something to find.
    ///
    /// The wiring fails quietly in two directions and this covers both. A grid
    /// that did not implement the selector would leave ⌘F disabled while the
    /// cursor is in it — the state this feature replaces — and one that
    /// validated every tag alike would offer Find Next over an empty field,
    /// where the command has nothing to step through.
    private static func checkTheGridAnswersTheFourFindCommands() {
        let grid = GridView(frame: NSRect(x: 0, y: 0, width: 400, height: 300), device: nil)
        expect(
            grid.responds(to: #selector(GridView.performFindPanelAction(_:))), true,
            "the selector the four items share, which is how the responder chain reaches here")

        // Before a result arrives there is nothing to search, and every one of
        // them is off.
        expect(
            [.showFindPanel, .next, .previous, .setFindString].map { grid.validates($0) },
            [false, false, false, false],
            "with no result under the grid, none of the four is offered")

        grid.offersFind = true
        expect(
            [.showFindPanel, .setFindString].map { grid.validates($0) }, [true, true],
            "with a result, the bar can be opened and a cell can be taken as the search")
        expect(
            [.next, .previous].map { grid.validates($0) }, [false, false],
            "but stepping is off until something is typed")

        grid.hasFindText = true
        expect(
            [.next, .previous].map { grid.validates($0) }, [true, true],
            "and on once there is")

        var taken: [NSFindPanelAction] = []
        grid.onFindAction = { taken.append($0) }
        for action in [NSFindPanelAction.showFindPanel, .next, .previous, .setFindString] {
            grid.performFindPanelAction(item(action))
        }
        expect(
            taken, [.showFindPanel, .next, .previous, .setFindString],
            "each item is passed on as the command its tag names, and not as another one")
    }

    // MARK: - Harness

    /// Five rows of three columns, holding the values every case above reads.
    ///
    /// `id`, `name`, `note`. Row 1's note holds `x` in a later column than row
    /// 2's name does, which is what makes the walk order visible; `4` is both an
    /// id and part of a note, which is what makes a column restriction visible;
    /// and row 0 has a NULL where a note would be.
    private static let rows: [[String?]] = [
        ["1", "alice", nil],
        ["2", "bob", "note 4x"],
        ["3", "carol x", "seen"],
        ["4", "dave", "seen"],
        ["5", "dave", "seen"]
    ]

    private static let columns = ["id", "name", "note"]

    private static func find(
        _ needle: String, from: GridSelection?, backwards: Bool = false, only: String? = nil
    ) -> GridSelection? {
        AppModel.nextMatch(
            for: needle, from: from, backwards: backwards, rows: rows.count, columns: columns,
            onlyColumn: only, value: { rows[$0][$1] })
    }

    /// A menu item as the Edit menu builds one: the shared selector, and the tag
    /// that is the only thing saying which command it is.
    private static func item(_ action: NSFindPanelAction) -> NSMenuItem {
        let item = NSMenuItem(
            title: "", action: #selector(GridView.performFindPanelAction(_:)), keyEquivalent: "")
        item.tag = Int(action.rawValue)
        return item
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("grid-find FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}

extension GridView {
    /// Whether this grid would enable the menu item for a find command.
    fileprivate func validates(_ action: NSFindPanelAction) -> Bool {
        let item = NSMenuItem(
            title: "", action: #selector(GridView.performFindPanelAction(_:)), keyEquivalent: "")
        item.tag = Int(action.rawValue)
        return validateMenuItem(item)
    }
}
