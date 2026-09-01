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
        checkCapabilitiesDecodeTheKeysTheCoreWrites()
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

        // Both halves of the identity, because the two are what the editing
        // controls read and each is null in the other's case.
        let named: RowIdentity? = decode(#"{"columns":["id"],"obstacle":null}"#)
        expect(named?.columns, ["id"], "a table a row can be named in")
        expect(named?.obstacle, nil, "and nothing to explain")

        let refused: RowIdentity? = decode(
            """
            {"columns":[],"obstacle":"bench.audit has no primary key, and the unique key \
            audit_email_key is over email, which can be null, so there is no way to name \
            one row of it"}
            """)
        expect(refused?.columns, [], "a table nothing names a row of")
        expect(
            refused?.obstacle?.contains("audit_email_key"), true,
            "and the sentence names the constraint that was turned down")
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

    /// The two spellings have to agree, and nothing else makes them.
    ///
    /// The core writes Rust field names and Swift reads Swift ones, so
    /// `cancel_stops_the_statement` is bridged by a `CodingKeys` entry — one line
    /// whose deletion compiles, passes every other check, and silently decodes
    /// nothing. What it decodes to is `false`, which reads as a real answer: the
    /// window would tell somebody on PostgreSQL that Cancel does not reach the
    /// server, and it would be wrong in the direction that sounds cautious.
    private static func checkCapabilitiesDecodeTheKeysTheCoreWrites() {
        let both: Capabilities? = decode(
            #"""
            {"transactional":true,"cancel_stops_the_statement":true,"switches_database":true,
             "writes_statements":true,"schema_is_the_database":false,"reports_routines":true,
             "reports_sequences":true,"server_processes":"interruptible",
             "reports_variables":true,"changes_relations":true,"changes_columns":true,
             "alters_columns":true,"changes_databases":true}
            """#)
        expect(both?.transactional, true, "a transactional connection says so")
        expect(both?.cancelStopsTheStatement, true, "and that its cancel reaches the server")
        expect(both?.switchesDatabase, true, "and that a database in the tree is somewhere to go")
        expect(both?.writesStatements, true, "and that the core can write a statement for it")
        expect(
            both?.schemaIsTheDatabase, false,
            "and that its schemas are schemas, since it has a level of databases above them")
        expect(both?.reportsRoutines, true, "and that it can list its functions and procedures")
        expect(both?.reportsSequences, true, "and its sequences, which is a separate question")
        // A word rather than a bool, and the one field here whose wire form can
        // be wrong without being absent: a spelling the core does not write
        // decodes to nothing at all, which is why this names the value.
        expect(
            both?.serverProcesses, .interruptible,
            "and that its sessions can be both listed and interrupted")
        expect(
            both?.reportsVariables, true,
            "and that the settings it is running with can be read")
        expect(
            both?.changesRelations, true,
            "and that the core writes a drop, an empty and a rename for it")
        expect(
            both?.changesColumns, true,
            "and an add, a drop and a rename for one of a table's columns")
        expect(
            both?.altersColumns, true,
            "and an ALTER COLUMN for one, which is the narrower of the two")
        expect(
            both?.changesDatabases, true,
            "and a create and a drop for a whole database, which is a separate question")

        // Cassandra's answer, which is the one `cancel_stops_the_statement`
        // exists to carry — and Redis's for the field beside it.
        let neither: Capabilities? = decode(
            #"""
            {"transactional":false,"cancel_stops_the_statement":false,"switches_database":false,
             "writes_statements":false,"schema_is_the_database":true,"reports_routines":false,
             "reports_sequences":false,"server_processes":"unreported",
             "reports_variables":false,"changes_relations":false,"changes_columns":false,
             "alters_columns":false,"changes_databases":false}
            """#)
        expect(neither?.cancelStopsTheStatement, false, "a cancel that never leaves this side")
        expect(
            neither?.writesStatements, false,
            "and a database this build has no grammar to write for")
        expect(
            neither?.schemaIsTheDatabase, true,
            "while its one level of containers is what Redis itself calls databases")
        expect(
            neither?.reportsRoutines, false,
            "and that there are no routines to list, Redis having no such object")
        expect(
            neither?.reportsSequences, false,
            "nor sequences — a counter here is a key, and the tree already has it")
        expect(
            neither?.serverProcesses, .unreported,
            "and nothing to say about what the server is doing, so the menu item stays shut")
        expect(
            neither?.reportsVariables, false,
            "nor about what it is configured with, which is the item beside it")
        expect(
            neither?.changesRelations, false,
            "and no statement for changing a relation, so the row menu is not drawn")
        expect(
            neither?.changesColumns, false,
            "nor for a column, so the Structure tab draws no controls over its columns")
        expect(
            neither?.altersColumns, false,
            "nor for altering one, which SQLite answers differently from the field above")
        expect(
            neither?.changesDatabases, false,
            "nor for making one, so New Database is greyed")

        // A field the core stopped writing is refused rather than read as false,
        // for the same reason `checkARenamedFieldIsRefusedRatherThanGuessed`
        // exists: a default here is an answer nobody gave.
        let renamed: Capabilities? = decode(
            #"""
            {"transactional":true,"cancel_stops_statement":true,"switches_database":false,
             "writes_statements":true,"schema_is_the_database":false,"reports_routines":true,
             "reports_sequences":true,"server_processes":"unreported",
             "reports_variables":false,"changes_relations":false,"changes_columns":false,
             "alters_columns":false,"changes_databases":false}
            """#)
        expect(renamed == nil, true, "a key the core no longer writes is not guessed at")

        expect(
            Capabilities.unknown.cancelStopsTheStatement, false,
            "and before asking, no promise is made")
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
