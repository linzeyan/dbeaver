import Foundation
import Security

/// Where to connect, as the connection form collects it.
///
/// Deliberately holds no password. This is the value that gets written to
/// UserDefaults, and a type that cannot carry a secret cannot leak one into a
/// plist through a later edit; the password travels beside it and lives in the
/// Keychain. `connectionString(password:)` is the one place the two meet.
struct ConnectionSettings: Equatable, Codable {
    /// Which database this connects to, as the scheme the core dispatches on.
    ///
    /// Held rather than derived, because for a file-shaped database there is
    /// nothing to derive it from: `/Users/me/notes.db` is a SQLite file and a
    /// DuckDB file and looks identical either way.
    var scheme: String
    var host: String
    var port: String
    var database: String
    var user: String
    /// Where the database lives, for a file-shaped driver. Empty for a server.
    var path: String

    init(
        scheme: String, host: String = "", port: String = "", database: String = "",
        user: String = "", path: String = ""
    ) {
        self.scheme = scheme
        self.host = host
        self.port = port
        self.database = database
        self.user = user
        self.path = path
    }

    var driver: DriverInfo? { DriverCatalog.named(scheme) }

    /// What an empty form offers.
    ///
    /// A loopback host and whichever port the chosen driver names are the two
    /// fields with a defensible default; a guessed database or user name would
    /// only be a value to delete.
    static func suggested(for driver: DriverInfo) -> ConnectionSettings {
        ConnectionSettings(
            scheme: driver.scheme,
            host: driver.shape == .server ? "127.0.0.1" : "",
            port: driver.defaultPort.map(String.init) ?? "")
    }

    /// Moves these settings to another database, keeping what still applies.
    ///
    /// Switching driver in the picker must not empty the form. The host and
    /// database someone has typed are still the host and database they want; the
    /// port is the one field that belongs to the old driver, so it moves to the
    /// new one's default — but only if it was the old default, since a port
    /// typed by hand was typed for a reason.
    func moved(to driver: DriverInfo) -> ConnectionSettings {
        var next = self
        next.scheme = driver.scheme
        let wasDefault = self.driver?.defaultPort.map(String.init) == port
        if wasDefault || port.isEmpty {
            next.port = driver.defaultPort.map(String.init) ?? ""
        }
        if driver.shape == .server && next.host.isEmpty {
            next.host = "127.0.0.1"
        }
        return next
    }

    /// Whether these name a database worth trying. The Connect button reads it:
    /// a form missing the database produces a server error about a field the
    /// user can see is empty, which is a worse answer than a disabled button.
    ///
    /// What counts as filled in depends on the driver. A file needs a path and
    /// nothing else — there is no server to authenticate to, and requiring a
    /// user name for SQLite would be asking for something that does not exist.
    var isComplete: Bool {
        switch driver?.shape {
        case .file: return !path.trimmingCharacters(in: .whitespaces).isEmpty
        case .server, nil:
            return ![host, port, database, user].contains {
                $0.trimmingCharacters(in: .whitespaces).isEmpty
            }
        }
    }

    /// The connection URL these settings and that password describe.
    ///
    /// A URL rather than the `keyword=value` string libpq takes, because the
    /// core has more than one database behind it and the scheme is how it knows
    /// which. A bare `host=… port=…` names no driver, and a client that guessed
    /// between them would be one that connects to the wrong database without
    /// saying so.
    ///
    /// The fields are trimmed; the password is not. A hostname pasted with a
    /// trailing space is the commonest way to spend five minutes on a connection
    /// error, while whitespace inside a password is a character the server is
    /// going to check.
    func connectionString(password: String) -> String {
        func trimmed(_ value: String) -> String {
            value.trimmingCharacters(in: .whitespaces)
        }
        var url = URLComponents()
        url.scheme = scheme

        if driver?.shape == .file {
            // Three slashes for an absolute path, two for one relative to where
            // the client was started: an empty authority, then the path. Setting
            // `host` to "" is what produces the `//` that the core's registry
            // splits on.
            url.host = ""
            let p = trimmed(path)
            url.path = p.hasPrefix("/") ? p : "/" + p
            return url.string ?? ""
        }

        url.host = trimmed(host)
        url.port = Int(trimmed(port))
        url.user = trimmed(user).isEmpty ? nil : trimmed(user)
        // Absent rather than empty: an empty password is a trust or peer
        // connection, and sending `:@` states something the form did not.
        url.password = password.isEmpty ? nil : password
        url.path = "/" + trimmed(database)
        // URLComponents percent-encodes what it is given, which is the whole
        // reason this is not string interpolation: a password holding `@` or `/`
        // would otherwise be read as the end of the credentials.
        return url.string ?? ""
    }

