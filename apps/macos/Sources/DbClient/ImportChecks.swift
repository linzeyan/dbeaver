import Foundation

/// Executable checks for which files import reads, run by `--verify-import`.
///
/// The reading itself is the core's, and so is everything that could go wrong
/// with a value once it is read — those are checked in `crates/transfer`, and
/// restating any of it here would be a second copy of a rule.
///
/// What is this side's own is the one decision the file extension makes: which
/// of the five export formats can be read back, and which cannot. Getting that
/// wrong does not fail to compile and does not fail at the panel either. It
/// fails at the point where somebody picks a `.sql` file expecting their script
/// to run and instead has its text inserted as rows — or where a format that
/// reads perfectly well is quietly missing from the panel and nobody notices
/// for a year.
enum ImportChecks {
    private static var failures = 0

    static func run() -> Bool {
        checkAnExtensionChoosesItsFormatWhateverItsCase()
        checkEveryImportableFormatIsReachableByItsOwnExtension()
        checkASqlScriptIsNotSomethingToImport()
        checkAnExtensionNothingReadsChoosesNothing()
        if failures == 0 {
            fputs("import: all checks passed\n", stderr)
        } else {
            fputs("import: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    /// A file picked from the Finder carries whatever case its author typed.
    private static func checkAnExtensionChoosesItsFormatWhateverItsCase() {
        expect(ExportFormat(importPathExtension: "csv"), .csv, "csv")
        expect(ExportFormat(importPathExtension: "CSV"), .csv, "CSV")
        expect(ExportFormat(importPathExtension: "Csv"), .csv, "Csv")
        expect(ExportFormat(importPathExtension: "tsv"), .tsv, "tsv")
        expect(ExportFormat(importPathExtension: "jsonl"), .jsonl, "jsonl")
        expect(ExportFormat(importPathExtension: "parquet"), .parquet, "parquet")
    }

    /// A format added later cannot be silently unreachable.
    ///
    /// The panel offers what `canImport` says, and the file that comes back is
    /// resolved by extension. If those two ever disagree, the panel offers a
    /// file it then refuses to open.
    private static func checkEveryImportableFormatIsReachableByItsOwnExtension() {
        for format in ExportFormat.allCases where format.canImport {
            expect(
                ExportFormat(importPathExtension: format.fileExtension), format,
                "\(format.label) is offered, so its own extension must resolve to it")
        }
    }

    /// Deliberate, not an oversight.
    ///
    /// A `.sql` file is a script. What a script wants is to be run, and this
    /// application runs one in an editor that shows the statements first.
    /// Importing it would insert its text as rows.
    private static func checkASqlScriptIsNotSomethingToImport() {
        expect(ExportFormat(importPathExtension: "sql"), nil, "sql")
        expect(ExportFormat(importPathExtension: "SQL"), nil, "SQL")
        // Still an export format, though — this is a rule about reading, not a
        // format being withdrawn.
        expect(ExportFormat(pathExtension: "sql"), .sql, "sql is still exported")
    }

    private static func checkAnExtensionNothingReadsChoosesNothing() {
        expect(ExportFormat(importPathExtension: "txt"), nil, "txt")
        expect(ExportFormat(importPathExtension: "xlsx"), nil, "xlsx")
        expect(ExportFormat(importPathExtension: ""), nil, "no extension at all")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("import FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
