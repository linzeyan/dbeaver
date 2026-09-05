import CoreGraphics
import Foundation

/// Executable checks for the schema diagram, run by `--verify-schema-diagram`.
///
/// A picture is the one thing here that cannot be checked, so none of this looks
/// at pixels. What is checked is everything the picture is decided by: which
/// tables get a box, which keys get a line, where the boxes go, what the cap
/// leaves out, and what the sheet does with an answer that arrives after the
/// question changed. Two of them go through a real SQLite file, because the keys
/// cross the FFI boundary as JSON and half of what can go wrong is the crossing.
@MainActor
enum SchemaDiagramChecks {
    private static var failures = 0
    private static var scratch: [URL] = []
    private static var held: [Database] = []

    static func run() -> Bool {
        failures = 0
        checkOnlyTablesAKeyReachesGetABox()
        checkABoxListsTheColumnsItsKeysTouchAndNothingElse()
        checkAKeyOutOfTheSchemaIsCountedRatherThanDrawn()
        checkEveryKeyIsOneLineFromTheTableThatDeclaresIt()
        checkATableThatPointsAtItselfIsOneBoxAndOneLine()
        checkTheCapSaysWhatItLeftOut()
        checkJoinedTablesAreLaidOutTogetherAndBoxesDoNotOverlap()
        checkLinesTouchTheBoxesTheyJoin()
        checkAnEmptyAnswerSaysWhichEmptyItIs()
        checkOnlyRelationsThatCanCarryAKeyAreAsked()
        checkTheKeysCrossTheBoundaryAndBecomeAPicture()
        checkTheSheetOpensOnTheTreesSchemaAndLetsThePictureGo()
        for file in scratch { try? FileManager.default.removeItem(at: file) }
        scratch = []
        if failures == 0 {
            fputs("schema diagram: all checks passed\n", stderr)
        } else {
            fputs("schema diagram: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - What is drawn

    /// A table nothing points at and that points at nothing gets no box.
    ///
    /// This is the answer to "what happens on a schema with three hundred
    /// tables", and it is the whole of it: the boxes that make such a diagram
    /// unreadable are the ones carrying no line, and each of them says nothing
    /// the sidebar does not already say. The counts still name them, so an almost
    /// empty canvas cannot be mistaken for a read that failed.
    private static func checkOnlyTablesAKeyReachesGetABox() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("audit", []),
                keys("orders", [key("orders_customer_fk", ["customer_id"], "customer", ["id"])]),
                keys("customer", []),
                keys("import_log", []),
                keys("settings", [])
            ])

        expect(diagram.tables.map(\.name), ["orders", "customer"], "two boxes, not five")
        expect(diagram.edges.count, 1, "and the one key between them")
        expect(diagram.asked, 5, "the other three were read")
        expect(diagram.related, 2, "and are counted as unrelated rather than forgotten")
        expect(
            diagram.summary, "1 key · 2 related tables of 5",
            "the sentence names both numbers")
    }

