import Foundation

/// Executable checks for the schema comparison, run by `--verify-diff`.
///
/// Behind a flag on the binary for the reason `PlanChecks` gives, and with the
/// same thing to prove: the report is composed by the core and crosses the FFI
/// boundary as JSON, so half of what can go wrong is the crossing. These run it
/// against two real SQLite files — no server, and still both handles.
@MainActor
enum SchemaDiffChecks {
    private static var failures = 0
    private static var scratch: [URL] = []
    private static var held: [Database] = []

    static func run() -> Bool {
        failures = 0
        checkTwoSchemasComeBackAsWhatTheyDoNotAgreeAbout()
        checkARelationOnOneSideIsOneLine()
        checkEverySideOfADifferenceReachesTheRow()
        checkTheSummaryTellsAgreementApartFromEmptiness()
        checkAComparisonCanBeAgainstTheConnectionItStartsFrom()
        checkTheSheetOpensOnAPairBothConnectionsHave()
        checkAComparisonWithNothingNamedIsRefusedRatherThanSent()
        for file in scratch { try? FileManager.default.removeItem(at: file) }
        scratch = []
        if failures == 0 {
            fputs("diff: all checks passed\n", stderr)
        } else {
            fputs("diff: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The whole path, from two open connections to the rows a sheet draws.
    ///
    /// The one difference between the two tables is a column that is NOT NULL on
    /// one side and not on the other — which is the difference a comparison that
    /// only matched names would miss entirely, and the one that decides whether a
    /// migration can run.
    private static func checkTwoSchemasComeBackAsWhatTheyDoNotAgreeAbout() {
        guard
            let left = opened([
                "CREATE TABLE invoice (id INTEGER PRIMARY KEY, sku TEXT, qty INTEGER NOT NULL)"
            ]),
            let right = opened([
                "CREATE TABLE invoice (id INTEGER PRIMARY KEY, sku TEXT, qty INTEGER)"
            ])
        else { return }
        guard let report = compared(left, right) else { return }

        expect(report.leftRelations, 1, "one relation on the left")
        expect(report.rightRelations, 1, "one on the right")
        expect(report.differences.count, 1, "and one thing they disagree about")
        guard let only = report.differences.first else { return }
        expect(only.table, "invoice", "in the table they share")
        expect(only.object, "qty", "about the column that changed")
        expect(only.kind, .column, "which is a column")
        expect(only.verdict, .changed, "present on both sides and described differently")
        expect(only.left.contains("not null"), true, "the left side says so: \(only.left)")
        expect(only.right.contains("not null"), false, "and the right does not: \(only.right)")
        // The columns the two agree about are not in the report at all. A
        // comparison that read a column's position would have called `id` and
        // `sku` changed too, and buried the one line that is news.
        expect(report.differences.contains { $0.object == "sku" }, false, "and nothing else is")
    }

    /// A table only one side has is one line, not one line per column it holds.
    ///
    /// Forty rows saying "this column is missing too" is the same news written
    /// forty times, and it buries the thirty-ninth table.
    private static func checkARelationOnOneSideIsOneLine() {
        guard
            let left = opened([
                "CREATE TABLE draft (id INTEGER, note TEXT, made TEXT, who TEXT)",
                "CREATE INDEX draft_who_idx ON draft (who)"
            ]),
            let right = opened([])
        else { return }
        guard let report = compared(left, right) else { return }
        expect(report.leftRelations, 1, "one relation on the left")
        expect(report.rightRelations, 0, "and an empty schema on the right")
        // Four columns and an index, and still one line: the news is that the
        // table is not there, said once.
        expect(report.differences.count, 1, "one line for the whole relation")
        guard let only = report.differences.first else { return }
        expect(only.kind, .relation, "and it is about the relation")
        expect(only.verdict, .onlyLeft, "which only the left has")
        expect(only.left, "table", "described as what it is")
        // The kind column takes the side's own word rather than a fixed one, so
        // a row about a view cannot be labelled "table" beside a cell saying
        // "view".
        expect(only.word, "table", "and the row names that too")
        expect(only.rightCell, SchemaDifference.absent, "with the other side blank")
        expect(only.marker, "◀", "and a glyph pointing at the side that has it")
    }

    /// What a row shows and what it says out loud, over the shapes SQLite cannot
    /// produce on its own.
    ///
    /// A fixture rather than a server here: this is about the mirror and the
    /// presentation, and the document is the core's own output shape — a field
    /// renamed on the far side fails to decode rather than quietly reading zero.
    private static func checkEverySideOfADifferenceReachesTheRow() {
        guard let report = decoded(document) else { return }
        expect(report.leftRelations, 12, "the counts crossed")
        expect(report.rightRelations, 11, "both of them")
        expect(report.differences.count, 4, "and every difference")

        let index = report.differences[0]
        expect(index.kind, .index, "an index")
        expect(index.marker, "◀", "only the left has it, and the glyph points there")
        expect(index.word, "index", "named for what it is")
        expect(index.rightCell, SchemaDifference.absent, "with nothing on the other side")
        expect(
            index.spoken(left: "prod.public", right: "staging.public"),
            "index invoice_sku_idx in invoice, only on prod.public: btree (sku)",
            "and read aloud as the side that has it")

        let key = report.differences[1]
        expect(key.kind, .foreignKey, "a foreign key")
        expect(key.word, "foreign key", "which is two words, not the JSON's one")
        expect(key.marker, "▶", "only on the right")
        expect(key.leftCell, SchemaDifference.absent, "so the left cell is blank")

        let constraint = report.differences[2]
        expect(constraint.marker, "≠", "a constraint both sides have and describe differently")
        expect(constraint.leftCell, "check ((qty > 0))", "each side keeps its own words")
        expect(constraint.rightCell, "check ((qty >= 0))", "including the one that changed")
        expect(
            constraint.spoken(left: "prod.public", right: "staging.public"),
            "constraint invoice_qty_check in invoice differs: "
                + "prod.public check ((qty > 0)), staging.public check ((qty >= 0))",
            "and both are read out, because the glyph points at a heading nobody heard")

        // A view the right side does not have: the kind column takes its word
        // from the side that has it rather than from the empty one.
        let view = report.differences[3]
        expect(view.kind, .relation, "a relation")
        expect(view.word, "view", "which is a view")
        expect(view.verdict, .onlyLeft, "only on the left")

        // Every row has to be its own row. Two differences that collapsed into
        // one id would draw one of them twice and lose the other.
        expect(Set(report.differences.map(\.id)).count, 4, "four rows, four identities")
    }

    /// "The two agree" and "there was nothing to read" are the same empty list
    /// and are not the same news. A login that can see nothing in the schema it
    /// named would otherwise be told the two schemas match.
    private static func checkTheSummaryTellsAgreementApartFromEmptiness() {
        let nothing = SchemaDiffReport(leftRelations: 0, rightRelations: 0, differences: [])
        expect(
            nothing.summary(left: "a", right: "b"),
            "Neither schema has anything in it to compare.", "an empty pair says so")

        let agreed = SchemaDiffReport(leftRelations: 12, rightRelations: 12, differences: [])
        expect(
            agreed.summary(left: "a", right: "b"),
            "No differences · 12 relations on each side", "and agreement says that")

        // The second half has to name what it is counting, or the numbers read
        // as a share of the differences rather than as what was looked at.
        guard let report = decoded(document) else { return }
        expect(
            report.summary(left: "prod.public", right: "staging.public"),
            "4 differences · 12 relations on prod.public, 11 on staging.public",
            "and a report names both sides")
    }

    /// Unlike a transfer, a comparison can be against the connection it started
    /// from: two schemas on one server is the ordinary case.
    private static func checkAComparisonCanBeAgainstTheConnectionItStartsFrom() {
        let model = makeModel()
        guard let here = opened([]) else { return }
        model.sessions[0].db = here
        model.sessions[0].connectionLabel = "prod"
        model.sessions[0].schemas = [SchemaInfo(name: "main", isSystem: false)]

        expect(model.transferTargets.isEmpty, true, "there is nowhere to send rows")
        expect(model.schemaDiffChoices.count, 1, "and still something to compare with")
        expect(
            model.schemaDiffChoices.first?.session === model.sessions[0], true,
            "which is this connection")
        expect(model.canCompareSchemas, true, "so the menu item is live")

        // The rules that keep a connection out are the same ones a transfer
        // uses, and they still apply.
        model.sessions[0].isBusy = true
        expect(model.schemaDiffChoices.isEmpty, true, "a busy connection is not offered")
        expect(model.canCompareSchemas, false, "and the item goes grey")
        model.sessions[0].isBusy = false
        model.sessions[0].db = nil
        expect(model.canCompareSchemas, false, "neither is a tab with nothing open in it")
    }

    /// The sheet opens on a pair both servers have, whatever it was left holding.
    ///
    /// A picker still naming last time's schema is not a cosmetic problem: it
    /// would be sent, and the comparison would fail against a schema this server
    /// does not have.
    private static func checkTheSheetOpensOnAPairBothConnectionsHave() {
        let model = makeModel()
        guard let here = opened([]), let there = opened([]) else { return }
        model.sessions[0].db = here
        model.sessions[0].schemas = [
            SchemaInfo(name: "pg_catalog", isSystem: true),
            SchemaInfo(name: "public", isSystem: false)
        ]
        model.presentConnection()
        guard model.sessions.count == 2 else {
            failures += 1
            fputs("diff FAIL: a second tab did not open\n", stderr)
            return
        }
        let other = model.sessions[1]
        other.db = there
        other.connectionLabel = "staging"
        other.schemas = [SchemaInfo(name: "archive", isSystem: false)]
        model.selectSession(0)

        // Left over from a connection that is not this one.
        model.schemaDiffLeftSchema = "somewhere_else"
        model.schemaDiffRightSchema = "somewhere_else"
        model.presentSchemaDiff()
        expect(model.isSchemaDiffOpen, true, "the sheet opens")
        // The engine's own schemas are offered but not defaulted to: comparing
        // two `pg_catalog`s is a real question and not the one anybody opens
        // this for.
        expect(model.schemaDiffLeftSchema, "public", "on a schema this connection has")
        expect(model.schemaDiffTarget, model.sessions[0].id, "against itself by default")
        expect(model.schemaDiffRightSchema, "public", "which is the same schema on both sides")

        // Changing the connection changes the schema under it, because a name
        // only the previous one had is a pair that cannot be read.
        model.schemaDiffTarget = other.id
        model.schemaDiffTargetChanged()
        expect(model.schemaDiffRightSchema, "archive", "the other connection's own schema")
        expect(model.schemaDiffLeftSchema, "public", "and the left side is left alone")

        // A connection closed under the open sheet. The picker falls back to
        // what is left, and the button has to mean the same connection the
        // picker is showing — a Compare that silently did nothing would read as
        // a broken button rather than as a connection that went away.
        other.db = nil
        expect(model.schemaDiffChoice?.session === model.sessions[0], true, "back to this one")
        model.schemaDiffLeftSchema = "main"
        model.schemaDiffRightSchema = "main"
        model.compareSchemas()
        expect(model.isComparingSchemas, true, "and Compare still compares something")
        // Named for the connection it fell back to, not the one that went away:
        // the headings on the report come from here.
        expect(
            model.status.contains(model.sessions[0].connectionLabel), true,
            "against the connection the picker is showing: \(model.status)")
    }

    /// A comparison that names no schema is refused where somebody can see it,
    /// rather than sent and failed by the server.
    private static func checkAComparisonWithNothingNamedIsRefusedRatherThanSent() {
        let model = makeModel()
        guard let here = opened([]) else { return }
        model.sessions[0].db = here
        model.schemaDiffTarget = model.sessions[0].id
        model.schemaDiffLeftSchema = ""
        model.schemaDiffRightSchema = "main"
        model.compareSchemas()
        expect(model.isComparingSchemas, false, "nothing was started")
        expect(model.errorMessage?.isEmpty == false, true, "and it says why")
        expect(model.sessions[0].isBusy, false, "with nothing left marked busy")
    }

    // MARK: - Fixtures

    /// A report of the shapes SQLite has no way to produce, in the core's own
    /// output form.
    private static let document = """
        {
          "left_relations": 12,
          "right_relations": 11,
          "differences": [
            {"table": "invoice", "object": "invoice_sku_idx", "kind": "index",
             "verdict": "only_left", "left": "btree (sku)", "right": ""},
            {"table": "invoice", "object": "invoice_customer_fk", "kind": "foreign_key",
             "verdict": "only_right", "left": "",
             "right": "(customer_id) -> public.customer (id) on delete cascade"},
            {"table": "invoice", "object": "invoice_qty_check", "kind": "constraint",
             "verdict": "changed", "left": "check ((qty > 0))", "right": "check ((qty >= 0))"},
            {"table": "paid", "object": "paid", "kind": "relation",
             "verdict": "only_left", "left": "view", "right": ""}
          ]
        }
        """

    private static func decoded(_ text: String) -> SchemaDiffReport? {
        guard let report = try? JSONDecoder().decode(SchemaDiffReport.self, from: Data(text.utf8))
        else {
            failures += 1
            fputs("diff FAIL: the report would not decode\n", stderr)
            return nil
        }
        return report
    }

    /// A scratch SQLite file with `statements` already run into it.
    ///
    /// The connection is kept for the life of the process rather than released
    /// with the check that opened it. One case here starts a comparison on the
    /// model's own queue and does not wait for it, and a handle freed while that
    /// call is inside the core would be a crash on the way out.
    private static func opened(_ statements: [String]) -> Database? {
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-diff-\(UUID().uuidString).db")
        scratch.append(file)
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("diff FAIL: a SQLite file would not open\n", stderr)
            return nil
        }
        held.append(db)
        for sql in statements {
            guard let query = try? db.query(sql, batchRows: 1) else {
                failures += 1
                fputs("diff FAIL: \(sql) was refused\n", stderr)
                return nil
            }
            while let batch = (try? query.nextBatch()) ?? nil { _ = batch }
        }
        return db
    }

    private static func compared(_ left: Database, _ right: Database) -> SchemaDiffReport? {
        guard let report = try? left.schemaDiff(of: "main", against: right, schema: "main") else {
            failures += 1
            fputs("diff FAIL: the two schemas would not compare\n", stderr)
            return nil
        }
        return report
    }

    private static func makeModel() -> AppModel {
        let store = ScratchDefaults.store("verify-diff")
        return AppModel(
            history: QueryHistory(defaults: store), favorites: QueryFavorites(defaults: store),
            preferences: Preferences(store: store))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("diff FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
