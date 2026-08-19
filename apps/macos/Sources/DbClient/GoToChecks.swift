import Foundation

/// Executable checks for the go-to palette's matching, run by `--verify-goto`.
///
/// The rule is `catalog::rank`'s and lives in two places, which is the whole
/// reason these exist: the Rust side is tested where it is written, and this
/// side has to be pinned by behaviour so that the two drifting apart shows up
/// here rather than as a palette that ranks a name differently from the
/// completion offering the same name.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum GoToChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAnEmptyNeedleListsEverythingInOrder()
        checkNamesThatBeginWithItComeFirst()
        checkADotSeparatesSchemaFromName()
        checkMatchingIgnoresCase()
        if failures == 0 {
            fputs("goto: all checks passed\n", stderr)
        } else {
            fputs("goto: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The palette opens with everything in it, in one predictable order.
    ///
    /// Ordered by the qualified name and not by the bare one: two schemas can
    /// hold a table of the same name, and a list whose order depends on which
    /// arrived first would put them in a different place each session.
    private static func checkAnEmptyNeedleListsEverythingInOrder() {
        expect(
            found(""),
            [
                "public.Customers", "public.customer_orders", "public.orders", "sales.orders",
                "sales.regions"
            ],
            "an empty needle is every table, by qualified name")
    }

    /// A name that begins with the text is offered before one that merely holds
    /// it, which is the half of the rule that makes typing three letters useful.
    private static func checkNamesThatBeginWithItComeFirst() {
        expect(
            found("orders"), ["public.orders", "sales.orders", "public.customer_orders"],
            "the two called orders come before the one containing it")
    }

    /// A dot is read as schema-then-name, the way the SQL completion reads it.
    ///
    /// The trailing-dot case is the one somebody is actually in while typing:
    /// `sales.` has named a schema and nothing in it, and listing that schema is
    /// the answer — an empty list would read as "this schema is empty".
    private static func checkADotSeparatesSchemaFromName() {
        expect(found("sales.ord"), ["sales.orders"], "both halves narrow the list")
        expect(
            found("sales."), ["sales.orders", "sales.regions"],
            "a trailing dot lists the schema")
    }

    /// Case is ignored on both sides. A database whose tables are named in mixed
    /// case is not a database somebody wants to hold Shift for.
    private static func checkMatchingIgnoresCase() {
        expect(
            found("CUST"), ["public.Customers", "public.customer_orders"],
            "an upper-case needle finds lower-case names and the other way round")
    }

    // MARK: - Fixture

    /// Two schemas, a name held by both, a name containing another, and one
    /// name that is not lower case — the four shapes the ordering has to
    /// separate.
    private static let targets = [
        GoToTarget(schema: "public", name: "orders"),
        GoToTarget(schema: "public", name: "customer_orders"),
        GoToTarget(schema: "public", name: "Customers"),
        GoToTarget(schema: "sales", name: "orders"),
        GoToTarget(schema: "sales", name: "regions")
    ]

    private static func found(_ needle: String) -> [String] {
        GoTo.ranked(targets, matching: needle).map(\.qualified)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("goto FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
