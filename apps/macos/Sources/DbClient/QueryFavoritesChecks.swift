import Foundation
import SwiftUI

/// Executable checks for the query favorites store, run by `--verify-favorites`.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum QueryFavoritesChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        MainActor.assumeIsolated {
            checkAFavoriteNeedsBothANameAndAStatement()
            checkTheListReadsByName()
            checkASnippetIsOnlyOfferedToItsOwnDatabase()
            checkImportingMergesRatherThanReplaces()
            checkTheListOutlivesTheWindow()
            checkSavingKeepsTheStatementThatWouldRun()
            checkAFavoriteArrivesInTheEditorReadyToRun()
            checkASecondOneIsAppendedRatherThanReplacing()
            checkAnExportedFileReadsBackAsTheListThatWroteIt()
            checkAFileThisBuildCannotReadLeavesTheListAlone()
        }
        if failures == 0 {
            fputs("favorites: all checks passed\n", stderr)
        } else {
            fputs("favorites: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// Both halves are required: an unnamed favorite cannot be found again, and
    /// an empty one has nothing to run.
    @MainActor private static func checkAFavoriteNeedsBothANameAndAStatement() {
        let store = scratch()
        expect(
            store.save(name: "  ", sql: "SELECT 1", scheme: "postgres") == nil, true,
            "a name of spaces is refused")
        expect(
            store.save(name: "Count", sql: "   ", scheme: "postgres") == nil, true,
            "and so is a statement of spaces")
        expect(store.favorites.count, 0, "so nothing was kept")

        // And the surrounding whitespace goes, so that a statement pasted with a
        // trailing newline is not stored differently from one typed.
        store.save(name: " Count ", sql: " SELECT 1\n", scheme: "postgres")
        expect(store.favorites.first?.name, "Count", "the name is trimmed")
        expect(store.favorites.first?.sql, "SELECT 1", "and so is the statement")
    }

    /// A favorite is looked up, not scanned, so the list reads alphabetically
    /// rather than newest-first the way the history does.
    @MainActor private static func checkTheListReadsByName() {
        let store = scratch()
        store.save(name: "Zombies", sql: "SELECT 1", scheme: "")
        store.save(name: "apples", sql: "SELECT 2", scheme: "")
        store.save(name: "Mangoes", sql: "SELECT 3", scheme: "")
        expect(
            store.favorites.map(\.name), ["apples", "Mangoes", "Zombies"],
            "by name, ignoring case")
    }

    /// The databases here disagree about quoting and about LIMIT, so a snippet
    /// written for one is a statement that cannot run on another.
    @MainActor private static func checkASnippetIsOnlyOfferedToItsOwnDatabase() {
        let store = scratch()
        store.save(name: "Postgres only", sql: "SELECT 1", scheme: "postgres")
        store.save(name: "MySQL only", sql: "SELECT 2", scheme: "mysql")
        store.save(name: "Anywhere", sql: "SELECT 3", scheme: "")
        expect(
            store.offered(to: "postgres").map(\.name), ["Anywhere", "Postgres only"],
            "a postgres connection is offered its own and the unattributed one")
        expect(
            store.offered(to: "mysql").map(\.name), ["Anywhere", "MySQL only"],
            "and a mysql connection the same")
    }

    /// An import adds to what is here. Replacing would make one mistaken import
    /// unrecoverable, and re-importing the same file must not double the list.
    @MainActor private static func checkImportingMergesRatherThanReplaces() {
        let store = scratch()
        store.save(name: "Mine", sql: "SELECT 1", scheme: "")
        let incoming = [
            QueryFavorite(
                id: UUID(uuidString: "00000000-0000-0000-0000-0000000000AA")!,
                name: "Theirs", sql: "SELECT 2", scheme: "", savedAt: Date())
        ]
        store.merge(incoming)
        expect(store.favorites.map(\.name), ["Mine", "Theirs"], "both lists are here")

        store.merge(incoming)
        expect(
            store.favorites.map(\.name), ["Mine", "Theirs"],
            "and importing the same file again changes nothing")

        let edited = [
            QueryFavorite(
                id: UUID(uuidString: "00000000-0000-0000-0000-0000000000AA")!,
                name: "Theirs, renamed", sql: "SELECT 2", scheme: "", savedAt: Date())
        ]
        store.merge(edited)
        expect(
            store.favorites.map(\.name), ["Mine", "Theirs, renamed"],
            "an entry that came back edited replaces the one it names")
    }

    /// A favorite that did not survive the window would be a worse note-taking
    /// tool than the editor it was saved from.
    @MainActor private static func checkTheListOutlivesTheWindow() {
        let name = suiteName()
        guard let store = UserDefaults(suiteName: name) else {
            failures += 1
            fputs("favorites FAIL: a scratch defaults suite could be made\n", stderr)
            return
        }
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let first = QueryFavorites(defaults: store)
        first.save(name: "Slow queries", sql: "SELECT 1", scheme: "postgres")

        // A second reader over the same store, which is what the next launch is.
        let second = QueryFavorites(defaults: store)
        expect(second.favorites.map(\.name), ["Slow queries"], "the favorite was kept")
        expect(second.favorites.first?.scheme, "postgres", "and so was what it was written for")
    }

    // MARK: - What the window does with them

    /// Save keeps what ⌘R would send, not the buffer around it.
    ///
    /// A window whose Save filed the whole editor would give one name to four
    /// statements, and the list would be useless the first time somebody kept a
    /// statement they had been experimenting beside.
    @MainActor private static func checkSavingKeepsTheStatementThatWouldRun() {
        guard let model = makeModel() else { return }
        model.activeTab = .query
        model.queryText = "SELECT 42"
        expect(model.savedQuery, "SELECT 42", "one statement is the statement")

        // Now two, with the caret standing in the second. Asked by what the
        // answer holds rather than by its exact spelling: what is being pinned
        // here is which statement was chosen, and the splitter owns where its
        // edges fall.
        model.queryText = "SELECT 1;\n\nSELECT 2"
        if let caret = SQLScript.range(12..<12, in: model.queryText) {
            model.querySelection = TextSelection(range: caret)
        }
        expect(model.savedQuery?.contains("SELECT 2"), true, "the caret's own statement")
        expect(
            model.savedQuery?.contains("SELECT 1"), false,
            "and not the buffer it is standing in")

        expect(model.saveQuery(named: "Second"), true, "and that is what Save keeps")
        expect(
            model.favorites.favorites.first?.sql.contains("SELECT 2"), true,
            "under the name it was given")

        // Nothing in the buffer is nothing to save, which is what disables the
        // control rather than filing an empty statement under a name.
        model.queryText = "   "
        expect(model.savedQuery == nil, true, "an empty buffer has nothing to keep")
        expect(model.saveQuery(named: "Nothing"), false, "so Save keeps nothing")
        expect(model.favorites.favorites.count, 1, "and the list is as it was")
    }

    /// Picking a favorite puts it in the editor selected, on the Query tab, with
    /// the panel closed — the same arrival a recalled statement gets, because
    /// the point of both lists is a statement that is ready for the ⌘R after it.
    @MainActor private static func checkAFavoriteArrivesInTheEditorReadyToRun() {
        guard let model = makeModel() else { return }
        model.activeTab = .content
        model.isHistoryOpen = true
        guard let favorite = model.favorites.save(name: "Count", sql: "SELECT 1", scheme: "")
        else {
            failures += 1
            fputs("favorites FAIL: the fixture favorite was kept\n", stderr)
            return
        }
        model.recall(favorite)
        expect(model.queryText, "SELECT 1", "the statement is in the editor")
        expect(model.activeTab, .query, "on the tab that can run it")
        expect(model.isHistoryOpen, false, "with the panel out of the way")
        expect(model.querySelection != nil, true, "and the statement selected, so ⌘R means it")
    }

    /// A second pick is appended with a terminator between, not dropped on top.
    ///
    /// The buffer is somebody's work: replacing it would make this list a way to
    /// lose the statement they were part way through, reached from a panel they
    /// opened to avoid retyping.
    @MainActor private static func checkASecondOneIsAppendedRatherThanReplacing() {
        guard let model = makeModel() else { return }
        model.queryText = "SELECT 1"
        guard let favorite = model.favorites.save(name: "Two", sql: "SELECT 2", scheme: "") else {
            failures += 1
            fputs("favorites FAIL: the fixture favorite was kept\n", stderr)
            return
        }
        model.recall(favorite)
        expect(model.queryText, "SELECT 1;\n\nSELECT 2", "both statements, separated")
    }

    // MARK: - The file

    /// A file written by this build reads back as what wrote it, whole.
    ///
    /// This is the entire promise of an export. A field dropped in the round
    /// trip is a statement that comes back offered to the wrong database, or
    /// under no name at all — and neither is visible until somebody needs the
    /// query.
    @MainActor private static func checkAnExportedFileReadsBackAsTheListThatWroteIt() {
        let store = scratch()
        store.save(name: "Postgres one", sql: "SELECT 1", scheme: "postgres")
        store.save(name: "Anywhere", sql: "SELECT 2", scheme: "")
        let written = store.favorites

        do {
            let data = try QueryFavorites.encoded(written)
            let read = try QueryFavorites.decoded(data)
            expect(read.map(\.id), written.map(\.id), "the same queries, in the same order")
            expect(read.map(\.name), written.map(\.name), "under the same names")
            expect(read.map(\.sql), written.map(\.sql), "holding the same statements")
            expect(read.map(\.scheme), written.map(\.scheme), "for the same databases")
            // To the second, not to the microsecond. ISO 8601 is what makes the
            // file legible, and `savedAt` exists only to break ties in the
            // ordering — a timestamp a person can read is worth more here than
            // one that survives a round trip bit for bit.
            expect(
                read.map { Int($0.savedAt.timeIntervalSince1970) },
                written.map { Int($0.savedAt.timeIntervalSince1970) },
                "saved at the same moment, to the second")
        } catch {
            failures += 1
            fputs("favorites FAIL: the list was written and read back: \(error)\n", stderr)
        }
    }

    /// A file this build cannot read is refused, and the list is untouched.
    ///
    /// The opposite of what the defaults store does with the same problem, and
    /// deliberately: an unreadable import that quietly emptied the list would
    /// destroy the thing it was asked to add to.
    @MainActor private static func checkAFileThisBuildCannotReadLeavesTheListAlone() {
        guard let model = makeModel() else { return }
        model.favorites.save(name: "Mine", sql: "SELECT 1", scheme: "")

        let file = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-favorites-\(UUID().uuidString).json")
        defer { try? FileManager.default.removeItem(at: file) }
        do {
            try Data("this is not a saved-queries file".utf8).write(to: file)
        } catch {
            failures += 1
            fputs("favorites FAIL: the fixture file was written: \(error)\n", stderr)
            return
        }

        model.importFavorites(from: file)
        expect(model.favorites.favorites.map(\.name), ["Mine"], "the list is as it was")
        expect(model.errorMessage != nil, true, "and the window says why nothing happened")
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
        let history = QueryHistory(defaults: UserDefaults(suiteName: suiteName())!)
        let preferences = Preferences(store: UserDefaults(suiteName: suiteName())!)
        return AppModel(history: history, favorites: scratch(), preferences: preferences)
    }

    private static func scratchDirectory() -> URL? {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "dbclient-verify-favorites-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        } catch {
            failures += 1
            fputs("favorites FAIL: a scratch directory could be made: \(error)\n", stderr)
            return nil
        }
        return root
    }

    private static func suiteName() -> String { "dev.dbclient.verify-favorites.\(UUID())" }

    /// A store on a suite of its own, so that running the checks cannot read or
    /// write the favorites a developer's own window is showing.
    @MainActor private static func scratch() -> QueryFavorites {
        QueryFavorites(defaults: UserDefaults(suiteName: suiteName())!)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("favorites FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
