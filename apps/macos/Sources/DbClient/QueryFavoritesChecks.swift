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
        defer { ScratchDefaults.release() }
        MainActor.assumeIsolated {
            checkAFavoriteNeedsBothANameAndAStatement()
            checkTheListReadsByName()
            checkASnippetIsOnlyOfferedToItsOwnDatabase()
            checkImportingMergesRatherThanReplaces()
            checkTheListOutlivesTheWindow()
            checkSavingKeepsTheStatementThatWouldRun()
            checkAFavoriteArrivesInTheEditorReadyToRun()
            checkASecondOneIsAppendedRatherThanReplacing()
            checkOneWithBlanksArrivesOnTheFirstOfThem()
            checkAnExportedFileReadsBackAsTheListThatWroteIt()
            checkAFileThisBuildCannotReadLeavesTheListAlone()
            checkAFullListRefusesTheNewestAndSaysSo()
            checkLoweringTheLimitDeletesNothing()
            checkAnImportPastTheLimitSaysWhatItLeftOut()
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
        let store = ScratchDefaults.store("verify-favorites")

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

    /// A saved query holding `${…}` arrives with the first blank selected, not
    /// with the whole statement selected.
    ///
    /// The whole-statement selection exists so that the ⌘R after a recall sends
    /// exactly what arrived. A statement still holding a blank is one no server
    /// will accept, so that arrival would be aiming Run at something that cannot
    /// run; the first thing to type is what it needs instead. Both halves are
    /// pinned here — the blank one lands on a blank, the plain one still lands on
    /// everything — because getting either wrong looks identical on screen until
    /// somebody presses a key.
    @MainActor private static func checkOneWithBlanksArrivesOnTheFirstOfThem() {
        guard let model = makeModel() else { return }
        model.recall(
            QueryFavorite(
                id: UUID(), name: "Rows by column",
                sql: "SELECT * FROM ${table} WHERE ${column} = ${value}", scheme: "",
                savedAt: Date()))
        expect(selected(in: model), "${table}", "the first blank, not the statement around it")

        // And Tab from there is the walk, which is what makes the arrival worth
        // anything: the editor asks the same rule with the same offsets.
        expect(
            EditorTyping.placeholderJump(
                in: model.queryText, selection: 14..<22)?.selection,
            29..<38, "with the next blank one Tab away")

        guard let plain = makeModel() else { return }
        plain.recall(
            QueryFavorite(
                id: UUID(), name: "Count", sql: "SELECT count(*) FROM orders", scheme: "",
                savedAt: Date()))
        expect(
            selected(in: plain), "SELECT count(*) FROM orders",
            "a statement with no blanks is still selected whole, ready for ⌘R")
    }

    /// What the editor's selection covers, as the text it names.
    @MainActor private static func selected(in model: AppModel) -> String? {
        guard let indices = model.querySelection?.indices,
            case .selection(let range) = indices
        else { return nil }
        return String(model.queryText[range])
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

    // MARK: - How many are kept

    /// A full list refuses the newest, and says which limit refused it.
    ///
    /// The opposite of the history's rule and deliberately so. These have names
    /// somebody typed, so the entry that loses is the one whose owner is
    /// standing here to be told — which is the whole difference between this and
    /// an eviction, and the reason it has to be said out loud rather than
    /// returned as a nil.
    @MainActor private static func checkAFullListRefusesTheNewestAndSaysSo() {
        guard let model = makeModel() else { return }
        model.preferences.favoritesLimit = 2
        model.favorites.save(name: "One", sql: "SELECT 1", scheme: "")
        model.favorites.save(name: "Two", sql: "SELECT 2", scheme: "")

        // The tab as well as the text: `savedQuery` is nil anywhere else, and a
        // Save refused for that reason would satisfy the assertion below while
        // proving nothing about the limit.
        model.activeTab = .query
        model.queryText = "SELECT 3"
        expect(model.saveQuery(named: "Three"), false, "the third is refused")
        expect(
            model.errorMessage?.contains("full at 2"), true,
            "and the limit that refused it is named, so the number is findable in Settings")
        expect(model.favorites.favorites.count, 2, "with the two that were there untouched")
        expect(
            model.favorites.favorites.contains { $0.name == "One" }, true,
            "including the oldest, which an evicting limit would have taken")

        // And the store refuses it on its own account. `isFull` above is what
        // lets the window name the number; the rule itself belongs to `save`,
        // or the limit would be something only a caller who remembered to ask
        // obeys — and `merge` on the same store already enforces it.
        expect(
            model.favorites.save(name: "Four", sql: "SELECT 4", scheme: "") == nil, true,
            "the store refuses a full list without being asked first")
        expect(model.favorites.favorites.count, 2, "and still keeps two")
    }

    /// Lowering the limit deletes nothing.
    ///
    /// A limit on named work is a brake, not a cap: it stops more going in. A
    /// number somebody types into Settings must not be a delete button for
    /// statements they wrote — and the failure this pins is silent, because a
    /// list that quietly shortened would look exactly like a list that had been
    /// that length.
    @MainActor private static func checkLoweringTheLimitDeletesNothing() {
        guard let model = makeModel() else { return }
        for i in 0..<5 {
            model.favorites.save(name: "Query \(i)", sql: "SELECT \(i)", scheme: "")
        }
        expect(model.favorites.favorites.count, 5, "five were kept with no limit set")

        model.preferences.favoritesLimit = 2
        expect(model.favorites.favorites.count, 5, "and all five survive the limit being lowered")
        expect(model.favorites.isFull, true, "the list is simply full, so nothing more goes in")

        model.activeTab = .query
        model.queryText = "SELECT 6"
        expect(model.saveQuery(named: "Six"), false, "which is what the next Save runs into")
        expect(
            model.errorMessage?.contains("full at 2"), true,
            "and is told so rather than silently doing nothing")
        expect(model.favorites.favorites.count, 5, "and it still deletes nothing")
    }

    /// An import past the limit says how many it left out.
    ///
    /// The one case where somebody can walk away believing they have a statement
    /// they do not. A replacement is taken even when full — it is not a new
    /// entry, and refusing it would make re-importing an edited file depend on
    /// how full the list happened to be.
    @MainActor private static func checkAnImportPastTheLimitSaysWhatItLeftOut() {
        guard let model = makeModel() else { return }
        model.preferences.favoritesLimit = 3
        // Filled to the limit first. With room to spare the replacement below
        // would be taken for the ordinary reason and would say nothing about
        // being full, which is the whole claim.
        let saved = (0..<3).compactMap {
            model.favorites.save(name: "Kept \($0)", sql: "SELECT \($0)", scheme: "")
        }
        guard let kept = saved.first, saved.count == 3 else {
            failures += 1
            fputs("favorites FAIL: the three fixture favorites were kept\n", stderr)
            return
        }
        expect(model.favorites.isFull, true, "the list is full before the import arrives")

        let edited = QueryFavorite(
            id: kept.id, name: "Kept 0, renamed", sql: "SELECT 0", scheme: "",
            savedAt: kept.savedAt)
        let incoming =
            [edited]
            + (0..<2).map {
                QueryFavorite(
                    id: UUID(), name: "New \($0)", sql: "SELECT new \($0)", scheme: "",
                    savedAt: Date())
            }
        expect(model.favorites.merge(incoming), 2, "both new ones had nowhere to go")
        expect(model.favorites.favorites.count, 3, "the list stops at its limit")
        expect(
            model.favorites.favorites.contains { $0.name == "Kept 0, renamed" }, true,
            "and the one that replaced an entry was taken even though the list was full")
    }

    // MARK: - Fixture

    /// A model on scratch stores throughout, with the config redirected.
    ///
    /// The redirect is not optional: without it the model reads the user's saved
    /// connections and asks the Keychain for the first one's password, which in
    /// a process with no GUI session blocks forever — so the symptom is not a
    /// failed check but a `make test-swift` that never returns.
    /// One suite for all three, and a fresh one per model — `ScratchDefaults`
    /// mints a new domain on every call, so this has to be minted once and
    /// shared rather than asked for three times.
    ///
    /// Shared because the limits are preferences the lists read out of the same
    /// defaults they keep their entries in: three suites would be a model whose
    /// Settings could not reach its own lists, which passes every case that does
    /// not set one and silently answers "no limit" to every case that does.
    @MainActor private static func makeModel() -> AppModel? {
        guard let directory = scratchDirectory() else { return nil }
        setenv("XDG_CONFIG_HOME", directory.path, 1)
        let store = ScratchDefaults.store("verify-favorites")
        return AppModel(
            history: QueryHistory(defaults: store),
            favorites: QueryFavorites(defaults: store),
            preferences: Preferences(store: store))
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

    /// A store on a suite of its own, so that running the checks cannot read or
    /// write the favorites a developer's own window is showing.
    @MainActor private static func scratch() -> QueryFavorites {
        QueryFavorites(defaults: ScratchDefaults.store("verify-favorites"))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("favorites FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