    /// The form's values for a URL this did not write.
    ///
    /// `--conn` comes from a person or a Makefile, and one that does not connect
    /// has to land back in the form for them to correct. Retyping four fields to
    /// fix one character is the difference between a client and a demonstration.
    ///
    /// A URL naming a driver this build does not have keeps its scheme rather
    /// than being silently reassigned to a working one. The form shows it as
    /// unknown, which is true, instead of offering to connect somewhere the user
    /// did not ask for.
    init(connectionString text: String) {
        let url = URLComponents(string: text)
        scheme = url?.scheme ?? DriverCatalog.first?.scheme ?? ""
        let shape = DriverCatalog.named(scheme)?.shape
        let rawPath = url?.path ?? ""

        if shape == .file {
            // A relative path parses as the authority, so it has to be put back
            // in front of the path or `sqlite://notes.db` reads as a host.
            path = (url?.host ?? "") + rawPath
            host = ""
            port = ""
            database = ""
            user = ""
            return
        }

        path = ""
        host = url?.host ?? ""
        port = url?.port.map(String.init) ?? ""
        database = rawPath.hasPrefix("/") ? String(rawPath.dropFirst()) : ""
        user = url?.user ?? ""
    }
}

/// The parts of a connection URL that are read rather than written.
///
/// Small on purpose. `ConnectionSettings` writes these strings and reads back
/// the ones it could have written; this is for the two questions asked of a
/// string that arrived from somewhere else.
enum ConnectionURL {
    /// The password a URL carries, if it carries one.
    ///
    /// Wanted because `--conn` seeds the form, and a form seeded without the
    /// password would make the user retype the one field they cannot see.
    static func password(in text: String) -> String? {
        URLComponents(string: text)?.password
    }

    /// Which database a URL names, as the scheme the core dispatches on.
    ///
    /// Wanted because the scheme picks a SQL dialect as well as a driver: the
    /// editor reads `"a"` as a quoted identifier against PostgreSQL and as a
    /// string against MySQL, and it can only ask the right question if it knows
    /// which database is on the other end. Empty for a string that is not a URL,
    /// which the core reads as PostgreSQL.
    static func scheme(in text: String) -> String {
        URLComponents(string: text)?.scheme ?? ""
    }

    /// How a session is named in a tab: `database@host`, or the file name for a
    /// database that is a file.
    ///
    /// Written without knowing which schemes are files, because that list is the
    /// core's and would go stale here. Two rules do it: the label is the last
    /// segment of the path, which is a no-op for a database name and picks the
    /// file out of a path; and the host is appended when there is one. A SQLite
    /// URL has no host, so it is named by its file rather than by a server that
    /// is not there.
    static func label(for text: String) -> String {
        guard let url = URLComponents(string: text) else { return "database" }
        let name = url.path.split(separator: "/").last.map(String.init)
        let host = url.host.flatMap { $0.isEmpty ? nil : $0 }
        switch (name, host) {
        case (let name?, let host?): return "\(name)@\(host)"
        case (let name?, nil): return name
        // A relative file path parses as the authority, so a host with nothing
        // after it is as likely to be a file as a server. Either way it is the
        // only name there is.
        case (nil, let host?): return host
        case (nil, nil): return "database"
        }
    }
}

/// Where the connection this window remembers is kept, as the Settings window
/// offers it.
///
/// Two answers, and they differ in where one file goes. The fields are a JSON
/// file: on this Mac it sits under `$XDG_CONFIG_HOME` — `~/.config` when that is
/// unset — where a developer can read it, edit it, copy it to another machine or
/// keep it in their own dotfiles, which is what a connection to a database is to
/// the people who use one. In iCloud the same file is written into iCloud Drive
/// instead, so a second Mac signed in to the same Apple Account opens the same
/// database without being told about it.
///
/// The password is in neither copy. It stays in the login Keychain under both
/// answers — see `ConnectionKeychain` — because a config file a developer can
/// read is also a config file every process running as them can read, and that
/// is not where a database password goes. Only the Keychain's own synchronised
/// item can carry it off the machine, and only for a build macOS will give one
/// to; where it will not, the other Mac gets the fields and asks for the
/// password once.
enum ConnectionStorage: String, CaseIterable, Identifiable {
    case thisMac
    case iCloud

    var id: String { rawValue }

