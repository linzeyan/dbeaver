import CDbFfi
import Foundation

/// Executable checks for the Arrow field metadata reader, run by
/// `--verify-schema-metadata`.
///
/// The function at the centre of them is `ArrowTable.declarations`, and it is
/// checked because it is the kind of code that fails silently. It walks a packed
/// buffer of counted strings with unaligned loads; a reader that mis-steps by
/// four bytes finds no key and answers the default, which is indistinguishable
/// from a column that declared nothing. The visible result would be the word
/// NULL where a blank belongs, or a document left on the one line the driver
/// sent — a wrong cell nobody would trace back to a pointer.
///
/// Three keys ride in that buffer now and one walk reads all of them, which is
/// the reason for the ordering cases: the entries appear in no promised order,
/// so each key has to be found behind the others.
///
/// The last section is about what the header does with the third of them.
/// Reading a declared type off the field is only half of it — a grid with three
/// possible answers about one column's type needs the order it believes them in
/// pinned down, because getting it wrong shows a coarser label than the one it
/// already had.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum SchemaMetadataChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheKeysAreSpelledTheWayTheCoreWritesThem()
        checkAColumnWithNoMetadataDeclaresNothing()
        checkTheDeclarationIsFoundWhenItIsTheOnlyEntry()
        checkTheDeclarationIsFoundBehindEntriesThatAreNotIt()
        checkAKeyThatIsNotTheDeclarationDecidesNothing()
        checkTheValueHasToSayOne()
        checkAKeyHoldingANulByteIsStillMatchedWhole()
        checkALengthBelowZeroIsRefusedRatherThanFollowed()
        checkTheShapeIsReadFromTheFieldAsWritten()
        checkTheTwoDeclarationsAreFoundInEitherOrder()
        checkAShapeThisReaderDoesNotActOnIsStillReported()
        checkTheDeclaredTypeIsReadFromTheFieldAsWritten()
        checkEveryDeclarationIsFoundWhateverTheOrder()
        checkTheColumnCarriesEverythingItsFieldDeclared()
        checkTheCatalogueIsBelievedOverTheResult()
        checkTheResultIsBelievedOverTheArrowKind()
        checkTheArrowKindAnswersWhenNothingWasDeclared()
        checkTheResultsSpellingIsShortenedTheSameWay()
        checkACapitalisedSpellingIsShortenedAndStaysCapitalised()
        if failures == 0 {
            fputs("schema-metadata: all checks passed\n", stderr)
        } else {
            fputs("schema-metadata: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The keys written out, which every other case here needs and none of them
    /// provides.
    ///
    /// They all build their fixtures from these constants, so all three could be
    /// renamed together and this file would pass while no key matched what the
    /// core writes. The other end is `dbconn::DECLARED_NOT_NULL`, `VALUE_SHAPE`,
    /// `SHAPE_JSON` and `DECLARED_TYPE`, spelled out in a test of their own for
    /// the same reason: the C data interface carries no shared header, so two
    /// sets of literal characters are the whole contract.
    private static func checkTheKeysAreSpelledTheWayTheCoreWritesThem() {
        expect(
            ArrowTable.declaredNotNullKey, "dbclient.declared_not_null", "the NOT NULL key")
        expect(ArrowTable.valueShapeKey, "dbclient.value_shape", "the value shape key")
        expect(ArrowTable.declaredTypeKey, "dbclient.declared_type", "the declared type key")
        expect(ArrowTable.jsonShape, "json", "the shape a column of documents is written in")
    }

    /// The common case by far: almost no column carries metadata, and the
    /// pointer is null rather than a buffer saying zero.
    private static func checkAColumnWithNoMetadataDeclaresNothing() {
        expect(ArrowTable.declarations(nil), ArrowTable.Declarations(), "a null metadata pointer")
        expect(read(packed([])), ArrowTable.Declarations(), "a buffer declaring no pairs at all")
    }

    private static func checkTheDeclarationIsFoundWhenItIsTheOnlyEntry() {
        expect(
            read(packed([(ArrowTable.declaredNotNullKey, "1")])).notNull, true,
            "the declaration alone")
    }

    /// The reader has to walk past entries it does not want, which is the step
    /// that goes wrong: every skip is two counted strings, not one.
    private static func checkTheDeclarationIsFoundBehindEntriesThatAreNotIt() {
        let blob = packed([
            ("duckdb.rendered_from", "List(Field { name: \"item\" })"),
            ("something.else", ""),
            (ArrowTable.declaredNotNullKey, "1")
        ])
        expect(read(blob).notNull, true, "the declaration reached after two other entries")
    }

    private static func checkAKeyThatIsNotTheDeclarationDecidesNothing() {
        expect(
            read(packed([("duckdb.rendered_from", "1")])).notNull, false,
            "another key holding \"1\"")
    }

    /// Absence is how a nullable column says so, so a key present with any other
    /// value must not be read as the declaration.
    private static func checkTheValueHasToSayOne() {
        expect(
            read(packed([(ArrowTable.declaredNotNullKey, "0")])).notNull, false,
            "the key set to \"0\"")
        expect(
            read(packed([(ArrowTable.declaredNotNullKey, "")])).notNull, false,
            "the key set to nothing")
    }

    /// Why the buffer cannot be read as a C string: a key containing NUL is
    /// legal here, and a reader that stopped at it would match a prefix.
    private static func checkAKeyHoldingANulByteIsStillMatchedWhole() {
        let blob = packed([
            ("dbclient.declared_not_null\0extra", "1"),
            (ArrowTable.declaredNotNullKey, "1")
        ])
        expect(read(blob).notNull, true, "a key that only starts like the declaration")
    }

    /// The core is what fills this buffer, so a negative length means memory has
    /// already gone wrong. Answering the defaults is the recoverable end of that.
    private static func checkALengthBelowZeroIsRefusedRatherThanFollowed() {
        var blob = [UInt8]()
        append(&blob, 1)
        append(&blob, -4)
        expect(read(blob), ArrowTable.Declarations(), "a key length below zero")
    }

    // MARK: - The shape a result declares for itself

    /// The key `dbconn::VALUE_SHAPE` writes, read back as written.
    ///
    /// Its whole job is to reach a column no catalogue describes — MongoDB's
    /// `_extra` is the one this was built for — so a spelling that drifted from
    /// the core's would leave that column as the single line it arrived on, with
    /// nothing anywhere saying the reader had looked for a name and not found it.
    private static func checkTheShapeIsReadFromTheFieldAsWritten() {
        expect(
            read(packed([(ArrowTable.valueShapeKey, ArrowTable.jsonShape)])).valueShape,
            "json", "the shape the core writes for a column of documents")
        expect(
            read(packed([("dbclient.value_type", "json")])).valueShape, "",
            "a key that is nearly it declares nothing")
    }

    /// Both keys, in both orders.
    ///
    /// Nothing promises which comes first — the core builds the map and Arrow
    /// packs it — so a reader that stopped walking at its first match would find
    /// whichever happened to be written first and answer the default for the
    /// other. That is a NOT NULL column drawn as nullable on exactly the fields
    /// that also declare a shape.
    private static func checkTheTwoDeclarationsAreFoundInEitherOrder() {
        let notNullFirst = read(
            packed([
                (ArrowTable.declaredNotNullKey, "1"),
                (ArrowTable.valueShapeKey, ArrowTable.jsonShape)
            ]))
        expect(notNullFirst.notNull, true, "the declaration written first")
        expect(notNullFirst.valueShape, "json", "and the shape behind it")

        let shapeFirst = read(
            packed([
                (ArrowTable.valueShapeKey, ArrowTable.jsonShape),
                (ArrowTable.declaredNotNullKey, "1")
            ]))
        expect(shapeFirst.valueShape, "json", "the shape written first")
        expect(shapeFirst.notNull, true, "and the declaration behind it")
    }

    /// A shape this build has no rendering for is reported rather than blanked.
    ///
    /// The reader's job is to say what the field said; deciding which shapes
    /// mean something is `AppModel.rendering`'s. Folding an unknown name to ""
    /// here would put the two decisions in one place and make the day a second
    /// shape is added a change in two files instead of one.
    private static func checkAShapeThisReaderDoesNotActOnIsStillReported() {
        expect(
            read(packed([(ArrowTable.valueShapeKey, "xml")])).valueShape, "xml",
            "a shape nothing renders yet")
    }

    // MARK: - The type a result declares for itself

    /// The key `dbconn::DECLARED_TYPE` writes, read back as written.
    ///
    /// Read rather than interpreted, and the two entries below are the reason:
    /// `VARCHAR` and `varchar(64)` come from different databases with different
    /// spellings, and anything this reader normalised would be a type name it
    /// invented rather than one a server used.
    private static func checkTheDeclaredTypeIsReadFromTheFieldAsWritten() {
        expect(
            read(packed([(ArrowTable.declaredTypeKey, "VARCHAR")])).declaredType, "VARCHAR",
            "the type DuckDB declares for a text column")
        expect(
            read(packed([(ArrowTable.declaredTypeKey, "numeric(12,2)")])).declaredType,
            "numeric(12,2)", "a type whose parameters decide how it compares")
        expect(
            read(packed([("dbclient.declared_types", "VARCHAR")])).declaredType, "",
            "a key that is nearly it declares nothing")
    }

    /// All three keys, in three orders that each put a different one first.
    ///
    /// Nothing promises the order — the core builds a map and Arrow packs it — so
    /// a reader that stopped at its first match would answer the default for
    /// whichever keys happened to be written behind it. Three entries make that
    /// failure invisible in a way two did not: with a single walk and a single
    /// early exit, two of the three would still be right.
    private static func checkEveryDeclarationIsFoundWhateverTheOrder() {
        let entries = [
            (ArrowTable.declaredNotNullKey, "1"),
            (ArrowTable.valueShapeKey, ArrowTable.jsonShape),
            (ArrowTable.declaredTypeKey, "jsonb")
        ]
        for first in entries.indices {
            let rotated = Array(entries[first...] + entries[..<first])
            let found = read(packed(rotated))
            expect(found.notNull, true, "the declaration with \(rotated[0].0) written first")
            expect(found.valueShape, "json", "the shape with \(rotated[0].0) written first")
            expect(found.declaredType, "jsonb", "the type with \(rotated[0].0) written first")
        }
    }

    /// And the column reaches the header carrying all three.
    ///
    /// The step between the two halves of this file: everything above reads a
    /// buffer, everything below decides what to draw, and `setSchema` is the one
    /// line that carries the first to the second. A field dropped there fails
    /// silently in the shape this whole file exists for — the column would answer
    /// the default, the header would fall through to the Arrow kind by the rule
    /// pinned down below, and the fallback doing its job looks exactly like a
    /// column that declared nothing.
    private static func checkTheColumnCarriesEverythingItsFieldDeclared() {
        let column = column(
            declaring: [
                (ArrowTable.declaredNotNullKey, "1"),
                (ArrowTable.valueShapeKey, ArrowTable.jsonShape),
                (ArrowTable.declaredTypeKey, "jsonb")
            ])
        expect(column.declaredNotNull, true, "the declaration on the column it was written for")
        expect(column.valueShape, "json", "the shape on that column")
        expect(column.declaredType, "jsonb", "the type on that column")
    }

    // MARK: - Which answer the header believes

    /// The catalogue wins where there is one, because it is the fuller answer.
    ///
    /// Measured against DuckDB, which is where the two disagree most: a named
    /// `ENUM` is `status` in `information_schema` and `ENUM` through the C API,
    /// because DuckDB does not hand a type's own name to the second one.
    /// Believing the result there would replace a name somebody declared with the
    /// family it belongs to.
    private static func checkTheCatalogueIsBelievedOverTheResult() {
        expect(
            GridRenderer.typeLabel(catalogue: "status", declared: "ENUM", kind: .utf8), "status",
            "the catalogue's name for a column the result calls ENUM")
        // An empty catalogue entry is not an answer. A relation whose column is
        // in no catalogue row reaches here as nil; one that answered with a blank
        // has said nothing either way, and taking it would put an empty line
        // under a name that had a type to show.
        expect(
            GridRenderer.typeLabel(catalogue: "", declared: "BIGINT", kind: .int64), "BIGINT",
            "a catalogue that answered with a blank")
    }

    /// And the result wins over the buffer it arrived in, which is the whole
    /// point of the key. `jsonb`, `text` and `varchar(64)` are one `utf8` to
    /// Arrow, so before this the query pane called all three the same thing.
    private static func checkTheResultIsBelievedOverTheArrowKind() {
        expect(
            GridRenderer.typeLabel(catalogue: nil, declared: "jsonb", kind: .utf8), "jsonb",
            "a query pane column the result declared")
    }

    /// With the Arrow kind left as the answer for a column nothing declared —
    /// a computed one, or a driver whose protocol carries no type name. It is a
    /// true statement about what arrived, which is the test every branch here has
    /// to pass.
    private static func checkTheArrowKindAnswersWhenNothingWasDeclared() {
        expect(
            GridRenderer.typeLabel(catalogue: nil, declared: "", kind: .int64), "int64",
            "a column with no declaration anywhere")
        expect(
            GridRenderer.typeLabel(
                catalogue: nil, declared: "", kind: .decimal128(precision: 12, scale: 2)),
            "decimal(12,2)", "and one whose Arrow type carries its parameters")
    }

    /// The result's spelling goes through the same shortening the catalogue's
    /// does. One rule for both, or `timestamp without time zone` reaching the
    /// header from a query would be cut at the width where it stops differing
    /// from `timestamp with time zone` — which is the distinction the label is
    /// there to draw.
    private static func checkTheResultsSpellingIsShortenedTheSameWay() {
        expect(
            GridRenderer.typeLabel(
                catalogue: nil, declared: "timestamp(3) with time zone",
                kind: .timestamp(tz: true, unit: .micro)),
            "timestamptz(3)", "a zoned timestamp declared by the result")
        expect(
            GridRenderer.typeLabel(catalogue: nil, declared: "character varying(64)", kind: .utf8),
            "varchar(64)", "and a spelling the server itself accepts twice over")
    }

    /// And it reads the capitals a C API answers in.
    ///
    /// The case is the whole of this: DuckDB hands over `TIMESTAMP WITH TIME
    /// ZONE` where PostgreSQL's `format_type` writes the same type in lower case,
    /// and a rule that matched only one of them left the other cut at
    /// `TIMESTAMP WITH TIME…` — the column's zone, which is the reason the label
    /// is on screen, falling off the end. The answer follows the case it was
    /// asked in, because `TIMESTAMPtz` beside `VARCHAR` and `DECIMAL(18,6)` reads
    /// as a defect in the header rather than as a type a server would accept.
    private static func checkACapitalisedSpellingIsShortenedAndStaysCapitalised() {
        expect(
            GridRenderer.typeLabel(
                catalogue: nil, declared: "TIMESTAMP WITH TIME ZONE",
                kind: .timestamp(tz: true, unit: .micro)),
            "TIMESTAMPTZ", "the type DuckDB's C API declares for a zoned timestamp")
        expect(
            GridRenderer.typeLabel(
                catalogue: nil, declared: "TIME WITH TIME ZONE", kind: .time64(unit: .micro)),
            "TIMETZ", "and the zoned time beside it")
        expect(
            GridRenderer.typeLabel(catalogue: nil, declared: "CHARACTER VARYING(64)", kind: .utf8),
            "VARCHAR(64)", "a rewrite from the other end of the rule, asked in capitals")
    }

    // MARK: - Harness

    /// The C data interface's packed form, built the way the core writes it.
    private static func packed(_ pairs: [(String, String)]) -> [UInt8] {
        var blob = [UInt8]()
        append(&blob, Int32(pairs.count))
        for (key, value) in pairs {
            append(&blob, Int32(key.utf8.count))
            blob.append(contentsOf: Array(key.utf8))
            append(&blob, Int32(value.utf8.count))
            blob.append(contentsOf: Array(value.utf8))
        }
        return blob
    }

    /// One field carrying that metadata, read back as the column `GridRenderer`
    /// is handed.
    ///
    /// A schema the way the core sends one: a struct root with the field as its
    /// child. `setSchema` copies out everything it keeps, so all of this is freed
    /// before the column leaves — and none of it is released, because nothing
    /// here was allocated by Arrow.
    private static func column(declaring pairs: [(String, String)]) -> ArrowTable.Column {
        var blocks: [UnsafeMutableRawPointer] = []
        func bytes(_ values: [UInt8]) -> UnsafeMutablePointer<CChar> {
            let block = UnsafeMutablePointer<CChar>.allocate(capacity: max(values.count, 1))
            for (at, byte) in values.enumerated() { block[at] = CChar(bitPattern: byte) }
            blocks.append(UnsafeMutableRawPointer(block))
            return block
        }
        func node(format: String, name: String) -> UnsafeMutablePointer<ArrowSchema> {
            let node = UnsafeMutablePointer<ArrowSchema>.allocate(capacity: 1)
            node.initialize(to: ArrowSchema())
            node.pointee.format = UnsafePointer(bytes(Array(format.utf8) + [0]))
            node.pointee.name = UnsafePointer(bytes(Array(name.utf8) + [0]))
            blocks.append(UnsafeMutableRawPointer(node))
            return node
        }

        let field = node(format: "u", name: "doc")
        field.pointee.metadata = UnsafePointer(bytes(packed(pairs)))
        let children = UnsafeMutablePointer<UnsafeMutablePointer<ArrowSchema>?>.allocate(
            capacity: 1)
        children[0] = field
        blocks.append(UnsafeMutableRawPointer(children))
        let root = node(format: "+s", name: "")
        root.pointee.children = children
        root.pointee.n_children = 1

        let table = ArrowTable()
        table.setSchema(root)
        defer { for block in blocks { block.deallocate() } }
        return table.columns[0]
    }

    private static func append(_ blob: inout [UInt8], _ value: Int32) {
        withUnsafeBytes(of: value) { blob.append(contentsOf: $0) }
    }

    private static func read(_ blob: [UInt8]) -> ArrowTable.Declarations {
        blob.withUnsafeBytes { raw in
            ArrowTable.declarations(raw.baseAddress!.assumingMemoryBound(to: CChar.self))
        }
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("schema-metadata FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
