import Foundation

/// Executable checks for what a browse dims while a page is on its way, run by
/// `--verify-progressive`.
///
/// One boolean, and worth a suite because getting it wrong is invisible in every
/// screenshot of a fast database: a veil over an appended page looks exactly like
/// a veil over a first one. The only person who finds out is somebody reading a
/// hundred thousand rows over a slow link, at the moment pressing *Load more*
/// takes away the rows they were reading.
///
/// Driven through `ResultSet` directly. Reaching these flags through a real
/// browse needs an Arrow table, which needs a server — the same limit
/// `RecordChecks` records at its head — and every rule below is about the flags
/// rather than about the rows.
enum ProgressiveLoadChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        MainActor.assumeIsolated {
            checkAFirstPageDimsWhatIsBehindIt()
            checkAnAppendedPageLeavesTheRowsAlone()
            checkAPageThatLandsEndsBothStates()
            checkACancelledFetchEndsBothStates()
            checkAFreshBrowseAfterAnAppendedPageDimsAgain()
            checkADiscardedResultIsNotStillFetching()
            checkTheStatusLineSaysAPageIsOnItsWay()
        }
        if failures == 0 {
            fputs("progressive: all checks passed\n", stderr)
        } else {
            fputs("progressive: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Which fetch dims what

    /// The first page of a browse dims the grid. There is nothing behind it
    /// worth reading — the grid is empty, or it is still holding the rows of the
    /// table just navigated away from — and leaving either undimmed presents it
    /// as the answer to the question being asked.
    @MainActor private static func checkAFirstPageDimsWhatIsBehindIt() {
        let result = ResultSet()
        result.beginLoading()
        expect(result.isLoading, true, "a first page is a fetch in flight")
        expect(result.isExtending, false, "and it is extending nothing")
        expect(result.isVeiled, true, "so what is behind it is dimmed")
    }

    /// A *Load more* does not dim. Those rows are the reader's own and they
    /// asked for more of them; covering them stops the reading the fetch exists
    /// to extend.
    ///
    /// It is still a fetch in flight, which is the half that is easy to lose:
    /// `canLoadMore` and `canExport` both ask `isLoading`, so a flag only the
    /// veil read would let a second *Load more* fire into a cursor already being
    /// fetched from.
    @MainActor private static func checkAnAppendedPageLeavesTheRowsAlone() {
        let result = ResultSet()
        result.beginLoading(appending: true)
        expect(result.isLoading, true, "an appended page is a fetch in flight too")
        expect(result.isExtending, true, "and it says which kind it is")
        expect(result.isVeiled, false, "so the rows stay readable while it runs")
    }

    // MARK: - Every way a fetch ends

    /// A page that lands ends both states, whichever kind of page it was. An
    /// `isExtending` left set is a status line reading "loading more…" over a
    /// result that finished arriving.
    @MainActor private static func checkAPageThatLandsEndsBothStates() {
        let appended = ResultSet()
        appended.beginLoading(appending: true)
        appended.extend(capped: false, milliseconds: 1, summary: "orders · 3 rows")
        expect(appended.isLoading, false, "the appended page is no longer in flight")
        expect(appended.isExtending, false, "and no longer extending")

        let first = ResultSet()
        first.beginLoading()
        first.finish(
            statement: "select 1", capped: false, milliseconds: 1, summary: "orders · 3 rows")
        expect(first.isLoading, false, "the first page is no longer in flight")
        expect(first.isExtending, false, "and never became an extension")
    }

    /// Cancel ends them too. It is the one path that leaves the result exactly
    /// as it was, so it is also the one where a flag left set stays set for as
    /// long as the window is open.
    @MainActor private static func checkACancelledFetchEndsBothStates() {
        let result = ResultSet()
        result.beginLoading(appending: true)
        result.abandonLoading()
        expect(result.isLoading, false, "the cancelled fetch is not in flight")
        expect(result.isExtending, false, "and not still extending")
        expect(result.isVeiled, false, "and nothing is left dimmed")
    }

    /// Picking another table after a *Load more* dims again.
    ///
    /// This is the case that fails if `beginLoading` ever stops assigning the
    /// flag and starts only raising it. Nothing else would: every browse for the
    /// rest of the session would run undimmed, showing the previous table's rows
    /// as though they were the new one's, and no other check here would notice.
    @MainActor private static func checkAFreshBrowseAfterAnAppendedPageDimsAgain() {
        let result = ResultSet()
        result.beginLoading(appending: true)
        result.beginLoading()
        expect(result.isExtending, false, "the new browse extends nothing")
        expect(result.isVeiled, true, "so it dims what the last table left on screen")
    }

    /// A result whose relation has stopped existing is dropped whole, and that
    /// includes the fetch it believed it was in the middle of.
    @MainActor private static func checkADiscardedResultIsNotStillFetching() {
        let result = ResultSet()
        result.beginLoading(appending: true)
        result.discard()
        expect(result.isLoading, false, "the discarded result is not in flight")
        expect(result.isExtending, false, "and not extending")
        expect(result.isVeiled, false, "and not dimmed")
    }

    // MARK: - What the window says instead

    /// The status line carries the feedback the veil used to.
    ///
    /// `canLoadMore` reads `isLoading`, so the *Load more* button disappears the
    /// instant it is pressed. Without this sentence the window answers the click
    /// by doing nothing visible at all for the length of a hundred-thousand-row
    /// fetch, which reads as a button that does not work.
    @MainActor private static func checkTheStatusLineSaysAPageIsOnItsWay() {
        guard let model = makeModel() else { return }
        let settled = "orders · first 100,000 rows · 0.12 s"
        model.browseResult.extend(capped: true, milliseconds: 120, summary: settled)
        expect(model.statusLine, settled, "a settled result reads as its own summary")
        model.browseResult.beginLoading(appending: true)
        expect(
            model.statusLine, "\(settled) · loading more…",
            "and a page on its way is said rather than left to be noticed")
    }

    // MARK: - Fixture

    /// A model on scratch stores throughout, with the config redirected.
    ///
    /// The redirect is not optional: without it the model reads the user's saved
    /// connections and asks the Keychain for the first one's password, which in
    /// a process with no GUI session blocks forever — so the symptom is not a
    /// failed check but a `make test-swift` that never returns.
    @MainActor private static func makeModel() -> AppModel? {
        guard let directory = scratchDirectory() else { return nil }
        setenv("XDG_CONFIG_HOME", directory.path, 1)
        return AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-progressive")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-progressive")),
            preferences: Preferences(store: ScratchDefaults.store("verify-progressive")))
    }

    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-progressive-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            failures += 1
            fputs("progressive FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("progressive FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
