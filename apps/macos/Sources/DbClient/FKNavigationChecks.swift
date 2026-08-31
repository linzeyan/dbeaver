import Foundation

/// Executable checks for what a cell leads to, run by `--verify-fk-nav`.
///
/// Two questions, and they are the two the menu asks: which cells offer a jump,
/// and what the target is filtered by when one is taken. Neither needs a
/// database — `AppModel.jumps` is a pure function of two key lists and a row —
/// and the key shapes worth checking are ones no fixture database here has: a
/// composite key, two keys into one table, and a key pointing at its own table.
///
/// What is *not* here is the browse that follows. Compiling a filter row into a
/// WHERE clause is `dbedit`'s, checked there against a real dialect, and
/// restating it in Swift would be a second answer to the same question.
enum FKNavigationChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAColumnWithNoKeyOnItLeadsNowhere()
        checkAForeignKeyColumnLeadsToTheRowItNames()
        checkANullForeignKeyLeadsNowhere()
        checkACompositeKeyCarriesEveryColumnOfIt()
        checkACompositeKeyMissingHalfARowIsRefused()
        checkTheReferencedSideIsOfferedFromTheParent()
        checkTwoKeysIntoOneTableAreTwoJumps()
        checkTwoKeysBackFromOneTableAreToldApartByTheirOwnColumns()
        checkAKeyIntoItsOwnTableIsOfferedLikeAnyOther()
        checkASchemaOfItsOwnIsNamedInTheLabel()
        if failures == 0 {
            fputs("fk-nav: all checks passed\n", stderr)
        } else {
            fputs("fk-nav: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The common cell. Most columns are not part of any key, and a menu that
    /// offered a jump from every cell would be a menu nobody reads.
    private static func checkAColumnWithNoKeyOnItLeadsNowhere() {
        let jumps = AppModel.jumps(
            atColumn: "sku", in: ["sku": "AB-1", "parent_id": "7"],
            through: [parentKey], and: [], reading: "public")
        expect(jumps.isEmpty, true, "a column no key names offers nothing")
    }

    /// The whole point: the cell holds the other table's key, so the other table
    /// is opened at the row that key names.
    private static func checkAForeignKeyColumnLeadsToTheRowItNames() {
        let jumps = AppModel.jumps(
            atColumn: "parent_id", in: ["sku": "AB-1", "parent_id": "7"],
            through: [parentKey], and: [], reading: "public")
        expect(jumps.referenced.count, 1, "the key's own column offers the jump")
        expect(jumps.referencing.count, 0, "and offers nothing in the other direction")
        expect(jumps.referenced.first?.label, "bench_wide", "named for where it goes")
        expect(jumps.referenced.first?.name, "bench_wide", "and pointed there")
        expect(
            jumps.referenced.first?.match,
            [FilterRule(column: "id", op: .equals, value: "7")],
            "filtered by the column it references, not by the column that was clicked")
    }

    /// A NULL foreign key references nothing, so there is nothing to go to. The
    /// row is passed in without its NULL columns, so this is the same case as a
    /// column the result does not carry — which is the answer wanted for both.
    private static func checkANullForeignKeyLeadsNowhere() {
        let jumps = AppModel.jumps(
            atColumn: "parent_id", in: ["sku": "AB-1"],
            through: [parentKey], and: [], reading: "public")
        expect(jumps.isEmpty, true, "a key with nothing in it leads nowhere")
    }

    /// The reason a jump is given the whole row rather than the clicked cell.
    /// Half a composite key matches every row that shares that half, which is
    /// not the row anybody clicked towards.
    private static func checkACompositeKeyCarriesEveryColumnOfIt() {
        let jumps = AppModel.jumps(
            atColumn: "order_id", in: ["order_id": "10", "line_no": "2", "sku": "AB-1"],
            through: [lineKey], and: [], reading: "public")
        expect(
            jumps.referenced.first?.match,
            [
                FilterRule(column: "ord", op: .equals, value: "10"),
                FilterRule(column: "line", op: .equals, value: "2")
            ],
            "both halves of the key go into the filter, in the key's own order")
        // And from the other half of the key, which is the same jump: a
        // composite key is one relationship, whichever of its columns is under
        // the pointer.
        let fromSecond = AppModel.jumps(
            atColumn: "line_no", in: ["order_id": "10", "line_no": "2"],
            through: [lineKey], and: [], reading: "public")
        expect(fromSecond.referenced.count, 1, "either column of the key offers it")
    }

    /// A row missing one column of a composite key offers nothing rather than a
    /// filter with a hole in it. A filter naming one column of two would land on
    /// a page of rows and call it a row.
    private static func checkACompositeKeyMissingHalfARowIsRefused() {
        let jumps = AppModel.jumps(
            atColumn: "order_id", in: ["order_id": "10"],
            through: [lineKey], and: [], reading: "public")
        expect(jumps.isEmpty, true, "half a key is not a jump")
    }

    /// The inbound direction, which the driver reports with the sides swapped:
    /// `localColumns` is this relation's whichever way the key points, so the
    /// same read answers both.
    private static func checkTheReferencedSideIsOfferedFromTheParent() {
        let jumps = AppModel.jumps(
            atColumn: "id", in: ["id": "7"], through: [], and: [childKey], reading: "public")
        expect(jumps.referenced.count, 0, "a parent's key column points at nothing")
        expect(jumps.referencing.count, 1, "but the rows pointing at it are offered")
        expect(
            jumps.referencing.first?.match,
            [FilterRule(column: "parent_id", op: .equals, value: "7")],
            "filtered by the child's column, holding this row's value")
    }

    /// Two keys into one table is an ordinary shape — an order with a billing
    /// address and a shipping one — and each is its own jump. The menu tells
    /// them apart by the columns they leave from, which is why a jump carries
    /// them.
    private static func checkTwoKeysIntoOneTableAreTwoJumps() {
        let jumps = AppModel.jumps(
            atColumn: "billing_id", in: ["billing_id": "3", "shipping_id": "4"],
            through: [addressKey(named: "billing_id"), addressKey(named: "shipping_id")],
            and: [], reading: "public")
        expect(jumps.referenced.count, 1, "only the key the clicked column belongs to")
        expect(jumps.referenced.first?.via, "billing_id", "and it says which one it is")
    }

    /// The same shape read from the parent, which is where naming the jump
    /// after this relation's columns would have failed: both keys arrive at
    /// `addresses.id`, so `id` is what they have in common and the child's
    /// column is the only thing that tells them apart.
    private static func checkTwoKeysBackFromOneTableAreToldApartByTheirOwnColumns() {
        let jumps = AppModel.jumps(
            atColumn: "id", in: ["id": "3"], through: [],
            and: [
                RelationshipInfo(
                    name: "fk_billing_id", localColumns: ["id"], otherSchema: "public",
                    otherTable: "orders", otherColumns: ["billing_id"], onUpdate: "", onDelete: ""),
                RelationshipInfo(
                    name: "fk_shipping_id", localColumns: ["id"], otherSchema: "public",
                    otherTable: "orders", otherColumns: ["shipping_id"], onUpdate: "", onDelete: "")
            ], reading: "public")
        expect(jumps.referencing.count, 2, "both ways in are offered")
        expect(
            jumps.referencing.map(\.via), ["billing_id", "shipping_id"],
            "and each says which of the other table's columns points here")
        expect(
            jumps.referencing.map(\.match),
            [
                [FilterRule(column: "billing_id", op: .equals, value: "3")],
                [FilterRule(column: "shipping_id", op: .equals, value: "3")]
            ],
            "each filtering the column it is named for")
    }

    /// A tree — a category with a parent category — is a key whose two ends are
    /// one table, and it is offered in both directions like any other. Where it
    /// is not like any other is what happens next, and that is in `jump`: the
    /// selection does not change, so the browse has to be re-run rather than
    /// waited for. That half needs a connection and is checked by `--fk-jump`.
    private static func checkAKeyIntoItsOwnTableIsOfferedLikeAnyOther() {
        let outbound = RelationshipInfo(
            name: "fk_parent", localColumns: ["parent_id"], otherSchema: "public",
            otherTable: "categories", otherColumns: ["id"], onUpdate: "", onDelete: "")
        let inbound = RelationshipInfo(
            name: "fk_parent", localColumns: ["id"], otherSchema: "public",
            otherTable: "categories", otherColumns: ["parent_id"], onUpdate: "", onDelete: "")
        let jumps = AppModel.jumps(
            atColumn: "parent_id", in: ["id": "4", "parent_id": "2"],
            through: [outbound], and: [inbound], reading: "public")
        expect(jumps.referenced.count, 1, "the parent row is offered")
        expect(
            jumps.referenced.first?.match, [FilterRule(column: "id", op: .equals, value: "2")],
            "at the id this row's parent_id holds")
        expect(
            jumps.referencing.count, 0,
            "and the children of this row are not, from a column that is not the key they point at")
    }

    /// A key into another schema is named in full. Dropping the schema is right
    /// for the ordinary case and wrong here: two tables called `orders` in one
    /// database is what schemas are for.
    private static func checkASchemaOfItsOwnIsNamedInTheLabel() {
        let jumps = AppModel.jumps(
            atColumn: "region_id", in: ["region_id": "5"],
            through: [
                RelationshipInfo(
                    name: "fk_region", localColumns: ["region_id"], otherSchema: "sales",
                    otherTable: "regions", otherColumns: ["id"], onUpdate: "", onDelete: "")
            ], and: [], reading: "public")
        expect(jumps.referenced.first?.label, "sales.regions", "another schema is spelled out")
    }

    // MARK: - Fixtures

    /// `bench_child.parent_id → bench_wide.id`, as the benchmark database has it.
    private static let parentKey = RelationshipInfo(
        name: "bench_child_parent_id_fkey", localColumns: ["parent_id"], otherSchema: "public",
        otherTable: "bench_wide", otherColumns: ["id"], onUpdate: "", onDelete: "CASCADE")

    /// The same key read from the other end, which is what `referencedBy`
    /// answers for `bench_wide`.
    private static let childKey = RelationshipInfo(
        name: "bench_child_parent_id_fkey", localColumns: ["id"], otherSchema: "public",
        otherTable: "bench_child", otherColumns: ["parent_id"], onUpdate: "", onDelete: "CASCADE")

    /// A two-column key whose columns are named differently at each end, so a
    /// filter built from the wrong side's names is visible.
    private static let lineKey = RelationshipInfo(
        name: "fk_line", localColumns: ["order_id", "line_no"], otherSchema: "public",
        otherTable: "lines", otherColumns: ["ord", "line"], onUpdate: "", onDelete: "")

    private static func addressKey(named column: String) -> RelationshipInfo {
        RelationshipInfo(
            name: "fk_\(column)", localColumns: [column], otherSchema: "public",
            otherTable: "addresses", otherColumns: ["id"], onUpdate: "", onDelete: "")
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("fk-nav FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