    var label: String {
        switch self {
        case .thisMac: return "On this Mac"
        case .iCloud: return "In iCloud"
        }
    }
}

/// The two directories the remembered connection can be written to.
///
/// A value rather than two functions, so the checks can hand in a scratch pair:
/// exercising this against the real answer would leave a fixture in the
/// developer's own `~/.config` and — worse, because it would then travel — in
/// their iCloud Drive.
struct ConnectionDirectories {
    let local: URL
    /// Nil when iCloud Drive is not set up on this Mac. The folder is created by
    /// the system when the user turns iCloud Drive on, so its absence is the
    /// answer rather than something to ask an API about.
    let cloud: URL?

    static var system: ConnectionDirectories {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return ConnectionDirectories(
            local: localDirectory(
                xdgConfigHome: ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"], home: home),
            cloud: cloudDirectory(home: home))
    }

    /// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset.
    ///
    /// A relative value is ignored, which is the specification's own rule and not
    /// an invention here: `XDG_CONFIG_HOME=.config` in a shell profile would
    /// otherwise put the file wherever the application happened to be launched
    /// from, and a remembered connection that moves with the working directory is
    /// one nobody can find twice.
    static func localDirectory(xdgConfigHome: String?, home: URL) -> URL {
        guard let xdgConfigHome, xdgConfigHome.hasPrefix("/") else {
            return home.appending(path: ".config")
        }
        return URL(filePath: xdgConfigHome)
    }

    static func cloudDirectory(home: URL) -> URL? {
        let drive = home.appending(path: "Library/Mobile Documents/com~apple~CloudDocs")
        return FileManager.default.fileExists(atPath: drive.path) ? drive : nil
    }

    func directory(for storage: ConnectionStorage) -> URL? {
        storage == .iCloud ? cloud : local
    }
}

/// The file, as a whole.
///
/// A wrapper around the array rather than a bare array at the top level, because
/// the document needs somewhere to say which shape it is in. A file that is only a
/// list can never gain a field without every older build reading the newer one
/// wrongly, and this is a file meant to be carried between machines.
struct SavedConnections: Codable {
    /// The shape the entries are in. Bumped when an entry stops meaning what it
    /// used to; a document numbered higher than this build knows about is read as
    /// no connections at all, which asks the user for one rather than showing them
    /// fields interpreted under the wrong shape.
    static let currentVersion = 1

    var version: Int
    var connections: [SavedConnection]

    init(connections: [SavedConnection], version: Int = SavedConnections.currentVersion) {
        self.version = version
        self.connections = connections
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        // A file with no version at all is one somebody wrote by hand, not one from
        // the future, so it is read rather than refused.
        version = try container.decodeIfPresent(Int.self, forKey: .version) ?? Self.currentVersion
        guard version <= Self.currentVersion else {
            connections = []
            return
        }
        let raw =
            try container.decodeIfPresent([SavedConnection.Raw].self, forKey: .connections) ?? []
        connections = raw.map { $0.toSavedConnection() }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(version, forKey: .version)
        try container.encode(connections.map(SavedConnection.Raw.init(from:)), forKey: .connections)
    }

    private enum CodingKeys: String, CodingKey {
        case connections, version
    }
}

/// The connections somebody kept, and where the file holding them goes.
///
/// One JSON file, in the directory the setting names, holding everything except
/// the passwords. Those go to `ConnectionKeychain`, one per connection, for the
/// reason `ConnectionStorage` gives.
///
/// The older single-connection `connection.json` is not read, not renamed and not
/// deleted. There is no migration here to go looking for: this project keeps no
/// compatibility paths, and it is one connection to type again.
///
/// Exactly one copy of the file exists. Choosing "on this Mac" deletes the one in
/// iCloud Drive and choosing iCloud deletes the local one, because the answer to
/// "where is this kept" is also a statement about where it is *not*, and a stale
/// copy naming a host and a user would outlive the decision to stop keeping it
/// there.
enum ConnectionStore {
    private static let folder = "dbclient"
    private static let file = "connections.json"

    static func load(
        from storage: ConnectionStorage, in directories: ConnectionDirectories = .system
    ) -> [SavedConnection] {
        if storage == .iCloud, let cloud = url(for: .iCloud, in: directories),
            let document = read(cloud)
        {
            return document.connections
        }
        // The local file, and not only as the answer for "on this Mac": a Mac
        // whose iCloud Drive is off wrote its fields here while the setting still
        // said iCloud, and reading nothing would ask that user for connections
        // this file already describes.
        guard let local = url(for: .thisMac, in: directories) else { return [] }
        return read(local)?.connections ?? []
    }

