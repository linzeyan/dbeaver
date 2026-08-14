import Foundation

/// Executable checks for the transaction seam, run by `--verify-transaction`.
///
/// What a transaction does is the core's business and is checked against real
/// servers in `crates/conn/tests/contract.rs` and `crates/ffi/tests/conformance.rs`.
/// Restating any of it here would be a second copy of a rule, which is a rule
/// that will disagree with the first one the day either is corrected.
///
/// What is checked here is this side's own: that `db_tx_state_json`'s payload
/// decodes into the fields the toolbar draws from, and that a field renamed on
/// the other side of the boundary fails loudly rather than arriving as a
/// plausible default. That second one is not hypothetical — a
/// `RelationInfo.estimated_rows` that became optional in the core and stayed
/// non-optional here stopped the window connecting at all, and the only reason
/// it was easy to find is that `JSONDecoder` refused rather than guessing.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum TransactionChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkTheStateDecodesIntoWhatTheToolbarDraws()
        checkARenamedFieldIsRefusedRatherThanGuessed()
        checkNothingConnectedOffersNoControl()
        if failures == 0 {
            fputs("transaction: all checks passed\n", stderr)
        } else {
            fputs("transaction: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The wire shape `db_tx_state_json` documents arrives as the four facts the
    /// toolbar and the menu are written against.
    private static func checkTheStateDecodesIntoWhatTheToolbarDraws() {
        let open = decoded(
            """
            {"transactional":true,"autocommit":false,"open":true,
             "savepoints":["before_edit","halfway"]}
            """)
        expect(open?.transactional, true, "the connection can hold a transaction")
        expect(open?.autocommit, false, "and is being asked to")
        expect(open?.open, true, "with work in it")
        expect(open?.savepoints, ["before_edit", "halfway"], "innermost last")

        let idle = decoded(
            #"{"transactional":false,"autocommit":true,"open":false,"savepoints":[]}"#)
        expect(idle?.transactional, false, "a database with no transaction to control")
        expect(idle?.savepoints, [], "and nothing marked in one")
    }

    /// A field the core renames stops the decode instead of arriving as false.
    ///
    /// Which is the behaviour worth having: `open` silently defaulting to false
    /// is a window that shows a clean connection over uncommitted work, and the
    /// person who then quits loses it without ever being asked.
    private static func checkARenamedFieldIsRefusedRatherThanGuessed() {
        expect(
            decoded(#"{"transactional":true,"autocommit":false,"savepoints":[]}"#) == nil, true,
            "a missing `open` is refused")
        expect(
            decoded(#"{"transactional":true,"autocommit":false,"open":true}"#) == nil, true,
            "and so is a missing `savepoints`")
    }

    /// Before anything is connected there is no mode, and no control for one.
    private static func checkNothingConnectedOffersNoControl() {
        expect(TransactionState.none.transactional, false, "nothing to control")
        expect(TransactionState.none.open, false, "and nothing open to lose")
    }

    // MARK: - Harness

    private static func decoded(_ json: String) -> TransactionState? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(TransactionState.self, from: data)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("transaction FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
