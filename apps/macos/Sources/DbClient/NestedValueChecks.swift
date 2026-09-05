import CDbFfi
import Foundation

/// Executable checks for the Arrow reader's walk into `children`, run by
/// `--verify-nested`.
///
/// Three drivers hand the server's own Arrow schema across the FFI with no type
/// table in between — Flight SQL, BigQuery and Databricks each decode an IPC
/// stream and keep what it describes — so a `LIST`, a `STRUCT` or a `MAP` from
/// any of them arrives with its values in children rather than in the column's
/// own buffers. Until this reader followed them, every cell of such a column drew
/// its Arrow format string: `<+l>`, which is neither a value nor a null.
///
/// The fixtures are built here as the C data interface lays them out, pointer by
/// pointer, rather than read off a server. That is the point of them. What this
/// file checks is arithmetic over another language's memory, and the
/// characteristic failure is not a crash: a reader that dropped a parent's offset
/// answers the neighbouring row, one that dropped a child's answers the row
/// before that, and both look like data. Two cases exist for exactly those two
/// mistakes, because every other fixture here starts at zero and would pass
/// whether the offsets were added or not.
///
/// `Arena` owns what is built and takes it back down. The batch itself is the one
/// thing it does not free: that pointer belongs to `ArrowTable` from the moment
/// it is appended, exactly as one from Rust does.
///
/// Behind a flag on the binary for the reason `SchemaMetadataChecks` gives: the
/// package declares one executable target and it links the Rust staticlib.
enum NestedValueChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        MainActor.assumeIsolated {
            checkAListIsReadThroughItsElementsRatherThanPrintedAsItsFormat()
            checkAStructPutsEachFieldUnderItsOwnName()
            checkAMapIsAnObjectRatherThanAListOfEntries()
            checkAFixedSizeListTakesItsWidthFromTheFormatString()
            checkASlicedBatchAnswersForTheRowItWasAsked()
            checkAChildThatStartsPartWayThroughItsBufferIsFollowedThere()
            checkAnOffsetsPairThatGoesBackwardsIsRefusedRatherThanFollowed()
            checkABatchThatDisagreesWithItsSchemaIsNotReadAnyway()
            checkANullCellAndANullElementAreDifferentAnswers()
            checkANestedChildThatIsNullSaysSoInsideTheDocument()
            checkAStringInsideAStructCannotEndTheDocument()
            checkTheCellIsCutToTheGridsBudgetAndSaysSo()
            checkTheViewerGetsTheWholeDocumentTheCellCouldNotHold()
            checkAWalkPastItsBudgetTurnsBackRatherThanRunningOn()
            checkAValueTheWalkNeverOpenedIsNotCountedAsLeftBehind()
            checkAListOfStructsIsReadAllTheWayDown()
            checkASchemaNestedPastTheCapIsRefusedRatherThanFollowed()
            checkAMapWhoseChildIsNotAPairIsNotGuessedAt()
            checkTheLabelIsArrowsSpellingOfTheColumn()
            checkANestedColumnIsNotOfferedToTheEditor()
        }
        if failures == 0 {
            fputs("nested: all checks passed\n", stderr)
        } else {
            fputs("nested: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - The four shapes that have been seen to arrive

    /// `SELECT [1, 2, 3]` over Flight SQL, which is `list<l: int32>` on the wire.
    ///
    /// The second assertion is the whole case: before this, the cell held the
    /// characters `<+l>` — the format string itself, in every row, for a column
    /// with values in it.
    @MainActor private static func checkAListIsReadThroughItsElementsRatherThanPrintedAsItsFormat()
    {
        let arena = Arena()
        let table = lists(arena, rows: [[10, 20], [30], []])
        expect(table.text(row: 0, column: 0), "[10,20]", "the elements this row points at")
        expect(
            table.text(row: 0, column: 0).contains("+l"), false,
            "and not the format string, which is what every row of one used to read")
        expect(table.text(row: 1, column: 0), "[30]", "the next row's, which are different ones")
        expect(table.text(row: 2, column: 0), "[]", "and an empty list, which is not a null")
        release(table, arena)
    }

    /// `struct<qty: int32, unit: string>`, the shape a DuckDB `STRUCT` takes.
    ///
    /// A struct's children are one array each, indexed by the row — not by the
    /// field — so a reader that walked them the way it walks a list would answer
    /// the first field's value under every name. The fields hold different types
    /// here so that such an answer cannot look right.
    @MainActor private static func checkAStructPutsEachFieldUnderItsOwnName() {
        let arena = Arena()
        let table = structs(arena)
        expect(
            table.text(row: 1, column: 0), "{\"qty\":7,\"unit\":\"kg\"}",
            "each field under the name the schema gave it, for the row asked for")
        release(table, arena)
    }

    /// `map<string, int32>`, which Arrow lays out as a list of
    /// `struct<key, value>`.
    ///
    /// Spelling it `[{"key":"y","value":2}]` would be this reader describing
    /// Arrow's layout where `{"y":2}` is the value the database holds, and the
    /// difference is invisible to anyone who has not read the spec.
    @MainActor private static func checkAMapIsAnObjectRatherThanAListOfEntries() {
        let arena = Arena()
        let table = maps(arena)
        let drawn = table.text(row: 1, column: 0)
        expect(drawn, "{\"y\":2,\"z\":3}", "the pairs as an object")
        expect(drawn.contains("\"key\""), false, "with Arrow's entries struct stepped over")
        expect(
            drawn.contains("sentinel") || drawn.contains("-999"), false,
            "and the entries array's own offset added, so nothing in front of it is read")
        expect(table.text(row: 2, column: 0), "{}", "and an empty map is an empty object")
        release(table, arena)
    }

    /// `fixed_size_list<: string>[2]`, a DuckDB `VARCHAR[2]`.
    ///
    /// The one shape with no offsets buffer: the width is in the format string
    /// and row *i* owns elements `i * N ..< (i + 1) * N`. A reader that took the
    /// width from anywhere else, or that read from the front, would answer the
    /// same pair for every row.
    @MainActor private static func checkAFixedSizeListTakesItsWidthFromTheFormatString() {
        let arena = Arena()
        let table = fixedLists(arena)
        expect(table.text(row: 0, column: 0), "[\"a\",\"b\"]", "the first row's pair")
        expect(table.text(row: 1, column: 0), "[\"c\",\"d\"]", "and the pair after it")
        release(table, arena)
    }

    // MARK: - The two offsets

    /// A batch handed over sliced answers for the row the grid asked for.
    ///
    /// Arrow describes a slice by leaving the buffers alone and moving `offset`,
    /// and a driver that pages a result produces one on every page after the
    /// first. A reader that ignored it would answer row 0 for row 0 — the right
    /// shape, the wrong row, and nothing on screen to say so.
    @MainActor private static func checkASlicedBatchAnswersForTheRowItWasAsked() {
        let arena = Arena()
        let table = lists(arena, rows: [[10, 20], [30], []], slicedBy: 1)
        expect(table.rowCount, 2, "two of the three rows are in view")
        expect(table.text(row: 0, column: 0), "[30]", "and the first of them is the second row")
        expect(table.text(row: 1, column: 0), "[]", "with the third behind it")
        release(table, arena)

        // The same offset, one shape along, because the two are added in
        // different lines: a list's goes into its offsets buffer and a struct's
        // is handed down to each field. Passing one says nothing about the other.
        let second = Arena()
        let structs = structs(second, slicedBy: 1)
        expect(
            structs.text(row: 0, column: 0), "{\"qty\":7,\"unit\":\"kg\"}",
            "a struct's fields are read at the sliced row, not at the buffer's first")
        release(structs, second)
    }

    /// And a child that starts part way through its own buffer is followed there.
    ///
    /// The second offset, and the one easier to leave out: a list's offsets are
    /// *logical* indices into its values array, so the values array's own offset
    /// is added on top. Here logical 0 is the second number in the buffer, and a
    /// reader that read the buffer directly would answer a sentinel this fixture
    /// put there for it to find.
    @MainActor private static func checkAChildThatStartsPartWayThroughItsBufferIsFollowedThere() {
        let arena = Arena()
        let schema = arena.schema(format: "+l", name: "v")
        arena.attach([arena.schema(format: "i", name: "l")], to: schema)
        let values = arena.array(
            length: 3, offset: 1, buffers: [nil, arena.buffer([Int32(-999), 5, 6, 7])])
        let array = arena.array(
            length: 2, buffers: [nil, arena.buffer([Int32(0), 2, 3])])
        arena.attach([values], to: array)
        let table = arena.table(schema: schema, array: array)
        expect(table.text(row: 0, column: 0), "[5,6]", "the child's logical elements")
        expect(
            table.text(row: 0, column: 0).contains("-999"), false,
            "and not the value sitting in front of where its values begin")
        release(table, arena)
    }

    // MARK: - A batch that disagrees with itself

    /// An offsets buffer whose pair goes backwards reads as unreadable, and does
    /// not take the window down.
    ///
    /// Found by mutation rather than by reading: `a..<b` traps when `b < a`, so a
    /// range assembled from two offsets and then bounds-checked is a check that
    /// never runs. The mutation that made a list one element short turned the
    /// empty rows into `0..<-1` and killed the process — a crash where the whole
    /// file's posture is that memory another language handed over gets refused
    /// rather than followed. A grid that crashes on a corrupt page is a worse
    /// failure than a cell that says it could not read one.
    @MainActor private static func checkAnOffsetsPairThatGoesBackwardsIsRefusedRatherThanFollowed()
    {
        let arena = Arena()
        let schema = arena.schema(format: "+l", name: "v")
        arena.attach([arena.schema(format: "i", name: "l")], to: schema)
        let values = arena.array(length: 3, buffers: [nil, arena.buffer([Int32(1), 2, 3])])
        // Row 0's pair runs backwards; row 1's is ordinary, so the case also
        // shows the refusal is per cell rather than per column.
        let array = arena.array(length: 2, buffers: [nil, arena.buffer([Int32(2), 0, 3])])
        arena.attach([values], to: array)
        let table = arena.table(schema: schema, array: array)
        expect(table.text(row: 0, column: 0), "\"<unreadable>\"", "a pair that goes backwards")
        expect(table.text(row: 1, column: 0), "[1,2,3]", "and the row beside it still reads")
        release(table, arena)

        // And a pair that runs off the end of the values array. The per-element
        // guard would catch each one and write a placeholder, so without the
        // whole-run bound the cell reads `[1,2,3,"<unreadable>","<unreadable>"]`
        // — a list two elements longer than the column holds, three of whose
        // values are real. Refusing the cell says what happened; padding it out
        // states a length the batch never had.
        let second = Arena()
        let past = second.schema(format: "+l", name: "v")
        second.attach([second.schema(format: "i", name: "l")], to: past)
        let short = second.array(length: 3, buffers: [nil, second.buffer([Int32(1), 2, 3])])
        let overrun = second.array(length: 1, buffers: [nil, second.buffer([Int32(0), 5])])
        second.attach([short], to: overrun)
        let table2 = second.table(schema: past, array: overrun)
        expect(
            table2.text(row: 0, column: 0), "\"<unreadable>\"",
            "a run that ends past the values array is refused whole")
        release(table2, second)
    }

    /// A batch carrying children the schema did not name is refused, not read as
    /// far as it lines up.
    ///
    /// The decision is "all of them or none", and the mutation that relaxed it to
    /// "at least as many" survived until this case existed. Reading the first two
    /// of three children is defensible right up to the moment the extra one was
    /// there because the producer disagreed about the order — and a struct read
    /// under the wrong names is a wrong value that looks like a right one, which
    /// is what every guard in this file is for.
    @MainActor private static func checkABatchThatDisagreesWithItsSchemaIsNotReadAnyway() {
        let arena = Arena()
        let schema = arena.schema(format: "+s", name: "v")
        arena.attach(
            [arena.schema(format: "i", name: "qty"), arena.schema(format: "u", name: "unit")],
            to: schema)
        let array = arena.array(length: 2, buffers: [nil])
        arena.attach(
            [
                arena.array(length: 2, buffers: [nil, arena.buffer([Int32(5), 7])]),
                arena.strings(["g", "kg"]),
                arena.array(length: 2, buffers: [nil, arena.buffer([Int32(1), 2])])
            ], to: array)
        let table = arena.table(schema: schema, array: array)
        expect(
            table.text(row: 0, column: 0), "\"<unreadable>\"",
            "three children under a two-field schema is a batch that cannot be trusted")
        release(table, arena)
    }

    // MARK: - Nulls, escaping, budgets

    /// A NULL list and a list of NULLs are different values, and the grid has to
    /// draw them differently.
    ///
    /// The cell of a NULL column is blank, as every other NULL cell in this grid
    /// is; a NULL *element* is the word `null` inside the document, because JSON
    /// has one and leaving it out would shorten the list. A reader that answered
    /// `[null]` for a NULL cell would say the row holds a one-element list.
    @MainActor private static func checkANullCellAndANullElementAreDifferentAnswers() {
        let arena = Arena()
        let table = lists(arena, rows: [[1], nil, [nil, 2]])
        expect(table.text(row: 1, column: 0), "", "a NULL cell is blank, like every other")
        expect(table.isNull(row: 1, column: 0), true, "and says so where the grid asks")
        expect(table.text(row: 2, column: 0), "[null,2]", "a null element is a null in the list")
        expect(table.isNull(row: 2, column: 0), false, "in a cell that is not itself null")
        release(table, arena)
    }

    /// A nested value that is itself NULL says so inside the document.
    ///
    /// The other half of the case above, and the one no fixture reached until a
    /// mutation asked: a NULL *cell* is caught by `text(at:)` before the walk
    /// starts, so the null guard inside the walk only ever answers for a nested
    /// *child* — a struct field that is a struct, an element of a list of lists.
    /// Without it the walk reads a row whose buffers say nothing is there and
    /// answers `{}` or `[]`, which is a value the database does not hold.
    @MainActor private static func checkANestedChildThatIsNullSaysSoInsideTheDocument() {
        let arena = Arena()
        let schema = arena.schema(format: "+l", name: "v")
        let element = arena.schema(format: "+s", name: "item")
        arena.attach([arena.schema(format: "i", name: "n")], to: element)
        arena.attach([element], to: schema)

        let numbers = arena.array(length: 2, buffers: [nil, arena.buffer([Int32(3), 4])])
        // The first struct is null; its `n` still holds a number, which is what
        // makes the answer `null` rather than `{"n":3}` a claim about the
        // validity bitmap instead of about the buffer under it.
        let structs = arena.array(length: 2, buffers: [arena.validity([false, true])])
        arena.attach([numbers], to: structs)

        let array = arena.array(length: 1, buffers: [nil, arena.buffer([Int32(0), 2])])
        arena.attach([structs], to: array)
        let table = arena.table(schema: schema, array: array)
        expect(
            table.text(row: 0, column: 0), "[null,{\"n\":4}]",
            "a null struct inside a list is a null, not the row its buffers still hold")
        release(table, arena)
    }

    /// A value holding a quote and a newline does not end the document.
    ///
    /// The failure this prevents is not a wrong value; it is a document the viewer
    /// refuses to lay out, falling back to the raw line — which is the one thing
    /// the pane was opened to escape. A `text` field inside a struct is exactly
    /// where an unescaped character comes from.
    @MainActor private static func checkAStringInsideAStructCannotEndTheDocument() {
        let arena = Arena()
        let table = structs(arena, unit: "a\"b\nc")
        expect(
            table.text(row: 1, column: 0), "{\"qty\":7,\"unit\":\"a\\\"b\\nc\"}",
            "the quote and the line break escaped where they stand")
        expect(
            prettyPrintedJSON(table.text(row: 1, column: 0)) != nil, true,
            "so the document still parses, which is what the viewer needs of it")
        release(table, arena)
    }

    /// A list longer than the grid can draw is cut, and the cut is visible.
    ///
    /// The budget is the memory rule of this path: the walk runs per visible cell
    /// per frame, and a ten-thousand-element list would build a megabyte of string
    /// to fill two hundred points of grid. The ellipsis is what stops a cut list
    /// reading as a whole one.
    @MainActor private static func checkTheCellIsCutToTheGridsBudgetAndSaysSo() {
        let arena = Arena()
        let table = lists(arena, rows: [[], Array(0..<1000).map { Int32($0) }, []])
        let drawn = table.text(row: 1, column: 0)
        expect(
            drawn.hasSuffix("… (911 more)"), true,
            "the cut is marked where it happened, and says how much is behind it")
        // 256 characters and the mark, written out rather than read back off
        // `cellBudget`: a check that took its expectation from the constant would
        // agree with any value the constant had, which is a check that cannot
        // fail. Raising the budget should mean deciding this line again.
        expect(
            drawn.count, 268,
            "and the cell holds the budget and the mark, not the thousand elements")
        expect(
            ArrowTable.cellBudget, 256,
            "which is what the budget is, said here so the two cannot drift apart quietly")
        release(table, arena)
    }

    /// And the viewer asks the same walk for the same value with room to hold it.
    ///
    /// Two budgets rather than one, because the two callers cost different
    /// things: the grid pays per frame and the pane pays once per selection
    /// change. A pane that showed the cell's truncation would leave a reader with
    /// no way to see the value at all, which is the state this whole slice is
    /// about.
    @MainActor private static func checkTheViewerGetsTheWholeDocumentTheCellCouldNotHold() {
        let arena = Arena()
        let table = lists(arena, rows: [[], Array(0..<1000).map { Int32($0) }, []])
        guard let document = table.json(row: 1, column: 0) else {
            fail("the viewer is given a document for a nested column")
            release(table, arena)
            return
        }
        expect(document.hasSuffix("…"), false, "nothing was cut at the viewer's budget")
        expect(document.hasSuffix(",999]"), true, "so the last element is in it")
        expect(
            table.json(row: 1, column: 1), nil,
            "and a column that is not nested has no document, so the caller keeps one path")

        // And the strip and the pane are handed that document rather than the
        // cell. This is the line between them: without it the pane draws the
        // grid's preview, ellipsis and all, and there is nowhere left to see the
        // value in full — which is the state the whole slice exists to end.
        let strip = AppModel.text(
            of: .nested, in: table, at: GridSelection(row: 1, column: 0, anchor: nil))
        expect(strip.hasSuffix("…"), false, "the strip is given the document, not the cell")
        expect(strip.count, document.count, "the same one the viewer's own call builds")
        release(table, arena)
    }

    /// The budget stops the walk, not only the writing.
    ///
    /// This is the half of the budget that is a memory rule rather than a layout
    /// one, and until the mark carried a number nothing here could see it: the
    /// sink refuses an over-budget write on its own, so a walk that visited every
    /// element of a thousand-element list writing nothing drew exactly the cell a
    /// walk that turned back after five drew. The same string, an order of
    /// magnitude apart in cost. The count beside the ellipsis is what tells them
    /// apart, because a walk can only report what it left if it stopped where it
    /// left it.
    ///
    /// The lengths counted below are counted against a 256-character cell.
    /// Raising `cellBudget` means counting them again — the check above pins the
    /// constant so that these cannot go quietly wrong.
    @MainActor private static func checkAWalkPastItsBudgetTurnsBackRatherThanRunningOn() {
        // `{`, `"a"`, `:` and a 250-character string quoted overrun 256 inside the
        // first field's value, leaving the second field one the walk never reached.
        let arena = Arena()
        let table = structOfList(arena, note: String(repeating: "x", count: 250))
        let drawn = table.text(row: 0, column: 0)
        expect(
            drawn.hasSuffix("… (1 more)"), true,
            "a struct stops at the field the budget ran out on and counts the rest")
        expect(drawn.count, 266, "having spent the budget and nothing past it")
        release(table, arena)

        // Three 60-character keys with their values, and a fourth key cut in half,
        // come to 256. The fifth entry is one the walk never opens.
        let keyed = Arena()
        let table2 = wideMap(
            keyed, keys: (0..<5).map { "k\($0)" + String(repeating: "y", count: 58) })
        let entries = table2.text(row: 0, column: 0)
        expect(
            entries.hasSuffix("… (1 more)"), true,
            "and a map does the same over its entries, which are a run and not fields")
        expect(entries.count, 266, "to the same budget")
        release(table2, keyed)
    }

    /// And it does not count a value it never opened.
    ///
    /// The number belongs to the run the walk was in when it stopped. A cell cut
    /// on a field's *name* has not looked inside that field, and reporting the
    /// field's length there would describe one list the reader cannot see the
    /// start of while saying nothing about the fields behind it. So the walk
    /// refuses a run it cannot spend on rather than entering it and turning back —
    /// the cheaper answer and the honest one at the same time.
    @MainActor private static func checkAValueTheWalkNeverOpenedIsNotCountedAsLeftBehind() {
        // `{`, `"a"`, `:`, a 245-character string quoted, `,` and `"b"` come to
        // exactly 256, so the budget runs out on the colon before the list and the
        // list is never read.
        let arena = Arena()
        let table = structOfList(arena, note: String(repeating: "x", count: 245))
        let drawn = table.text(row: 0, column: 0)
        expect(
            drawn.hasSuffix("\"b\"…"), true,
            "the cut lands on the field's name, and the mark carries no count")
        expect(
            drawn.contains("more)"), false,
            "because the four elements behind it are not a run this walk was ever in")
        release(table, arena)
    }

    /// A list of structs, which is what a query returning rows inside rows sends.
    ///
    /// Depth is where an index rule that happens to work at one level stops
    /// working: each struct is read at the element index the list pointed at,
    /// which is neither the row nor zero.
    @MainActor private static func checkAListOfStructsIsReadAllTheWayDown() {
        let arena = Arena()
        let table = listsOfStructs(arena)
        expect(
            table.text(row: 1, column: 0), "[{\"n\":3},{\"n\":4}]",
            "each struct read at the element the list pointed at")
        release(table, arena)
    }

    /// A schema nested past the cap reads as its format string.
    ///
    /// The walk is recursive over another process's memory, so how deep it goes
    /// is a number a server chooses. Stopping at a bound is the recoverable end of
    /// that: the level past it reads as `<+l>`, which is what every nested column
    /// read as before this file could follow one at all.
    ///
    /// Both sides of the bound, because a reader that stopped one level early is
    /// indistinguishable from one that stopped one level late until somebody has
    /// a schema of exactly that depth.
    @MainActor private static func checkASchemaNestedPastTheCapIsRefusedRatherThanFollowed() {
        let arena = Arena()
        let cap = ArrowTable.maxNesting
        expect(
            ArrowTable.kind(of: chain(arena, levels: cap)).label, listLabel(cap, around: "int32"),
            "a schema exactly at the cap is followed to its leaf")
        expect(
            ArrowTable.kind(of: chain(arena, levels: cap + 1)).label,
            listLabel(cap, around: "<+l>"),
            "and one level further stops, with the level it stopped at saying so")
        arena.release()
    }

    /// A `+m` whose child is not a two-field struct is not a map this can read.
    ///
    /// Which of the children was the key would be a guess, and a guess here puts a
    /// value where a name goes. Refusing leaves `<+m>`, which is honest about what
    /// happened.
    @MainActor private static func checkAMapWhoseChildIsNotAPairIsNotGuessedAt() {
        let arena = Arena()
        let broken = arena.schema(format: "+m", name: "m")
        let entries = arena.schema(format: "+s", name: "entries")
        arena.attach([arena.schema(format: "u", name: "key")], to: entries)
        arena.attach([entries], to: broken)
        expect(
            ArrowTable.kind(of: broken).label, "<+m>",
            "one child under the entries struct is not a key and a value")
        arena.release()
    }

    /// The strip names a nested column in Arrow's words.
    ///
    /// Arrow's rather than SQL's, for the reason the scalar labels give: DuckDB
    /// spells this `STRUCT(qty INTEGER, unit VARCHAR)` and ClickHouse spells it
    /// `Tuple(qty Int32, unit String)`, and a column arriving over Flight SQL has
    /// no catalogue entry to borrow either from.
    @MainActor private static func checkTheLabelIsArrowsSpellingOfTheColumn() {
        let arena = Arena()
        let table = structs(arena)
        expect(
            table.columns[0].kind.label, "struct<qty: int32, unit: utf8>",
            "the fields, their names and their Arrow types")
        expect(table.columns[0].kind.isNumeric, false, "and nothing right-aligns a struct")
        release(table, arena)
    }

    /// The editor refuses a nested cell, and refuses it as itself.
    ///
    /// The hazard is the one `.binary` already records: what is on screen is this
    /// reader's JSON of buffers that have no text form, so a box seeded from it
    /// would offer to write a rendering into a `STRUCT`. Flight SQL carries a
    /// dialect, which is what `edits_rows` asks for, so its tables are editable —
    /// this is a refusal somebody can reach rather than a precaution.
    @MainActor private static func checkANestedColumnIsNotOfferedToTheEditor() {
        let arena = Arena()
        let table = structs(arena)
        let rendering = AppModel.rendering(
            kind: table.columns[0].kind, shape: "", declared: "", bytes: { [] })
        guard case .nested = rendering else {
            fail("a nested column asks for the nested rendering")
            release(table, arena)
            return
        }
        let cell = AppModel.InspectedCell(
            column: "c", type: "", value: table.text(row: 1, column: 0), isNull: false,
            address: "row 2", rendering: rendering, isExpanded: true, toggleExpanded: {})
        expect(
            ValueEdit.offered(for: cell, obstacle: nil),
            .refused("A nested value cannot be edited here."),
            "said out loud, and not by a box that quietly does not appear")
        expect(
            RenderedValue.make(from: cell).text.contains("\n"), true,
            "while the pane still lays the value out over lines")
        release(table, arena)
    }

    // MARK: - Fixtures

    /// A one-column table of `list<int32>`.
    ///
    /// A `nil` row is a NULL cell and a `nil` element is a NULL element, which are
    /// different answers and have a case of their own.
    @MainActor private static func lists(
        _ arena: Arena, rows: [[Int32?]?], slicedBy: Int = 0
    ) -> ArrowTable {
        let schema = arena.schema(format: "+l", name: "v")
        arena.attach([arena.schema(format: "i", name: "l")], to: schema)

        var offsets: [Int32] = [0]
        var values: [Int32?] = []
        for row in rows {
            values.append(contentsOf: row ?? [])
            offsets.append(Int32(values.count))
        }
        let child = arena.array(
            length: values.count,
            buffers: [
                arena.validity(values.map { $0 != nil }), arena.buffer(values.map { $0 ?? 0 })
            ])
        let array = arena.array(
            length: rows.count - slicedBy, offset: slicedBy,
            buffers: [arena.validity(rows.map { $0 != nil }), arena.buffer(offsets)])
        arena.attach([child], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `struct<qty: int32, unit: utf8>` column over three rows.
    @MainActor private static func structs(
        _ arena: Arena, unit: String = "kg", slicedBy: Int = 0
    ) -> ArrowTable {
        let schema = arena.schema(format: "+s", name: "v")
        arena.attach(
            [arena.schema(format: "i", name: "qty"), arena.schema(format: "u", name: "unit")],
            to: schema)

        let quantities = arena.array(length: 3, buffers: [nil, arena.buffer([Int32(5), 7, 9])])
        let units = arena.strings(["g", unit, "t"])
        let array = arena.array(length: 3 - slicedBy, offset: slicedBy, buffers: [nil])
        arena.attach([quantities, units], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `map<utf8, int32>` column: `{a:1}`, `{y:2, z:3}`, `{}`.
    ///
    /// The entries array starts one pair into its own children, with a sentinel
    /// in front of it. Arrow produces that shape whenever a map column has been
    /// sliced, and a reader that added the list's offsets to the children without
    /// adding the entries array's own would find the sentinel.
    @MainActor private static func maps(_ arena: Arena) -> ArrowTable {
        let schema = arena.schema(format: "+m", name: "v")
        let entriesSchema = arena.schema(format: "+s", name: "entries")
        arena.attach(
            [arena.schema(format: "u", name: "key"), arena.schema(format: "i", name: "value")],
            to: entriesSchema)
        arena.attach([entriesSchema], to: schema)

        let keys = arena.strings(["sentinel", "a", "y", "z"])
        let values = arena.array(length: 4, buffers: [nil, arena.buffer([Int32(-999), 1, 2, 3])])
        let entries = arena.array(length: 3, offset: 1, buffers: [nil])
        arena.attach([keys, values], to: entries)

        let array = arena.array(length: 3, buffers: [nil, arena.buffer([Int32(0), 1, 3, 3])])
        arena.attach([entries], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `struct<a: utf8, b: list<int32>>` row: the note, then `[1,2,3,4]`.
    ///
    /// The field names are one character each because the checks that use this
    /// count them, and the note is the caller's so that the cut can be placed
    /// exactly where a check wants it. Nesting under the second field rather than
    /// beside it, because the rule being checked is about a run the walk may or
    /// may not step into.
    @MainActor private static func structOfList(_ arena: Arena, note: String) -> ArrowTable {
        let schema = arena.schema(format: "+s", name: "v")
        let tail = arena.schema(format: "+l", name: "b")
        arena.attach([arena.schema(format: "i", name: "item")], to: tail)
        arena.attach([arena.schema(format: "u", name: "a"), tail], to: schema)

        let leaves = arena.array(length: 4, buffers: [nil, arena.buffer([Int32(1), 2, 3, 4])])
        let tails = arena.array(length: 1, buffers: [nil, arena.buffer([Int32(0), 4])])
        arena.attach([leaves], to: tails)

        let array = arena.array(length: 1, buffers: [nil])
        arena.attach([arena.strings([note]), tails], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `map<utf8, int32>` row holding every key given, valued by position.
    ///
    /// Wide rather than deep: a map with more entries than a cell can hold is the
    /// only shape that reaches the entries loop's own budget rule.
    @MainActor private static func wideMap(_ arena: Arena, keys: [String]) -> ArrowTable {
        let schema = arena.schema(format: "+m", name: "v")
        let entriesSchema = arena.schema(format: "+s", name: "entries")
        arena.attach(
            [arena.schema(format: "u", name: "key"), arena.schema(format: "i", name: "value")],
            to: entriesSchema)
        arena.attach([entriesSchema], to: schema)

        let values = arena.array(
            length: keys.count, buffers: [nil, arena.buffer(keys.indices.map { Int32($0) })])
        let entries = arena.array(length: keys.count, buffers: [nil])
        arena.attach([arena.strings(keys), values], to: entries)

        let array = arena.array(
            length: 1, buffers: [nil, arena.buffer([Int32(0), Int32(keys.count)])])
        arena.attach([entries], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `fixed_size_list<utf8>[2]` column: `[a,b]`, `[c,d]`, `[e,f]`.
    @MainActor private static func fixedLists(_ arena: Arena) -> ArrowTable {
        let schema = arena.schema(format: "+w:2", name: "v")
        arena.attach([arena.schema(format: "u", name: "")], to: schema)
        let array = arena.array(length: 3, buffers: [nil])
        arena.attach([arena.strings(["a", "b", "c", "d", "e", "f"])], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// One `list<struct<n: int32>>` column: `[{1},{2}]`, `[{3},{4}]`, `[]`.
    @MainActor private static func listsOfStructs(_ arena: Arena) -> ArrowTable {
        let schema = arena.schema(format: "+l", name: "v")
        let element = arena.schema(format: "+s", name: "item")
        arena.attach([arena.schema(format: "i", name: "n")], to: element)
        arena.attach([element], to: schema)

        let numbers = arena.array(length: 4, buffers: [nil, arena.buffer([Int32(1), 2, 3, 4])])
        let elements = arena.array(length: 4, buffers: [nil])
        arena.attach([numbers], to: elements)

        let array = arena.array(length: 3, buffers: [nil, arena.buffer([Int32(0), 2, 4, 4])])
        arena.attach([elements], to: array)
        return arena.table(schema: schema, array: array)
    }

    /// `levels` lists wrapped around an `int32`, as a schema and as its label.
    private static func chain(_ arena: Arena, levels: Int) -> UnsafeMutablePointer<ArrowSchema> {
        let outer = arena.schema(format: "+l", name: "v")
        var node = outer
        for _ in 1..<max(levels, 1) {
            let inner = arena.schema(format: "+l", name: "item")
            arena.attach([inner], to: node)
            node = inner
        }
        arena.attach([arena.schema(format: "i", name: "leaf")], to: node)
        return outer
    }

    private static func listLabel(_ levels: Int, around leaf: String) -> String {
        String(repeating: "list<", count: levels) + leaf + String(repeating: ">", count: levels)
    }

    // MARK: - Harness

    /// Drops the table before the arena, in that order and not the other.
    ///
    /// `ArrowTable` releases the batch it was given — which here is memory this
    /// file allocated — and the arena frees everything under it. Freeing the arena
    /// first would leave the table releasing a pointer already back with the
    /// allocator.
    @MainActor private static func release(_ table: ArrowTable, _ arena: Arena) {
        table.reset()
        arena.release()
    }

    private static func fail(_ what: String) {
        failures += 1
        fputs("nested FAIL: \(what)\n", stderr)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("nested FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}

/// Somewhere to build a C data interface tree, and a way to take it back down.
///
/// Every allocation is recorded so `release` can free it: a check that leaked
/// what it built would have worse memory discipline than the code it is about.
/// The one exception is the batch handed to `ArrowTable`, which owns and
/// deallocates it from the moment it is appended.
private final class Arena {
    private var schemas: [UnsafeMutablePointer<ArrowSchema>] = []
    private var arrays: [UnsafeMutablePointer<ArrowArray>] = []
    private var blocks: [UnsafeMutableRawPointer] = []

    // MARK: Schema

    func schema(format: String, name: String) -> UnsafeMutablePointer<ArrowSchema> {
        let node = UnsafeMutablePointer<ArrowSchema>.allocate(capacity: 1)
        node.initialize(to: ArrowSchema())
        node.pointee.format = UnsafePointer(cString(format))
        node.pointee.name = UnsafePointer(cString(name))
        node.pointee.metadata = nil
        node.pointee.flags = 0
        node.pointee.n_children = 0
        node.pointee.children = nil
        node.pointee.dictionary = nil
        node.pointee.release = nil
        node.pointee.private_data = nil
        schemas.append(node)
        return node
    }

    func attach(
        _ children: [UnsafeMutablePointer<ArrowSchema>],
        to parent: UnsafeMutablePointer<ArrowSchema>
    ) {
        let list = UnsafeMutablePointer<UnsafeMutablePointer<ArrowSchema>?>.allocate(
            capacity: children.count)
        for (at, child) in children.enumerated() { list[at] = child }
        blocks.append(UnsafeMutableRawPointer(list))
        parent.pointee.children = list
        parent.pointee.n_children = Int64(children.count)
    }

    // MARK: Array

    /// One array node. A nil buffer where the validity bitmap goes is how Arrow
    /// says every value in this array is present.
    func array(
        length: Int, offset: Int = 0, buffers: [UnsafeMutableRawPointer?]
    ) -> UnsafeMutablePointer<ArrowArray> {
        let node = UnsafeMutablePointer<ArrowArray>.allocate(capacity: 1)
        node.initialize(to: ArrowArray())
        node.pointee.length = Int64(length)
        node.pointee.null_count = -1
        node.pointee.offset = Int64(offset)
        node.pointee.n_buffers = Int64(buffers.count)
        node.pointee.n_children = 0
        node.pointee.children = nil
        node.pointee.dictionary = nil
        node.pointee.release = nil
        node.pointee.private_data = nil

        let list = UnsafeMutablePointer<UnsafeRawPointer?>.allocate(capacity: buffers.count)
        for (at, buffer) in buffers.enumerated() { list[at] = buffer.map { UnsafeRawPointer($0) } }
        blocks.append(UnsafeMutableRawPointer(list))
        node.pointee.buffers = list
        arrays.append(node)
        return node
    }

    func attach(
        _ children: [UnsafeMutablePointer<ArrowArray>], to parent: UnsafeMutablePointer<ArrowArray>
    ) {
        let list = UnsafeMutablePointer<UnsafeMutablePointer<ArrowArray>?>.allocate(
            capacity: children.count)
        for (at, child) in children.enumerated() { list[at] = child }
        blocks.append(UnsafeMutableRawPointer(list))
        parent.pointee.children = list
        parent.pointee.n_children = Int64(children.count)
    }

    /// A Utf8 array: an offsets buffer and the characters behind it.
    func strings(_ values: [String]) -> UnsafeMutablePointer<ArrowArray> {
        var offsets: [Int32] = [0]
        var bytes: [UInt8] = []
        for value in values {
            bytes.append(contentsOf: Array(value.utf8))
            offsets.append(Int32(bytes.count))
        }
        return array(length: values.count, buffers: [nil, buffer(offsets), buffer(bytes)])
    }

    // MARK: Buffers

    func buffer<T>(_ values: [T]) -> UnsafeMutableRawPointer {
        let size = max(MemoryLayout<T>.stride * values.count, 1)
        let block = UnsafeMutableRawPointer.allocate(
            byteCount: size, alignment: MemoryLayout<T>.alignment)
        values.withUnsafeBytes { raw in
            if let base = raw.baseAddress, raw.count > 0 {
                block.copyMemory(from: base, byteCount: raw.count)
            }
        }
        blocks.append(block)
        return block
    }

    /// A validity bitmap, one bit per value, least significant bit first.
    func validity(_ present: [Bool]) -> UnsafeMutableRawPointer {
        var bits = [UInt8](repeating: 0, count: present.count / 8 + 1)
        for (at, isPresent) in present.enumerated() where isPresent {
            bits[at / 8] |= UInt8(1 << (at % 8))
        }
        return buffer(bits)
    }

    // MARK: Handing it over

    /// The schema and the batch as `ArrowTable` receives them: the column under
    /// test, and a plain `int32` beside it so the cases can ask what a column that
    /// is *not* nested answers.
    @MainActor func table(
        schema: UnsafeMutablePointer<ArrowSchema>, array: UnsafeMutablePointer<ArrowArray>
    ) -> ArrowTable {
        let root = self.schema(format: "+s", name: "")
        attach([schema, self.schema(format: "i", name: "plain")], to: root)

        let rows = Int(array.pointee.length)
        let plain = self.array(
            length: rows, buffers: [nil, buffer([Int32](repeating: 0, count: max(rows, 1)))])

        // Allocated outside the arena: `ArrowTable` deallocates the batch it is
        // given, so from here this one pointer is the table's.
        let batch = UnsafeMutablePointer<ArrowArray>.allocate(capacity: 1)
        batch.initialize(to: ArrowArray())
        batch.pointee.length = Int64(rows)
        batch.pointee.null_count = -1
        batch.pointee.offset = 0
        batch.pointee.n_buffers = 1
        batch.pointee.dictionary = nil
        batch.pointee.release = nil
        batch.pointee.private_data = nil

        let none = UnsafeMutablePointer<UnsafeRawPointer?>.allocate(capacity: 1)
        none[0] = nil
        blocks.append(UnsafeMutableRawPointer(none))
        batch.pointee.buffers = none

        let columns = UnsafeMutablePointer<UnsafeMutablePointer<ArrowArray>?>.allocate(capacity: 2)
        columns[0] = array
        columns[1] = plain
        blocks.append(UnsafeMutableRawPointer(columns))
        batch.pointee.children = columns
        batch.pointee.n_children = 2

        let table = ArrowTable()
        table.setSchema(root)
        table.append(batch: batch)
        return table
    }

    func release() {
        for node in schemas { node.deallocate() }
        for node in arrays { node.deallocate() }
        for block in blocks { block.deallocate() }
        schemas.removeAll()
        arrays.removeAll()
        blocks.removeAll()
    }

    private func cString(_ text: String) -> UnsafeMutablePointer<CChar> {
        let bytes = Array(text.utf8) + [0]
        let block = UnsafeMutablePointer<CChar>.allocate(capacity: bytes.count)
        for (at, byte) in bytes.enumerated() { block[at] = CChar(bitPattern: byte) }
        blocks.append(UnsafeMutableRawPointer(block))
        return block
    }
}
