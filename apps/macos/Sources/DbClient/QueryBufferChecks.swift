import Foundation

/// Executable checks for the query buffers behind the session tab bar, run by
/// `--verify-query-buffers`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
///
/// What is pinned here is the arithmetic of the list — which buffer the editor
/// is on after a close, and that two buffers hold two texts. What the tab strip
/// draws is not pinned and cannot be from here: that needs a window.
enum QueryBufferChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        MainActor.assumeIsolated {
            checkANewBufferIsTheOneBeingEdited()
            checkEachBufferKeepsItsOwnText()
            checkClosingTheActiveLastBufferFallsBackOne()
            checkClosingBelowTheActiveOneKeepsItActive()
            checkTheLastBufferCannotBeClosed()
            checkRenamingTakesAndIsTrimmed()
            checkANamelessBufferIsRefused()
        }
        if failures == 0 {
            fputs("query buffers: all checks passed\n", stderr)
        } else {
            fputs("query buffers: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// Opening a tab puts the editor in it. A buffer that is added without
    /// being switched to is a tab that appears and does nothing.
    @MainActor private static func checkANewBufferIsTheOneBeingEdited() {
        guard let model = makeModel() else { return }
        model.addQueryBuffer()
        expect(model.queryBuffers.count, 2, "the second buffer is in the list")
        expect(model.activeQueryBufferIndex, 1, "and it is the one the editor is on")
    }

    /// Two tabs, two texts. This is the whole point of the list: a buffer
    /// showing its neighbour's statement would be one editor wearing two names.
    @MainActor private static func checkEachBufferKeepsItsOwnText() {
        guard let model = makeModel() else { return }
        model.queryText = "select 1"
        model.addQueryBuffer()
        model.queryText = "select 2"
        expect(model.queryText, "select 2", "the new buffer holds what was typed into it")
        model.selectQueryBuffer(0)
        expect(model.queryText, "select 1", "and the first one still holds its own")
    }

    /// Closing the rightmost tab leaves the editor on the one to its left,
    /// which is where the eye already is and the only index the list still has.
    @MainActor private static func checkClosingTheActiveLastBufferFallsBackOne() {
        guard let model = makeModel() else { return }
        model.addQueryBuffer()
        model.closeQueryBuffer(1)
        expect(model.queryBuffers.count, 1, "the buffer is gone")
        expect(model.activeQueryBufferIndex, 0, "and the editor fell back to the one before it")
    }

    /// Closing a tab to the left of the active one shifts that index down with
    /// the list. Leaving it where it was would swap the text under the caret
    /// for a neighbour's — the buffer somebody is typing in is not the one they
    /// closed.
    @MainActor private static func checkClosingBelowTheActiveOneKeepsItActive() {
        guard let model = makeModel() else { return }
        model.queryText = "first"
        model.addQueryBuffer()
        model.queryText = "second"
        model.addQueryBuffer()
        model.queryText = "third"
        model.closeQueryBuffer(0)
        expect(model.queryBuffers.count, 2, "one of the three is gone")
        expect(model.activeQueryBufferIndex, 1, "the active one moved down with the list")
        expect(model.queryText, "third", "and it is still the text that was being edited")
    }

    /// An editor with nowhere to type is not a state this window has.
    @MainActor private static func checkTheLastBufferCannotBeClosed() {
        guard let model = makeModel() else { return }
        model.closeQueryBuffer(0)
        expect(model.queryBuffers.count, 1, "the only buffer stayed")
    }

    /// The name is what the strip draws, and the ends of a typed one are never
    /// meant. Refusing over a trailing space would be a rename that appeared to
    /// do nothing.
    @MainActor private static func checkRenamingTakesAndIsTrimmed() {
        guard let model = makeModel() else { return }
        model.addQueryBuffer()
        model.renameQueryBuffer(1, to: "  daily report\n")
        expect(model.queryBuffers[1].name, "daily report", "the typed name is kept, trimmed")
        expect(model.queryBuffers[0].name, "query 1", "and the buffer beside it is untouched")
        model.renameQueryBuffer(7, to: "nowhere")
        expect(model.queryBuffers.count, 2, "a rename of a buffer that is not there does nothing")
    }

    /// A buffer's name is the whole of its presence in the strip. One made of
    /// spaces is a tab with no width — reachable by ⌘⇧] and by nothing a pointer
    /// can do.
    @MainActor private static func checkANamelessBufferIsRefused() {
        guard let model = makeModel() else { return }
        model.renameQueryBuffer(0, to: "   ")
        expect(model.queryBuffers[0].name, "query 1", "a name of spaces is refused")
        model.renameQueryBuffer(0, to: "")
        expect(model.queryBuffers[0].name, "query 1", "and so is an empty one")
    }

    // MARK: - Fixture

    /// A model on scratch stores throughout, with the config redirected.
    ///
    /// The redirect is not optional: without it the model reads the user's
    /// saved connections and asks the Keychain for the first one's password,
    /// which in a process with no GUI session blocks forever — so the symptom
    /// is not a failed check but a `make test-swift` that never returns.
    @MainActor private static func makeModel() -> AppModel? {
        guard let directory = scratchDirectory() else { return nil }
        setenv("XDG_CONFIG_HOME", directory.path, 1)
        return AppModel(
            history: QueryHistory(defaults: ScratchDefaults.store("verify-query-buffers")),
            favorites: QueryFavorites(defaults: ScratchDefaults.store("verify-query-buffers")),
            preferences: Preferences(store: ScratchDefaults.store("verify-query-buffers")))
    }

    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-query-buffers-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            failures += 1
            fputs("query buffers FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("query buffers FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
