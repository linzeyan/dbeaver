import Foundation
import Observation

/// A statement somebody chose to keep, under a name they chose for it.
///
/// The name is typed rather than derived. `QueryHistoryEntry.preview` takes the
/// first line because a history is scanned for something recognised; a favorite
/// is looked up for something wanted, and half of these would otherwise be
/// listed as `SELECT`.
struct QueryFavorite: Codable, Identifiable, Equatable {
    let id: UUID
    var name: String
    /// The statement, verbatim. This is what goes back into the editor, so it is
    /// kept as it was written rather than reformatted.
    var sql: String
    /// The scheme this was written for, or empty for one with no opinion.
    ///
    /// Kept because the databases here disagree about quoting, about LIMIT, and
    /// about half their functions: a MySQL snippet offered to PostgreSQL is a
    /// statement that cannot run, which is worse than one not offered. Empty is
    /// the honest answer for a statement saved with no connection open, and
    /// those are offered everywhere rather than nowhere.
    var scheme: String
    let savedAt: Date
}

/// The statements this window keeps by name, across launches.
///
/// `UserDefaults` under a key of its own, exactly as `QueryHistory` does and for
/// the same reason: one JSON blob under one key is all this needs, and a shared
/// container is a place for two features written at the same time to collide.
///
/// No limit, unlike the history's. A history is a log and its oldest entries are
/// worth losing; a favorite is something a person typed a name for, and a store
/// that silently dropped the two-hundred-and-first would be deleting their work.
@Observable
@MainActor
final class QueryFavorites {
    /// By name, case-insensitively, which is the order the list is read in.
    /// Insertion order would put the newest first, and a favorite is looked up
    /// rather than scanned.
    private(set) var favorites: [QueryFavorite] = []

    private static let key = "dev.dbclient.queryFavorites"

    private let defaults: UserDefaults

    /// The store is injectable so that a check can be given a scratch one, the
    /// way `QueryHistory`'s is. Everything else takes the default.
    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        favorites = Self.sorted(Self.load(from: defaults))
    }

    /// The ones worth offering on a connection using `scheme`.
    ///
    /// A favorite with no scheme is offered everywhere: it was saved before a
    /// connection existed, so nothing is known against it.
    func offered(to scheme: String) -> [QueryFavorite] {
        favorites.filter { $0.scheme.isEmpty || $0.scheme == scheme }
    }

    /// Keeps a statement, and answers with what was kept.
    ///
    /// Nil where there is nothing to keep. A favorite needs both halves: an
    /// unnamed one cannot be found again, and an empty one has nothing to run.
    ///
    /// Two favorites may share a name. The name is a label its owner chose, not
    /// a key — someone keeping `count` for four databases has named them
    /// honestly, and refusing the fourth would be this store having an opinion
    /// about their filing.
    @discardableResult
    func save(name: String, sql: String, scheme: String) -> QueryFavorite? {
        let title = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let statement = sql.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, !statement.isEmpty else { return nil }
        let favorite = QueryFavorite(
            id: UUID(), name: title, sql: statement, scheme: scheme, savedAt: Date())
        favorites = Self.sorted(favorites + [favorite])
        write()
        return favorite
    }

    /// Renames one, ignoring a name that is only whitespace.
    func rename(_ id: UUID, to name: String) {
        let title = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, let index = favorites.firstIndex(where: { $0.id == id }) else {
            return
        }
        favorites[index].name = title
        favorites = Self.sorted(favorites)
        write()
    }

    func remove(_ id: UUID) {
        favorites.removeAll { $0.id == id }
        write()
    }

    /// Takes in what an import read.
    ///
    /// Merges rather than replaces. An import is somebody adding a colleague's
    /// snippets to their own, and a file that silently emptied the list would
    /// make the first mistaken import unrecoverable. An entry whose id is
    /// already here replaces that one, so importing the same file twice leaves
    /// one copy rather than two.
    func merge(_ incoming: [QueryFavorite]) {
        var merged = favorites
        for favorite in incoming {
            if let index = merged.firstIndex(where: { $0.id == favorite.id }) {
                merged[index] = favorite
            } else {
                merged.append(favorite)
            }
        }
        favorites = Self.sorted(merged)
        write()
    }

    /// Ordered by name, then by when it was saved so that two with one name keep
    /// a stable order rather than swapping places between launches.
    private static func sorted(_ favorites: [QueryFavorite]) -> [QueryFavorite] {
        favorites.sorted {
            let left = $0.name.lowercased()
            let right = $1.name.lowercased()
            if left != right { return left < right }
            return $0.savedAt < $1.savedAt
        }
    }

    private func write() {
        // A failure here costs the list and nothing else, and the only way
        // `[QueryFavorite]` fails to encode is a bug in this file.
        guard let data = try? JSONEncoder().encode(favorites) else { return }
        defaults.set(data, forKey: Self.key)
    }

    /// Reads what a previous launch wrote, and starts empty when it cannot.
    ///
    /// Unreadable data is dropped rather than migrated, for the reason
    /// `QueryHistory.load` gives: refusing to open because a build from last
    /// week wrote a different shape is a worse trade than opening having
    /// forgotten.
    private static func load(from defaults: UserDefaults) -> [QueryFavorite] {
        guard let data = defaults.data(forKey: key),
            let decoded = try? JSONDecoder().decode([QueryFavorite].self, from: data)
        else { return [] }
        return decoded
    }
}