    /// Writes the list to wherever the setting says.
    ///
    /// The whole list every time rather than the entry that changed: the file is
    /// small, and a partial write is how two copies of it start disagreeing about
    /// what is in the list.
    ///
    /// No password passes through here. Each one is written by whoever changed it,
    /// under its own connection's identity — a list handed a password would have to
    /// be told which entry it belonged to, and that is one more thing to get wrong
    /// in the one place where getting it wrong overwrites somebody else's.
    ///
    /// An iCloud that is not there falls through to the local file rather than
    /// dropping the write: the choice between "kept here" and "lost entirely" is
    /// not close. The Settings window says which of those happened, where the
    /// setting is, which is the only place the answer is of any use.
    static func save(
        _ connections: [SavedConnection], to storage: ConnectionStorage,
        in directories: ConnectionDirectories = .system
    ) {
        let destination = directories.directory(for: storage) ?? directories.local
        guard let target = url(in: destination) else { return }
        write(SavedConnections(connections: connections), to: target)
        for other in [directories.local, directories.cloud].compactMap({ $0 })
        where other != destination {
            if let stale = url(in: other) { try? FileManager.default.removeItem(at: stale) }
        }
    }

    /// What is not going to happen, for the answer the user picked, or nil when
    /// both halves of it work.
    ///
    /// Two separate limitations, and they are not the same size. Without iCloud
    /// Drive nothing syncs at all; with it, the fields travel and only the password
    /// stays behind. A control that silently did something other than what it says
    /// is the failure this exists to prevent, so the sentence names which one it
    /// is.
    static func syncCaveat(in directories: ConnectionDirectories = .system) -> String? {
        guard directories.cloud != nil else {
            return
                "iCloud Drive is not set up on this Mac, so there is nowhere to sync to and the "
                + "connection is being kept in \(directories.local.path)/\(folder) instead."
        }
        return ConnectionKeychain.synchronisedRefusal()
    }

    private static func url(for storage: ConnectionStorage, in directories: ConnectionDirectories)
        -> URL?
    {
        directories.directory(for: storage).flatMap(url(in:))
    }

    private static func url(in directory: URL) -> URL? {
        directory.appending(path: folder).appending(path: file)
    }

    private static func read(_ url: URL) -> SavedConnections? {
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(SavedConnections.self, from: data)
    }

    /// Written whole, sorted, indented and newline-terminated, and readable only
    /// by its owner.
    ///
    /// Formatted for a person because a file under `~/.config` is one somebody is
    /// going to open, diff and put in a dotfiles repository. Written atomically
    /// because a crash halfway through must not leave the next launch parsing half
    /// a JSON object and asking for a connection it already has. And 0600 even
    /// though it holds no secret: it names a host, a database and a user, which is
    /// a list of what to attack and who to attack it as, and restricting it costs
    /// nothing.
    private static func write(_ document: SavedConnections, to url: URL) {
        let manager = FileManager.default
        try? manager.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard var data = try? encoder.encode(document) else { return }
        data.append(0x0A)
        try? data.write(to: url, options: [.atomic])
        // After the write: an atomic write is a rename over the target, so
        // permissions set before it belong to a file that is already gone.
        try? manager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }
}

/// The password for a saved connection.
///
/// The Keychain rather than UserDefaults, which is the entire reason this file
/// exists: a plist beside the other settings is readable by anything running as
/// the user, and a database password handed out that cheaply is not a password.
///
/// Measured under this project's ad-hoc signature, which `make package` renews
/// on every build: a write always succeeds, but a read from a build whose code
/// signature has changed since the item was written comes back
/// `errSecUserCanceled` — macOS is asking the user to let the new binary at an
/// item the old one owned, and a background queue gets the refusal. So a failed
/// read is treated as "nothing saved" rather than as an error: the form comes
/// up, the password is typed once, and the write below re-anchors the item to
/// the signature now running. A stably signed build never sees it.
enum ConnectionKeychain {
    private static let service = "dev.dbclient.connection"

