import Foundation

/// Executable checks for what the value viewer will let you edit, run by
/// `--verify-value`.
///
/// One decision is checked here and it is the one that can corrupt a column
/// without anybody noticing: which string the editor starts from. The pane draws
/// a rendering — JSON re-indented, a blob dumped as hex, anything past the cap
/// cut short — and a box seeded from that writes this program's formatting back
/// to the server on the first Stage. `ValueEdit.offered` answers with the stored
/// value or refuses, and every case below is about that difference.
///
/// What is not here: staging itself. Putting a value into `staged` needs a
/// browse result, which needs an Arrow table, which needs a server — the same
/// limit `RecordChecks` records at its head. The decision was put in a pure
/// function precisely so that what cannot be checked is a guard and a call.
///
/// There is no case asserting that NULL and a zero-length string seed the same
/// empty box. They do, deliberately, and a check saying so would pass whether
/// the rule was right or wrong — see `ValueEdit` for why the two are one answer.
enum ValueViewerChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        MainActor.assumeIsolated {
            checkTheBoxStartsFromWhatIsStoredAndNotFromTheRendering()
            checkAMultiLineValueArrivesWithItsLineBreaks()
            checkANullCellStartsEmptyRatherThanWithTheWord()
            checkABinaryValueIsRefusedRatherThanOfferedItsPreview()
            checkTheRowsObstacleAnswersBeforeTheValueIsLookedAt()
            checkAValueTooLongToLayOutIsRefusedWithItsLength()
        }
        if failures == 0 {
            fputs("value: all checks passed\n", stderr)
        } else {
            fputs("value: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// A `jsonb` cell offers the line the database sent, not the pretty-printed
    /// form on screen.
    ///
    /// This is the whole reason the decision is a function. The two strings are
    /// different — the second assertion proves it rather than assuming it — and
    /// wiring the box to `RenderedValue.text`, which is the obvious thing to do
    /// when the box replaces the pane that was drawing it, would re-indent every
    /// JSON document anybody opened and stage the result.
    @MainActor private static func checkTheBoxStartsFromWhatIsStoredAndNotFromTheRendering() {
        let stored = "{\"a\":1,\"b\":[2,3]}"
        let cell = cell(value: stored, type: "jsonb", rendering: .json)
        expect(
            ValueEdit.offered(for: cell, obstacle: nil), .editable(stored),
            "the stored document, character for character")
        expect(
            RenderedValue.make(from: cell).text == stored, false,
            "and the pane is showing something else, which is what makes this matter")
    }

    /// A value with newlines in it arrives whole.
    ///
    /// The same code path as the case above, and here on purpose: this is the
    /// requirement the editor exists for. A one-line field cannot hold a value
    /// with a line break in it, so a `text` column containing one could not be
    /// changed at all before this — and a check named after the reason is what
    /// stops the reason being optimised away.
    @MainActor private static func checkAMultiLineValueArrivesWithItsLineBreaks() {
        let stored = "first line\nsecond line\n\nfourth"
        expect(
            ValueEdit.offered(for: cell(value: stored, type: "text"), obstacle: nil),
            .editable(stored),
            "line breaks and the empty line between them are part of the value")
    }

    /// NULL seeds an empty box, not the word.
    @MainActor private static func checkANullCellStartsEmptyRatherThanWithTheWord() {
        // What `AppModel.cell(at:in:)` builds for a NULL: the word is what the
        // strip draws, and it is in `value` because the strip reads it there.
        let cell = cell(value: "NULL", type: "text", isNull: true)
        expect(
            ValueEdit.offered(for: cell, obstacle: nil), .editable(""),
            "typing nothing and staging writes an empty string, which is a choice; "
                + "seeding the word would write four characters nobody typed")
    }

    /// A binary cell is refused, and specifically is not offered its own
    /// preview.
    @MainActor private static func checkABinaryValueIsRefusedRatherThanOfferedItsPreview() {
        let bytes = [UInt8](repeating: 0xAB, count: 200)
        let preview = ValueRendering.preview(bytes: bytes)
        let cell = cell(value: preview, type: "bytea", rendering: .binary(bytes))
        expect(
            ValueEdit.offered(for: cell, obstacle: nil),
            .refused("A binary value cannot be edited here."),
            "said out loud rather than left as a box that quietly does not appear")
        expect(
            preview.hasSuffix("…"), true,
            "and the value it would otherwise have offered is a truncation of the blob")
    }

    /// The row's obstacle wins, even over a value that has one of its own.
    ///
    /// A relation with no key cannot have any cell written, so the sentence has
    /// to be the one about the relation. Answering "binary" there sends the
    /// reader off to convert a column that was never what stopped them.
    @MainActor private static func checkTheRowsObstacleAnswersBeforeTheValueIsLookedAt() {
        let obstacle = "A view has no rows of its own to change."
        expect(
            ValueEdit.offered(for: cell(value: "hello", type: "text"), obstacle: obstacle),
            .refused(obstacle),
            "passed through as written, so the pane and the strip cannot give two reasons")

        let bytes = [UInt8](repeating: 0, count: 4)
        expect(
            ValueEdit.offered(
                for: cell(value: "\\x00000000", type: "bytea", rendering: .binary(bytes)),
                obstacle: obstacle),
            .refused(obstacle),
            "and it is still the answer for a cell that would have been refused anyway")
    }

    /// The cap is the pane's cap, and the sentence carries the length.
    ///
    /// Checked on both sides of the boundary, because an editor that refuses one
    /// character early is indistinguishable from one that refuses one character
    /// late until somebody has a value of exactly that size.
    @MainActor private static func checkAValueTooLongToLayOutIsRefusedWithItsLength() {
        let cap = RenderedValue.characterCap
        let atCap = String(repeating: "x", count: cap)
        expect(
            ValueEdit.offered(for: cell(value: atCap, type: "text"), obstacle: nil),
            .editable(atCap),
            "a value the pane will draw in full is one the box can hold")

        let overCap = String(repeating: "x", count: cap + 1)
        guard
            case .refused(let sentence) = ValueEdit.offered(
                for: cell(value: overCap, type: "text"), obstacle: nil)
        else {
            failures += 1
            fputs("value FAIL: one character past the cap is refused\n", stderr)
            return
        }
        expect(
            sentence.contains(AppModel.formatted(cap + 1)), true,
            "and the sentence says how long it is, because \"too long\" alone "
                + "is not something a reader can act on")
    }

    // MARK: - Harness

    /// A cell as `AppModel` would describe it, with the fields this decision
    /// does not read left at whatever is cheapest to write.
    @MainActor private static func cell(
        value: String, type: String, isNull: Bool = false,
        rendering: ValueRendering = .text
    ) -> AppModel.InspectedCell {
        AppModel.InspectedCell(
            column: "c", type: type, value: value, isNull: isNull,
            address: "row 1", rendering: rendering,
            isExpanded: true, toggleExpanded: {})
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("value FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
