import Foundation

/// Executable checks for the back/forward history, run by `--verify-history`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum BrowseHistoryChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAFreshHistoryGoesNowhere()
        checkBackAndForwardWalkThePath()
        checkArrivingAfterBackDropsWhatWasAhead()
        checkArrivingWhereWeAreRecordsNothing()
        checkTwoTabsOfOneTableAreTwoPlaces()
        checkConnectingElsewhereForgetsThePath()
        if failures == 0 {
            fputs("history: all checks passed\n", stderr)
        } else {
            fputs("history: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// A window that has opened nothing offers neither direction, and asking
    /// anyway does not move it.
    private static func checkAFreshHistoryGoesNowhere() {
        var history = BrowseHistory()
        expect(history.canGoBack, false, "a fresh history cannot go back")
        expect(history.canGoForward, false, "nor forward")
        expect(history.goBack(), nil, "and asking answers nil")
        expect(history.current, nil, "with nothing current")
    }

    /// The ordinary case, and the one the item exists for: three tables, then
    /// back to the first and forward again.
    private static func checkBackAndForwardWalkThePath() {
        var history = BrowseHistory()
        history.visit(orders)
        history.visit(regions)
        history.visit(customers)
        expect(history.current, customers, "we are where we last went")
        expect(history.canGoForward, false, "with nothing ahead")

        expect(history.goBack(), regions, "back lands on the second")
        expect(history.goBack(), orders, "and then the first")
        expect(history.canGoBack, false, "which is the start of the path")
        expect(history.goForward(), regions, "forward retraces it")
        expect(history.goForward(), customers, "all the way to the end")
        expect(history.goForward(), nil, "and stops there")
    }

    /// Going somewhere new after Back abandons the route you had left. This is
    /// what every browser does, and the alternative — keeping it — would offer a
    /// Forward that jumps to a table the user never chose from here.
    private static func checkArrivingAfterBackDropsWhatWasAhead() {
        var history = BrowseHistory()
        history.visit(orders)
        history.visit(regions)
        history.visit(customers)
        _ = history.goBack()
        _ = history.goBack()
        history.visit(invoices)
        expect(history.current, invoices, "we are at the new place")
        expect(history.canGoForward, false, "and the old forward path is gone")
        expect(history.visits, [orders, invoices], "the path is where we went, then here")
        expect(history.goBack(), orders, "back still reaches what came before")
    }

    /// A refresh re-selects the same relation and a tab click may land where we
    /// already are. Recording those would fill the path with steps that look, to
    /// somebody pressing Back, like nothing happening.
    private static func checkArrivingWhereWeAreRecordsNothing() {
        var history = BrowseHistory()
        history.visit(orders)
        history.visit(orders)
        history.visit(orders)
        expect(history.visits.count, 1, "arriving where we are is not a move")
        expect(history.canGoBack, false, "so there is nothing behind us")
    }

    /// The tab is part of the identity. Describing a table and then browsing it
    /// are two places, and Back from the rows means the description.
    private static func checkTwoTabsOfOneTableAreTwoPlaces() {
        var history = BrowseHistory()
        history.visit(Visit(relationID: "public.orders", tab: .structure))
        history.visit(orders)
        expect(history.visits.count, 2, "one table on two tabs is two places")
        expect(
            history.goBack(), Visit(relationID: "public.orders", tab: .structure),
            "and back means the other tab, not the previous table")
    }

    /// `schema.name` names a different table on a different server, so the path
    /// cannot survive a reconnection.
    private static func checkConnectingElsewhereForgetsThePath() {
        var history = BrowseHistory()
        history.visit(orders)
        history.visit(regions)
        history.clear()
        expect(history.current, nil, "reconnecting leaves us nowhere")
        expect(history.canGoBack, false, "with no path behind")
        expect(history.visits, [], "and none recorded")
    }

    // MARK: - Fixture

    /// Four places on the Content tab, which is where this window spends its
    /// time; the structure tab appears in the one check that is about tabs.
    private static let orders = Visit(relationID: "public.orders", tab: .content)
    private static let regions = Visit(relationID: "sales.regions", tab: .content)
    private static let customers = Visit(relationID: "public.customers", tab: .content)
    private static let invoices = Visit(relationID: "sales.invoices", tab: .content)

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("history FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