    /// `synchronised` picks which item is being asked about, and it has to be
    /// stated: a Keychain query with no opinion matches local items only, so the
    /// synchronised one is invisible unless it is asked for by name.
    static func password(for id: UUID, synchronised: Bool = false) -> String? {
        var query = item(for: id, synchronised: synchronised)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var found: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &found) == errSecSuccess,
            let data = found as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Deletes and adds rather than updating in place. `SecItemUpdate` needs
    /// access to the existing item and so hits the signature problem above;
    /// deleting one does not, and the add that follows leaves the item readable
    /// by the build that wrote it.
    ///
    /// A failed local write is not reported: it costs the convenience of not
    /// typing the password again, and there is nothing the user could do about it
    /// from the form that they are not already doing. The answer is returned all
    /// the same, because a *synchronised* write that fails is a different matter —
    /// `ConnectionStore.save` has to fall through to the local item, and this is
    /// the only side that knows the difference between "macOS will not sync for
    /// this build" and "written".
    @discardableResult
    static func save(
        _ password: String, for id: UUID, synchronised: Bool = false
    ) -> Bool {
        let query = item(for: id, synchronised: synchronised)
        _ = SecItemDelete(query as CFDictionary)
        // An empty password is a trust or peer connection. Storing nothing for
        // it is not a gap: there is nothing to remember.
        guard !password.isEmpty else { return true }
        var add = query
        add[kSecValueData as String] = Data(password.utf8)
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    /// Forgets a connection's password, both copies of it.
    ///
    /// Called when a connection is deleted from the list. A password left behind
    /// for an entry nobody can see any more is a secret with no owner: nothing in
    /// the application will ever show it, offer to change it, or delete it, and it
    /// outlives the decision to stop keeping the connection at all.
    ///
    /// Both items, because the synchronised one is invisible to a query that does
    /// not ask for it by name — deleting only the local copy would leave the one
    /// that can travel.
    static func delete(for id: UUID) {
        _ = SecItemDelete(item(for: id, synchronised: false) as CFDictionary)
        _ = SecItemDelete(item(for: id, synchronised: true) as CFDictionary)
    }

    // MARK: - The half that can leave the machine

    /// Why the password cannot travel even though the fields can, or nil when it
    /// can.
    ///
    /// Asked by writing a throwaway item, because there is no API that answers
    /// it. Measured under this project's ad-hoc signature: an add carrying
    /// `kSecAttrSynchronizable` comes back `errSecMissingEntitlement` (-34018)
    /// while the same add without the flag succeeds — so the refusal arrives as a
    /// code rather than as a silent no-op, and the Settings window can say what
    /// is happening instead of leaving the user to discover it on another Mac.
    ///
    /// This is only about the password. The fields go through iCloud Drive, which
    /// an application that is not sandboxed writes to as an ordinary folder and
    /// needs no entitlement for — which is why they sync from this build and the
    /// password does not.
    static func synchronisedRefusal() -> String? {
        var probe = syncedItem()
        probe[kSecAttrAccount as String] = "icloud-probe"
        _ = SecItemDelete(probe as CFDictionary)
        var add = probe
        add[kSecValueData as String] = Data("probe".utf8)
        let status = SecItemAdd(add as CFDictionary, nil)
        _ = SecItemDelete(probe as CFDictionary)
        switch status {
        case errSecSuccess, errSecDuplicateItem:
            return nil
        case errSecMissingEntitlement:
            return
                "The fields sync through iCloud Drive, but the password cannot: macOS gives "
                + "synchronised Keychain items only to an application signed with a Developer ID "
                + "and the entitlement for them, and this one is signed ad-hoc. Another Mac opens "
                + "the same database and asks for the password once."
        default:
            return
                "The fields sync through iCloud Drive, but macOS refused a synchronised Keychain "
                + "item for the password (OSStatus \(status)). Another Mac asks for it once."
        }
    }

    /// The probe's shape: any synchronised item of this service will do, so it
    /// borrows the real item's dictionary and overrides the account.
    private static func syncedItem() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrSynchronizable as String: true
        ]
    }

    /// One item per saved connection, named by the connection's own id.
    ///
    /// The id rather than `scheme://user@host:port/database`, which is what this
    /// keyed on while there was only ever one connection to key. With a list, the
    /// entry is what owns the password: correcting a host in a saved connection
    /// must not orphan its password, and two entries that happen to name the same
    /// server and user are two connections a person is keeping apart on purpose —
    /// under an identity spelled out of the fields they would share one password
    /// and overwrite each other's.
    ///
    /// The synchronised item is the same identity with the flag set. The id arrives
    /// on the other Mac in the file, so that machine knows which item to ask for,
    /// and the two copies of a password cannot come to describe different
    /// connections.
    private static func item(for id: UUID, synchronised: Bool) -> [String: Any] {
        var item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: id.uuidString
        ]
        if synchronised { item[kSecAttrSynchronizable as String] = true }
        return item
    }
}
