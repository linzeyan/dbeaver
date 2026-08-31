import AppKit
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
            checkAMongoDocumentColumnIsReadAsJSONAndItsNeighbourIsNot()
            checkAMultiLineValueArrivesWithItsLineBreaks()
            checkANullCellStartsEmptyRatherThanWithTheWord()
            checkABinaryValueIsRefusedRatherThanOfferedItsPreview()
            checkTheRowsObstacleAnswersBeforeTheValueIsLookedAt()
            checkAValueTooLongToLayOutIsRefusedWithItsLength()
            checkTheControlReadsTheSameAnswerAsThePane()
            checkEachPictureFormatIsKnownByItsFirstBytes()
            checkABlobThatIsNotAPictureIsNotTakenForOne()
            checkAPictureIsDrawnAndTheStripSaysWhatItIs()
            checkASignatureWithNoPictureBehindItFallsBackToTheBytes()
            checkAPictureTooLargeToDrawSaysSoAndKeepsItsBytes()
            checkAPictureIsStillNotSomethingThisCanEdit()
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

    /// MongoDB's inferred `document` type gets the JSON rendering, and the text
    /// catch-all beside it does not.
    ///
    /// The two halves of this agreement are in different languages — `shape.rs`
    /// names the type, this file reads the name — and nothing carries the string
    /// between them, so a check is the only thing holding them together. The
    /// second assertion is the one that matters as much: a collection's ObjectId
    /// columns arrive as `text`, and a rule loose enough to catch those would
    /// hand every one of them to a JSON parser that fails.
    @MainActor private static func checkAMongoDocumentColumnIsReadAsJSONAndItsNeighbourIsNot() {
        expect(
            ValueRendering.isJSONType("document"), true,
            "the name `ColumnType::Document` reports, spelled the same on this side")
        expect(
            ValueRendering.isJSONType("text"), false,
            "and the catch-all it was split out of stays text")

        let stored = "{\"city\":\"Taipei\",\"zip\":100}"
        let cell = cell(value: stored, type: "document", rendering: .json)
        expect(
            RenderedValue.make(from: cell).text.contains("\n"), true,
            "a document is laid out over lines rather than left as the one the driver sent")
        expect(
            ValueEdit.offered(for: cell, obstacle: nil), .editable(stored),
            "and the box still starts from what is stored, indentation and all")
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

    /// The two answers the strip's pencil reads agree with the case they came
    /// from.
    ///
    /// The button is enabled by one and captioned by the other, so an
    /// `isEditable` that drifted from its case would put a live pencil over a
    /// blob — and the box behind it is seeded from a payload that case does not
    /// have.
    private static func checkTheControlReadsTheSameAnswerAsThePane() {
        expect(ValueEdit.editable("x").isEditable, true, "an editable value has a box to open")
        expect(ValueEdit.editable("x").refusal, nil, "and nothing to explain")
        expect(ValueEdit.refused("no").isEditable, false, "a refused one has no box")
        expect(
            ValueEdit.refused("no").refusal, "no",
            "and carries its sentence, for the tooltip on the control it disabled")
    }

    // MARK: - Pictures

    /// Each of the five signatures is recognised, and the near misses are not.
    ///
    /// Written out as literal bytes rather than taken from an encoder, because
    /// what this checks is the table itself: a magic number copied down wrong
    /// would agree with an encoder's output only if the encoder were the one
    /// that wrote the table. The negatives beside each are the point of the
    /// case — `GIF88a` is not a GIF, a RIFF holding audio is not a WebP, and a
    /// PNG signature one byte short is not a PNG.
    private static func checkEachPictureFormatIsKnownByItsFirstBytes() {
        let png: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        expect(ValueRendering.imageFormat(of: png + [0, 0]), .png, "the PNG signature")
        expect(
            ValueRendering.imageFormat(of: Array(png.dropLast())), nil,
            "and seven eighths of it is not a PNG")

        expect(ValueRendering.imageFormat(of: [0xFF, 0xD8, 0xFF, 0xE0]), .jpeg, "JFIF")
        expect(ValueRendering.imageFormat(of: [0xFF, 0xD8, 0xFF, 0xE1]), .jpeg, "and Exif")
        expect(
            ValueRendering.imageFormat(of: [0xFF, 0xD8, 0x00]), nil,
            "and a start-of-image with no marker after it is neither")

        expect(ValueRendering.imageFormat(of: bytes("GIF87a")), .gif, "the older GIF")
        expect(ValueRendering.imageFormat(of: bytes("GIF89a")), .gif, "and the one in use")
        expect(
            ValueRendering.imageFormat(of: bytes("GIF88a")), nil,
            "and a version between them that was never written")

        expect(
            ValueRendering.imageFormat(of: bytes("RIFF") + [0, 0, 0, 0] + bytes("WEBP")),
            .webp, "a RIFF container holding WebP")
        expect(
            ValueRendering.imageFormat(of: bytes("RIFF") + [0, 0, 0, 0] + bytes("WAVE")), nil,
            "and the same container holding audio, which the first word alone would have taken")

        expect(ValueRendering.imageFormat(of: bytes("BM")), .bmp, "the two letters BMP has")
    }

    /// The blob that is not a picture, which is nearly every blob.
    ///
    /// The sniff exists as much for this as for the pictures: it is what keeps
    /// a twenty-megabyte `bytea` of protobuf away from a decoder that would
    /// read all of it to conclude the same thing.
    @MainActor private static func checkABlobThatIsNotAPictureIsNotTakenForOne() {
        let blob: [UInt8] = [0x08, 0x96, 0x01, 0x12, 0x07, 0x74, 0x65, 0x73, 0x74]
        expect(ValueRendering.imageFormat(of: blob), nil, "a protobuf is not a picture")
        expect(ValueRendering.imageFormat(of: []), nil, "and neither is nothing at all")

        let rendered = RenderedValue.make(
            from: cell(value: "\\x0896", type: "bytea", rendering: .binary(blob)))
        expect(rendered.image == nil, true, "so the pane draws no picture")
        expect(
            rendered.descriptor, "hex dump · 9 bytes",
            "and the strip says what it did, exactly as it did before pictures existed")
    }

    /// A real picture is drawn, and the strip names its format and its size.
    ///
    /// Encoded here rather than embedded, so what is fed in is a file with the
    /// header a real one has. Both formats on purpose: PNG carries its size in
    /// a fixed position and JPEG carries it in a segment that has to be walked
    /// to, and a header read that only ever met the easy one would be a header
    /// read nobody had checked.
    @MainActor private static func checkAPictureIsDrawnAndTheStripSaysWhatItIs() {
        for (format, name) in [(NSBitmapImageRep.FileType.png, "PNG"), (.jpeg, "JPEG")] {
            guard let picture = encoded(format, width: 24, height: 16) else {
                failures += 1
                fputs("value FAIL: could not encode a \(name) to check against\n", stderr)
                continue
            }
            let rendered = RenderedValue.make(
                from: cell(
                    value: ValueRendering.preview(bytes: picture), type: "bytea",
                    rendering: .binary(picture)))
            expect(rendered.image != nil, true, "a \(name) blob is drawn as a picture")
            expect(rendered.image?.width, 24, "at the width its header declares")
            expect(rendered.image?.height, 16, "and the height")
            expect(
                rendered.descriptor,
                "\(name) · 24 × 16 · \(AppModel.pluralized(picture.count, "byte"))",
                "and the strip names the format, the pixels and the stored size")
            expect(
                rendered.text, "",
                "with no hex dump behind it, which nothing would have drawn")
        }
    }

    /// A blob whose first bytes look like a signature and whose rest is not a
    /// picture goes back to being a blob.
    ///
    /// This is the whole reason the sniff is only the first of two gates. BMP's
    /// signature is the two letters `BM`, so a text column's worth of notes
    /// beginning "BMP export failed" sniffs as a bitmap; nothing is drawn on
    /// that answer alone, and what the reader gets is the hex dump they would
    /// have got anyway.
    @MainActor private static func checkASignatureWithNoPictureBehindItFallsBackToTheBytes() {
        let text = bytes("BMP export failed")
        expect(
            ValueRendering.imageFormat(of: text), .bmp,
            "the two-letter signature is matched, which is as much as it can say")
        let rendered = RenderedValue.make(
            from: cell(value: "\\x424d", type: "bytea", rendering: .binary(text)))
        expect(rendered.image == nil, true, "and nothing is drawn, because there is no header")
        expect(
            rendered.descriptor, "hex dump · \(text.count) bytes",
            "so the strip says hex dump rather than claiming a bitmap")
    }

    /// A picture past the decode cap is named and not drawn.
    ///
    /// The size still comes from the header — that read is bounded whatever the
    /// blob weighs — so the sentence can be specific about what is being
    /// refused. Saying "too large" without the numbers would leave a reader
    /// unable to tell a photograph from a corrupt column.
    @MainActor private static func checkAPictureTooLargeToDrawSaysSoAndKeepsItsBytes() {
        guard let small = encoded(.png, width: 24, height: 16) else {
            failures += 1
            fputs("value FAIL: could not encode a PNG to pad\n", stderr)
            return
        }
        // Padded past the cap rather than encoded at a size that would reach
        // it: a PNG stays readable with trailing bytes after its end chunk, and
        // encoding eight megabytes of real pixels would spend a second of every
        // run to check a comparison.
        let padded = small + [UInt8](repeating: 0, count: 9 * 1024 * 1024)
        let rendered = RenderedValue.make(
            from: cell(value: "\\x89504e47", type: "bytea", rendering: .binary(padded)))
        expect(rendered.image == nil, true, "past the cap nothing is decoded")
        expect(
            rendered.descriptor,
            "PNG · 24 × 16 · \(AppModel.pluralized(padded.count, "byte")) — too large to draw here",
            "and the strip names what it refused, from a header it could still afford to read")
        expect(
            rendered.text.hasPrefix("00000000  89 50 4e 47"), true,
            "with the bytes shown, which is what the reader is left able to look at")
    }

    /// A picture is still a binary value, and the editor still refuses it.
    ///
    /// Drawing a blob is a reading change and nothing more. The hazard it could
    /// have introduced is a pencil that lights up over a picture and seeds a box
    /// from `cell.value`, which for a binary cell is the first 64 bytes of the
    /// blob written out as hex — staging that would replace a photograph with a
    /// transcription of its own header.
    @MainActor private static func checkAPictureIsStillNotSomethingThisCanEdit() {
        guard let picture = encoded(.png, width: 24, height: 16) else {
            failures += 1
            fputs("value FAIL: could not encode a PNG to offer\n", stderr)
            return
        }
        expect(
            ValueEdit.offered(
                for: cell(
                    value: ValueRendering.preview(bytes: picture), type: "bytea",
                    rendering: .binary(picture)),
                obstacle: nil),
            .refused("A binary value cannot be edited here."),
            "the same refusal a blob got before it could be looked at")
    }

    // MARK: - Harness

    /// A real picture of a known size, written by the system encoder.
    ///
    /// By an encoder rather than as a literal, because these cases are about
    /// the header a real file has: a hand-typed one would check the size read
    /// against whatever was typed. The pixels are left as allocated — every
    /// case here reads the header and none of them looks at the picture.
    @MainActor private static func encoded(
        _ format: NSBitmapImageRep.FileType, width: Int, height: Int
    ) -> [UInt8]? {
        guard
            let canvas = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                bitsPerSample: 8, samplesPerPixel: 3, hasAlpha: false, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
            let data = canvas.representation(using: format, properties: [:])
        else { return nil }
        return [UInt8](data)
    }

    private static func bytes(_ text: String) -> [UInt8] { Array(text.utf8) }

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
