import Foundation

/// Executable checks for the Arrow field metadata reader, run by
/// `--verify-schema-metadata`.
///
/// One function is checked, `ArrowTable.declarations`, and it is checked because
/// it is the kind of code that fails silently. It walks a packed buffer of
/// counted strings with unaligned loads; a reader that mis-steps by four bytes
/// finds no key and answers the default, which is indistinguishable from a
/// column that declared nothing. The visible result would be the word NULL where
/// a blank belongs, or a document left on the one line the driver sent — a wrong
/// cell nobody would trace back to a pointer.
///
/// Two keys ride in that buffer now and one walk reads both, which is the reason
/// the second set of cases is here: the entries appear in no promised order, so
/// each key has to be found behind the other.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum SchemaMetadataChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
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
        if failures == 0 {
            fputs("schema-metadata: all checks passed\n", stderr)
        } else {
            fputs("schema-metadata: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

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
