import Foundation

/// The navigator tree as it was the last time a connection was open.
///
/// A cache and not a document, which is why it lives under `$XDG_CACHE_HOME`
/// rather than beside `connections.json`. Nothing here was typed by anybody,
/// every line of it can be rebuilt by connecting, and deleting the whole
/// directory costs one slow first look at each server. The rule for what belongs
/// in the config directory is whether somebody would miss it, and nobody misses
/// this.
///
/// Not encrypted, unlike the credential file two directories over. Schema and
/// table names are not a secret in the way a password is, and a key derived from
/// this machine would make a cache that cannot be read or deleted by hand —
/// which is the one thing a cache has to allow.
struct NavigatorCache {
    let directory: URL

    static var shared: NavigatorCache {
        NavigatorCache(
            directory: Self.cacheDirectory(
                xdgCacheHome: ProcessInfo.processInfo.environment["XDG_CACHE_HOME"],
                home: FileManager.default.homeDirectoryForCurrentUser
            ).appending(path: "dbclient/navigator"))
    }

    /// `$XDG_CACHE_HOME`, or `~/.cache` when it is unset.
    ///
    /// A relative value is ignored, which is the specification's own rule rather
    /// than an invention here — `ConnectionDirectories.localDirectory` follows it
    /// for the config directory, and a cache that moved with the working
    /// directory would be one that is never hit twice.
    static func cacheDirectory(xdgCacheHome: String?, home: URL) -> URL {
        guard let xdgCacheHome, xdgCacheHome.hasPrefix("/") else {
            return home.appending(path: ".cache")
        }
        return URL(filePath: xdgCacheHome)
    }

    /// What the navigator was drawing, for one database on one connection.
    ///
    /// The three things `AppModel.Inventory` carries that describe the tree, and
    /// not the two that do not: the server label belongs to the connection list
    /// and is already kept there, and what a connection can do is answered by the
    /// connection rather than remembered about it.
    struct Tree: Codable {
        let schemas: [SchemaInfo]
        let databases: [DatabaseInfo]?
        let relations: [String: [RelationInfo]]
    }

    /// One file per connection, holding one tree per database on it.
    ///
    /// Per connection rather than per database, because forgetting a connection
    /// has to take everything under it — and a connection somebody opened four
    /// databases on would otherwise leave three files nothing will ever name
    /// again.
    private struct Stored: Codable {
        var version: Int
        var trees: [String: Tree]
    }

    /// What this build writes and will read back.
    ///
    /// A file written in another shape is ignored rather than migrated. This is
    /// the one file in the application where that is the right answer: the cost
    /// of ignoring it is one slow first look at a server, and the code that would
    /// migrate it is code that has to be right about a format nobody can see.
    private static let version = 1

    func load(_ key: NavigatorCacheKey) -> Tree? {
        stored(key.connection)?.trees[key.database]
    }

    func save(_ tree: Tree, for key: NavigatorCacheKey) {
        var held = stored(key.connection) ?? Stored(version: Self.version, trees: [:])
        held.trees[key.database] = tree
        guard let data = try? JSONEncoder().encode(held) else { return }
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        // Failures are silent, and that is the whole of the policy: a cache that
        // could not be written costs a slow look next time, and a message about
        // it would be this application reporting its own housekeeping to somebody
        // who asked to see a table.
        try? data.write(to: file(key.connection), options: [.atomic])
    }

    /// Everything remembered about one connection, for the moment it is deleted.
    func forget(_ connection: UUID) {
        try? FileManager.default.removeItem(at: file(connection))
    }

    private func stored(_ connection: UUID) -> Stored? {
        guard let data = try? Data(contentsOf: file(connection)),
            let held = try? JSONDecoder().decode(Stored.self, from: data),
            held.version == Self.version
        else { return nil }
        return held
    }

    private func file(_ connection: UUID) -> URL {
        directory.appending(path: "\(connection.uuidString).json")
    }
}

/// Which tree: one database, on one saved connection.
///
/// The database is part of the key because opening another database on the same
/// server is a second connection under the same saved entry. One key for both
/// would draw the schemas of `sales` under `archive` for as long as it took the
/// real ones to arrive — which is exactly the window this cache exists to fill.
struct NavigatorCacheKey: Hashable {
    let connection: UUID
    let database: String
}
