import Foundation

/// Executable checks for the Arrow field metadata reader, run by
/// `--verify-schema-metadata`.
///
/// One function is checked, `ArrowTable.declaresNotNull`, and it is checked
/// because it is the kind of code that fails silently. It walks a packed buffer
/// of counted strings with unaligned loads; a reader that mis-steps by four
/// bytes finds no key and answers false, which is indistinguishable from a
/// column that was never declared NOT NULL. The visible result would be the word
/// NULL where a blank belongs — a wrong cell nobody would trace back to a
/// pointer.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum SchemaMetadataChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAColumnWithNoMetadataIsNotDeclaredNotNull()
        checkTheDeclarationIsFoundWhenItIsTheOnlyEntry()
        checkTheDeclarationIsFoundBehindEntriesThatAreNotIt()
        checkAKeyThatIsNotTheDeclarationDecidesNothing()
        checkTheValueHasToSayOne()
        checkAKeyHoldingANulByteIsStillMatchedWhole()
        checkALengthBelowZeroIsRefusedRatherThanFollowed()
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
    private static func checkAColumnWithNoMetadataIsNotDeclaredNotNull() {
        expect(ArrowTable.declaresNotNull(nil), false, "a null metadata pointer")
        expect(read(packed([])), false, "a buffer declaring no pairs at all")
    }

    private static func checkTheDeclarationIsFoundWhenItIsTheOnlyEntry() {
        expect(read(packed([(ArrowTable.declaredNotNullKey, "1")])), true, "the declaration alone")
    }

    /// The reader has to walk past entries it does not want, which is the step
    /// that goes wrong: every skip is two counted strings, not one.
    private static func checkTheDeclarationIsFoundBehindEntriesThatAreNotIt() {
        let blob = packed([
            ("duckdb.rendered_from", "List(Field { name: \"item\" })"),
            ("something.else", ""),
            (ArrowTable.declaredNotNullKey, "1")
        ])
        expect(read(blob), true, "the declaration reached after two other entries")
    }

    private static func checkAKeyThatIsNotTheDeclarationDecidesNothing() {
        expect(read(packed([("duckdb.rendered_from", "1")])), false, "another key holding \"1\"")
    }

    /// Absence is how a nullable column says so, so a key present with any other
    /// value must not be read as the declaration.
    private static func checkTheValueHasToSayOne() {
        expect(read(packed([(ArrowTable.declaredNotNullKey, "0")])), false, "the key set to \"0\"")
        expect(read(packed([(ArrowTable.declaredNotNullKey, "")])), false, "the key set to nothing")
    }

    /// Why the buffer cannot be read as a C string: a key containing NUL is
    /// legal here, and a reader that stopped at it would match a prefix.
    private static func checkAKeyHoldingANulByteIsStillMatchedWhole() {
        let blob = packed([
            ("dbclient.declared_not_null\0extra", "1"),
            (ArrowTable.declaredNotNullKey, "1")
        ])
        expect(read(blob), true, "a key that only starts like the declaration")
    }

    /// The core is what fills this buffer, so a negative length means memory has
    /// already gone wrong. Answering false is the recoverable end of that.
    private static func checkALengthBelowZeroIsRefusedRatherThanFollowed() {
        var blob = [UInt8]()
        append(&blob, 1)
        append(&blob, -4)
        expect(read(blob), false, "a key length below zero")
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

    private static func read(_ blob: [UInt8]) -> Bool {
        blob.withUnsafeBytes { raw in
            ArrowTable.declaresNotNull(raw.baseAddress!.assumingMemoryBound(to: CChar.self))
        }
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("schema-metadata FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
