import Foundation

/// Executable checks for the metadata seam, run by `--verify-metadata`.
///
/// One rule, applied to every struct the navigator and the Structure tab decode:
/// a field the core declares optional has to be optional here, and a payload
/// with it null has to decode. Nothing else is checked — what the values mean is
/// the core's business and is checked against real servers.
///
/// This exists because the rule was broken and the failure was invisible.
/// `TriggerInfo` declared `timing`, `level` and `function` non-optional while
/// the core had sent all three as `Option<String>` since the day it was written.
/// PostgreSQL fills them, so every check passed; MySQL keeps the statement a
/// trigger was created from and none of the four, so decoding the triggers of a
/// MySQL table threw — and because the window routes every background failure
/// through one handler, the throw abandoned the browse that was in flight
/// beside it. The symptom was a MySQL table with a trigger showing no rows,
/// which points nowhere near a JSON field name.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum MetadataChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkATriggerTheDatabaseBarelyDescribesStillDecodes()
        checkATriggerWithAFunctionReadsAsACall()
        checkEveryOptionalFieldArrivesNull()
        checkARenamedFieldIsRefusedRatherThanGuessed()
        if failures == 0 {
            fputs("metadata: all checks passed\n", stderr)
        } else {
            fputs("metadata: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// MySQL's answer: a name, whether it is enabled, and the statement it was
    /// created from. Everything else is null, and that is not an error.
    private static func checkATriggerTheDatabaseBarelyDescribesStillDecodes() {
        let trigger: TriggerInfo? = decode(
            """
            {"name":"bench_child_stamp","timing":null,"events":[],"level":null,
             "function":null,"enabled":true,
             "definition":"BEGIN\\n  SET NEW.qty = NEW.qty;\\nEND"}
            """)
        expect(trigger?.name, "bench_child_stamp", "the trigger decodes")
        expect(trigger?.timing, nil, "with no timing the catalog could give")
        expect(
            trigger?.runsLabel, "BEGIN   SET NEW.qty = NEW.qty; END",
            "and the pane shows the body, because there is no function to name")
    }

    /// PostgreSQL's answer, where the same fields are all present.
    private static func checkATriggerWithAFunctionReadsAsACall() {
        let trigger: TriggerInfo? = decode(
            """
            {"name":"bench_child_audit","timing":"BEFORE","events":["INSERT","UPDATE"],
             "level":"ROW","function":"audit_row","enabled":true,"definition":null}
            """)
        expect(trigger?.whenLabel, "BEFORE INSERT, UPDATE · ROW", "when it fires")
        expect(trigger?.runsLabel, "audit_row()", "and what it runs")
    }

    /// Every other struct, with each optional field the core declares set to
    /// null at once. A decode that throws here is a pane that cannot draw.
    private static func checkEveryOptionalFieldArrivesNull() {
        let relation: RelationInfo? = decode(
            #"{"schema":"bench","name":"orders","kind":"table","estimated_rows":null}"#)
        // `rowsLabel` is not read here: it is main-actor isolated because it
        // shares the window's number formatter, and what matters at this seam is
        // that the absent estimate survives the decode as an absence.
        expect(relation?.estimatedRows, nil, "a relation nothing has analysed")

        let index: IndexInfo? = decode(
            """
            {"name":"orders_pkey","columns":["id"],"is_unique":true,"is_primary":true,
             "method":"btree","predicate":null}
            """)
        expect(index?.predicate, nil, "an index over every row")

        let column: ColumnInfo? = decode(
            """
            {"name":"note","data_type":"text","nullable":true,"position":7,
             "is_primary_key":false,"default_value":null}
            """)
        expect(column?.defaultValue, nil, "a column with no default")
    }

    /// A field the core renames stops the decode instead of arriving as a
    /// plausible default, for the reason `TransactionChecks` gives: a wrong
    /// value that draws is worse than a refusal that says so.
    private static func checkARenamedFieldIsRefusedRatherThanGuessed() {
        let missingEvents: TriggerInfo? = decode(
            #"{"name":"t","timing":null,"level":null,"function":null,"enabled":true,"definition":null}"#
        )
        expect(missingEvents == nil, true, "a missing `events` is refused")
    }

    // MARK: - Harness

    private static func decode<T: Decodable>(_ json: String) -> T? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("metadata FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