    /// A box lists the columns its keys touch — its own and the ones pointed at —
    /// and no other column of the table.
    ///
    /// The claim under this one is a cost: no column list is ever fetched. A box
    /// that showed every column would need `db_columns_json` per table, which on
    /// a wide schema is a second round trip per table to draw names nobody is
    /// reading at this zoom.
    private static func checkABoxListsTheColumnsItsKeysTouchAndNothingElse() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys(
                    "shipment",
                    [
                        key(
                            "shipment_order_fk", ["order_id", "order_line"], "order_line",
                            ["order_id", "line"]),
                        // A second key into the same table, sharing a column with
                        // the first: the shared one is listed once.
                        key("shipment_line_fk", ["order_id"], "order_line", ["order_id"])
                    ]),
                keys("order_line", [])
            ])

        expect(
            diagram.table(named: "shipment")?.columns, ["order_id", "order_line"],
            "the referencing side lists its own key columns, deduplicated")
        expect(
            diagram.table(named: "order_line")?.columns, ["order_id", "line"],
            "and the referenced side lists what is pointed at, in key order")
        expect(diagram.table(named: "shipment")?.hiddenColumns, 0, "with nothing held back")

        // The lid on one box. A key over more columns than a box has room for is
        // rare, and a box that grew to forty rows would be a table of columns
        // pretending to be a diagram.
        let wide = (1...12).map { "c\($0)" }
        let big = SchemaDiagram.of(
            schema: "public",
            read: [keys("wide", [key("wide_fk", wide, "other", wide)]), keys("other", [])])
        expect(
            big.table(named: "wide")?.columns.count, SchemaDiagram.columnCap,
            "a box lists at most the cap")
        expect(
            big.table(named: "wide")?.hiddenColumns, 12 - SchemaDiagram.columnCap,
            "and says how many it did not")
    }

    /// A key whose other side is not in this schema is counted and not drawn.
    ///
    /// Drawing it would need a box for a table from somewhere else, and a diagram
    /// headed `public` with a table from `archive` in it is a diagram that is not
    /// of one schema. Counted rather than dropped in silence, because "this
    /// schema is joined to another one" is a fact about the schema.
    private static func checkAKeyOutOfTheSchemaIsCountedRatherThanDrawn() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys(
                    "orders",
                    [
                        key("orders_customer_fk", ["customer_id"], "customer", ["id"]),
                        key("orders_region_fk", ["region_id"], "region", ["id"], schema: "archive"),
                        // A name this schema did not list. Nothing should invent
                        // a box for a table that was never read.
                        key("orders_ghost_fk", ["ghost_id"], "ghost", ["id"])
                    ]),
                keys("customer", [])
            ])

        expect(diagram.tables.map(\.name), ["orders", "customer"], "only what is here gets a box")
        expect(diagram.edges.count, 1, "and only the key that stays inside gets a line")
        expect(diagram.outside, 2, "the other two are counted")
        expect(
            diagram.summary.contains("2 keys point outside public"), true,
            "and said out loud: \(diagram.summary)")
    }

    /// The line goes from the table that declares the key to the table it names.
    ///
    /// Direction is the one thing about a relationship this side actually knows —
    /// it is read from the referencing end — and it is the difference between
    /// "orders belong to a customer" and the opposite. It also explains why
    /// `referenced_by` is never asked: the same constraint answers from both
    /// ends, and asking both would draw every line twice at twice the cost.
    private static func checkEveryKeyIsOneLineFromTheTableThatDeclaresIt() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("customer", []),
                keys("orders", [key("orders_customer_fk", ["customer_id"], "customer", ["id"])])
            ])

        expect(diagram.edges.count, 1, "one key, one line")
        guard let edge = diagram.edges.first else { return }
        expect(edge.from, "orders", "from the table that declares it")
        expect(edge.to, "customer", "to the one it names")
        expect(edge.fromColumns, ["customer_id"], "carrying the referencing columns")
        expect(edge.toColumns, ["id"], "and the referenced ones")
        expect(edge.label, "customer_id → customer(id)", "which is how the line reads")
        expect(edge.isSelfReference, false, "and it is not a loop")

        // Read aloud, because the lines are on a canvas and a canvas says
        // nothing to a screen reader. Both directions, from either box.
        guard let orders = diagram.table(named: "orders"),
            let customer = diagram.table(named: "customer")
        else { return }
        expect(
            diagram.spoken(for: orders), "orders, customer_id points at customer",
            "the referencing box says what it points at")
        expect(
            diagram.spoken(for: customer), "customer, referenced by orders",
            "and the referenced box says what points at it")
    }

    /// A key a table declares against itself is one box with a line on it.
    ///
    /// The case that breaks a diagram drawn centre to centre: both ends are the
    /// same point, so a straight line between them is nothing at all. The model
    /// says which edges are these; the sheet draws them as a loop.
    private static func checkATableThatPointsAtItselfIsOneBoxAndOneLine() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("employee", [key("employee_manager_fk", ["manager_id"], "employee", ["id"])])
            ])

        expect(diagram.tables.count, 1, "one table, one box")
        expect(diagram.edges.count, 1, "and the key it declares against itself")
        expect(diagram.edges.first?.isSelfReference, true, "marked as the loop it is")
        expect(
            diagram.table(named: "employee")?.columns, ["manager_id", "id"],
            "both ends of it listed on the one box")
    }

    /// Past the cap, the diagram stops at a group boundary and says how much of
    /// the schema is missing from it.
    ///
    /// Both numbers, because either alone is misleading: "3 tables" hides that
    /// there are seven, and a table count that did not mention the keys dropped
    /// with the boxes would leave somebody reading a picture with lines silently
    /// missing from it. Stopping at a boundary rather than at the cap itself is
    /// what keeps the drawn half of a group from being a picture whose other
    /// half is the answer.
    private static func checkTheCapSaysWhatItLeftOut() {
        // Three groups: a triangle and two pairs. A cap of four has room for the
        // triangle and not for a pair after it.
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("a1", [key("a1_fk", ["b"], "a2", ["id"])]),
                keys("a2", []),
                keys("b1", [key("b1_fk", ["c"], "b2", ["id"])]),
                keys("b2", []),
                keys("t1", [key("t1_fk", ["x"], "t2", ["id"])]),
                keys("t2", [key("t2_fk", ["y"], "t3", ["id"])]),
                keys("t3", [key("t3_fk", ["z"], "t1", ["id"])])
            ],
            cap: 4)

        expect(
            diagram.tables.map(\.name), ["t1", "t2", "t3"],
            "the biggest group, whole, and nothing of the pair that would not fit")
        expect(diagram.related, 7, "out of seven tables that take part in a key")
        expect(diagram.edges.count, 3, "the keys inside what is drawn")
        expect(diagram.undrawn, 2, "and the two whose tables were cut are counted")
        expect(
            diagram.summary, "3 keys · 3 of 7 related tables · 2 not drawn",
            "with the sentence saying so")
        expect(everyBoxHasALine(diagram), true, "and no box is left with nothing attached")

        // A group bigger than the whole cap is the one case that is cut inside a
        // group: an empty canvas would be a worse answer for a schema that is one
        // big web. The chain is deliberately not in the schema's own order —
        // n1 joins to n5, which joins to n4 — so that cutting it by that order
        // would leave n1 on the canvas with nothing attached to it. Breadth-first
        // is what makes the drawn part a connected picture instead.
        let chain = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("n1", [key("n1_fk", ["next"], "n5", ["id"])]),
                keys("n2", []),
                keys("n3", [key("n3_fk", ["next"], "n2", ["id"])]),
                keys("n4", [key("n4_fk", ["next"], "n3", ["id"])]),
                keys("n5", [key("n5_fk", ["next"], "n4", ["id"])])
            ],
            cap: 3)
        expect(chain.tables.map(\.name), ["n1", "n5", "n4"], "the cap cuts along the chain")
        expect(chain.edges.count, 2, "with the keys between what is drawn")
        expect(chain.undrawn, 2, "and the rest counted")
        expect(everyBoxHasALine(chain), true, "and still no box on its own")

        // The cap that is actually shipped, applied when nobody names one. A
        // build whose default was small would draw a fraction of a schema and
        // pass every case above, all of which name their own.
        let pairs = (1...70).flatMap { position in
            [
                keys("p\(position)", [key("p\(position)_fk", ["id"], "q\(position)", ["id"])]),
                keys("q\(position)", [])
            ]
        }
        let capped = SchemaDiagram.of(schema: "public", read: pairs)
        expect(capped.tables.count, SchemaDiagram.tableCap, "the default cap is what is drawn to")
        expect(capped.related, 140, "out of every table that takes part in a key")
        expect(capped.undrawn, 40, "and the keys of the groups it did not reach are counted")
    }

    /// Every line on the diagram has both of its boxes, and every box at least
    /// one line. A line to a table that was cut would go nowhere, and a box with
    /// nothing attached is the thing the diagram exists not to draw.
    private static func everyBoxHasALine(_ diagram: SchemaDiagram) -> Bool {
        let drawn = Set(diagram.tables.map(\.name))
        let joined = Set(diagram.edges.flatMap { [$0.from, $0.to] })
        return drawn == joined
    }

    /// Tables joined to each other are laid out next to each other, and no two
    /// boxes overlap.
    ///
    /// The grouping is the only layout intelligence there is, and it is what an
    /// alphabetical grid gets worst: `orders` in one corner and `order_line`
    /// eleven boxes away, with the line between them crossing everything. The
    /// non-overlap claim is separate and about the rows: boxes differ in height
    /// because they differ in how many key columns they carry, so a grid on a
    /// fixed pitch would draw a tall box through the one under it.
    private static func checkJoinedTablesAreLaidOutTogetherAndBoxesDoNotOverlap() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                // Interleaved on purpose: the groups are not adjacent in the
                // schema's own order, so an untouched order would separate them.
                keys("alpha", [key("alpha_fk", ["z"], "zulu", ["id"])]),
                keys("bravo", [key("bravo_fk", ["y"], "yankee", ["id"])]),
                keys(
                    "charlie",
                    [
                        key("charlie_one", ["a", "b", "c", "d"], "xray", ["id", "b", "c", "d"]),
                        key("charlie_two", ["e"], "xray", ["id"])
                    ]),
                keys("delta", [key("delta_fk", ["c"], "charlie", ["a"])]),
                keys("xray", []),
                keys("yankee", []),
                keys("zulu", [])
            ])

        expect(
            diagram.tables.map(\.name),
            ["charlie", "delta", "xray", "alpha", "zulu", "bravo", "yankee"],
            "each group together, the biggest first")

        // Square-ish rather than one long row: a diagram read by scrolling right
        // past every line is not read.
        let columns = Set(diagram.tables.map(\.x)).count
        expect(columns, 3, "seven boxes go three across")

        for (index, table) in diagram.tables.enumerated() {
            for other in diagram.tables[(index + 1)...] where table.frame.intersects(other.frame) {
                failures += 1
                fputs(
                    "schema diagram FAIL: \(table.name) and \(other.name) overlap\n"
                        + "  \(table.frame) and \(other.frame)\n", stderr)
            }
        }
        // The tall box is the one with four key columns, and the row under it has
        // to clear it rather than the shortest in its row.
        guard let charlie = diagram.table(named: "charlie"),
            let second = diagram.tables.first(where: { $0.y > charlie.y })
        else {
            failures += 1
            fputs("schema diagram FAIL: there is no second row\n", stderr)
            return
        }
        expect(second.y > charlie.frame.maxY, true, "the next row clears the tallest box above it")
    }

    /// A line starts and ends on the border of the boxes it joins.
    ///
    /// Centre to centre would put both ends under the boxes, where the only part
    /// of the line anybody sees is the middle — which is the same picture for
    /// "these two are joined" as for "these two are joined to something else
    /// behind them".
    private static func checkLinesTouchTheBoxesTheyJoin() {
        let diagram = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("orders", [key("orders_customer_fk", ["customer_id"], "customer", ["id"])]),
                keys("customer", [])
            ])
        guard let orders = diagram.table(named: "orders"),
            let customer = diagram.table(named: "customer")
        else { return }

        let (start, end) = SchemaDiagram.link(from: orders, to: customer)
        expect(onBorder(start, of: orders), true, "the line leaves the first box's edge: \(start)")
        expect(onBorder(end, of: customer), true, "and lands on the second's: \(end)")
        // Side by side in one row, so the line runs from one box's right edge to
        // the other's left and stays level.
        expect(start.x, orders.frame.maxX, "out of the right-hand side")
        expect(end.x, customer.frame.minX, "into the left-hand side")
    }

    /// "Nothing to read" and "nothing declares a key" are two different answers
    /// and get two different sentences.
    ///
    /// A schema of two hundred tables and no foreign key at all is a real and
    /// common shape — every warehouse is one — and telling somebody that as
    /// "nothing here" would read as a failed read.
    private static func checkAnEmptyAnswerSaysWhichEmptyItIs() {
        let nothing = SchemaDiagram.of(schema: "public", read: [])
        expect(nothing.isEmpty, true, "an empty schema draws nothing")
        expect(nothing.summary, "Nothing in public to read.", "and says the schema is empty")

        let unrelated = SchemaDiagram.of(
            schema: "public", read: [keys("events", []), keys("metrics", [])])
        expect(unrelated.isEmpty, true, "tables with no keys draw nothing either")
        expect(
            unrelated.summary, "No foreign keys · 2 tables read in public",
            "but the sentence is the other one")
    }

    /// Only the relations that can be on either end of a key are asked.
    ///
    /// This is where the cost of a wide schema is decided: a view declares no key
    /// and nothing can point at one — the referenced side of a key must carry a
    /// unique constraint — so asking them is a round trip per view to be told
    /// "none". A warehouse schema is mostly views.
    private static func checkOnlyRelationsThatCanCarryAKeyAreAsked() {
        let listed = [
            relation("orders", .table),
            relation("orders_v", .view),
            relation("orders_daily", .materializedView),
            relation("orders_2024", .partitionedTable),
            relation("remote_orders", .foreignTable),
            relation("mystery", .unknown)
        ]
        expect(
            SchemaDiagram.asks(listed).map(\.name),
            ["orders", "orders_2024", "remote_orders", "mystery"],
            "views are skipped and everything that might hold a key is asked")
    }

    // MARK: - Over the boundary

    /// The whole read path on a real database: relations, keys, and a picture.
    ///
    /// A file rather than a fixture because the keys are composed by the core and
    /// arrive as JSON — a field renamed on the far side decodes as an empty
    /// diagram rather than failing — and because SQLite is the engine that
    /// answers `other_schema` with the schema it was asked about, which is the
    /// half of the "is this key inside" rule a fixture cannot exercise.
    private static func checkTheKeysCrossTheBoundaryAndBecomeAPicture() {
        guard
            let db = opened([
                "CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT)",
                "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER "
                    + "REFERENCES customer (id))",
                "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)",
                "CREATE VIEW recent AS SELECT * FROM orders"
            ])
        else { return }

        guard let listed = try? db.relations(schema: "main") else {
            failures += 1
            fputs("schema diagram FAIL: the schema would not list\n", stderr)
            return
        }
        let asked = SchemaDiagram.asks(listed)
        expect(asked.count, 3, "the view is not asked: \(asked.map(\.name))")

        var read: [SchemaDiagram.TableKeys] = []
        for relation in asked {
            guard let keys = try? db.foreignKeys(schema: "main", relation: relation.name) else {
                failures += 1
                fputs("schema diagram FAIL: \(relation.name) would not answer\n", stderr)
                return
            }
            read.append(SchemaDiagram.TableKeys(table: relation.name, keys: keys))
        }
        let diagram = SchemaDiagram.of(schema: "main", read: read)

        expect(diagram.asked, 3, "three tables read")
        expect(Set(diagram.tables.map(\.name)), ["orders", "customer"], "two of them related")
        expect(diagram.outside, 0, "and the key is inside this schema, not outside it")
        expect(diagram.edges.count, 1, "one key drawn")
        expect(diagram.edges.first?.from, "orders", "declared by orders")
        expect(diagram.edges.first?.fromColumns, ["customer_id"], "on the column that holds it")
        expect(diagram.edges.first?.toColumns, ["id"], "pointing at the primary key")
    }

    /// The sheet opens on the schema the tree opens on, and lets the picture go
    /// when it closes.
    ///
    /// The second half is the memory rule, and it is not free: a diagram is a
    /// snapshot of metadata, and one kept per tab for the life of a window would
    /// be a stale picture nobody asked to see again.
    private static func checkTheSheetOpensOnTheTreesSchemaAndLetsThePictureGo() {
        let model = makeModel()
        expect(model.canDrawSchemaDiagram, false, "a tab with nothing open draws nothing")

        guard let db = opened([]) else { return }
        model.sessions[0].db = db
        // `public` after a system schema and another user schema: the navigator's
        // own rule is what picks, so that the window cannot disagree with itself
        // about which schema it means.
        model.sessions[0].schemas = [
            SchemaInfo(name: "pg_catalog", isSystem: true),
            SchemaInfo(name: "archive", isSystem: false),
            SchemaInfo(name: "public", isSystem: false)
        ]
        model.schemaDiagramSchema = "somewhere_else"

        model.presentSchemaDiagram()
        expect(model.isSchemaDiagramOpen, true, "the sheet opens")
        expect(model.schemaDiagramSchema, "public", "on the schema the tree opens on")
        expect(model.isDrawingSchemaDiagram, true, "and starts reading without being told twice")

        let drawn = SchemaDiagram.of(
            schema: "public",
            read: [
                keys("orders", [key("orders_customer_fk", ["customer_id"], "customer", ["id"])]),
                keys("customer", [])
            ])
        model.landSchemaDiagram(drawn)
        expect(model.schemaDiagram?.tables.count, 2, "the picture lands")
        expect(model.status, drawn.summary, "and the sentence goes to the status bar")

        // Closing lets it go. Anything else is a snapshot outliving the sheet.
        model.closeSchemaDiagram()
        expect(model.isSchemaDiagramOpen, false, "the sheet closes")
        expect(model.schemaDiagram == nil, true, "and the picture is dropped")

        // A read that lands after the sheet was closed is not put back on screen.
        model.landSchemaDiagram(drawn)
        expect(model.schemaDiagram == nil, true, "a late answer does not reopen it")

        // Nor is one of a schema the picker has moved off, which would be a
        // canvas of one schema under a heading naming another. The flag is put
        // back by hand: the read started above is on the session's queue and a
        // check that never returns to the run loop is a check it cannot land in.
        model.isDrawingSchemaDiagram = false
        model.presentSchemaDiagram()
        model.schemaDiagramSchema = "archive"
        model.landSchemaDiagram(drawn)
        expect(model.schemaDiagram == nil, true, "and neither is one of the wrong schema")
        model.schemaDiagramSchema = "public"
        model.landSchemaDiagram(drawn)
        expect(model.schemaDiagram?.schema, "public", "while the right one still lands")
    }

    // MARK: - Fixtures

    private static func keys(_ table: String, _ keys: [RelationshipInfo]) -> SchemaDiagram.TableKeys
    {
        SchemaDiagram.TableKeys(table: table, keys: keys)
    }

    private static func key(
        _ name: String, _ local: [String], _ table: String, _ other: [String],
        schema: String = "public"
    ) -> RelationshipInfo {
        RelationshipInfo(
            name: name, localColumns: local, otherSchema: schema, otherTable: table,
            otherColumns: other, onUpdate: "NO ACTION", onDelete: "NO ACTION")
    }

    private static func relation(_ name: String, _ kind: RelationKind) -> RelationInfo {
        RelationInfo(schema: "public", name: name, kind: kind, estimatedRows: nil)
    }

    /// Whether a point is on the rectangle's edge rather than inside it.
    private static func onBorder(_ point: CGPoint, of table: SchemaDiagram.Table) -> Bool {
        let frame = table.frame
        let touchesSide = abs(point.x - frame.minX) < 0.001 || abs(point.x - frame.maxX) < 0.001
        let touchesEnd = abs(point.y - frame.minY) < 0.001 || abs(point.y - frame.maxY) < 0.001
        return (touchesSide || touchesEnd) && frame.insetBy(dx: -0.001, dy: -0.001).contains(point)
    }

    /// A scratch SQLite file with `statements` already run into it. Held for the
    /// life of the process, for the reason `SchemaDiffChecks` holds its own.
    private static func opened(_ statements: [String]) -> Database? {
        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-diagram-\(UUID().uuidString).db")
        scratch.append(file)
        FileManager.default.createFile(atPath: file.path, contents: nil)
        guard let db = try? Database(connString: "sqlite://\(file.path)") else {
            failures += 1
            fputs("schema diagram FAIL: a SQLite file would not open\n", stderr)
            return nil
        }
        held.append(db)
        for sql in statements {
            guard let query = try? db.query(sql, batchRows: 1) else {
                failures += 1
                fputs("schema diagram FAIL: \(sql) was refused\n", stderr)
                return nil
            }
            while let batch = (try? query.nextBatch()) ?? nil { _ = batch }
        }
        return db
    }

    private static func makeModel() -> AppModel {
        let store = ScratchDefaults.store("verify-schema-diagram")
        return AppModel(
            history: QueryHistory(defaults: store), favorites: QueryFavorites(defaults: store),
            preferences: Preferences(store: store))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("schema diagram FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
