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
/// Two answers because they differ in what they risk rather than in how much
/// typing they save. On this Mac, the fields are a plist in the user's Library
/// and the password is an item in their login Keychain, and neither leaves the
/// machine. In iCloud, both travel: a second Mac opens the same database
/// without being told about it, and the password is in the user's iCloud
/// Keychain, where it is worth as much as their Apple Account is protected.
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

/// The last connection that worked, remembered so the next launch does not ask
/// again.
///
/// Two places it can be, chosen by a setting. On this Mac, UserDefaults holds
/// everything except the password — it is a plist in the user's Library that
/// anything running as them can read, which is exactly the place a database
/// password must not be — and that half goes to `ConnectionKeychain`. In iCloud
/// it is one synchronised Keychain item holding both, because fields that stayed
/// behind would leave the other Mac with a password for a host it has never
/// heard of.
enum ConnectionStore {
    private static let key = "lastConnection"

    /// The defaults the local half lives in, injectable for the reason
    /// `Preferences`' store is: a check has to be able to write and read one back
    /// without changing what a developer's own window opens.
    static func load(from storage: ConnectionStorage, store: UserDefaults = .standard)
        -> ConnectionSettings?
    {
        if storage == .iCloud, let synced = ConnectionKeychain.syncedConnection() {
            return synced.settings
        }
        guard let stored = store.dictionary(forKey: key) as? [String: String]
        else { return nil }
        // The scheme is read with a fallback because a settings dictionary
        // written before there was more than one database has no scheme in it,
        // and the connection it describes is a PostgreSQL one.
        let scheme = stored["scheme"] ?? DriverCatalog.first?.scheme ?? ""
        return ConnectionSettings(
            scheme: scheme,
            host: stored["host"] ?? "", port: stored["port"] ?? "",
            database: stored["database"] ?? "", user: stored["user"] ?? "",
            path: stored["path"] ?? "")
    }

    /// Writes the connection and its password to wherever the setting says.
    ///
    /// One call for both halves, because there is no state in which remembering
    /// one without the other is right, and because which halves there are depends
    /// on where it goes: iCloud is a single item, this Mac is a pair.
    ///
    /// An iCloud write that macOS refuses falls through to the local pair rather
    /// than being dropped. A build without the entitlement for synchronised
    /// Keychain items — which is every ad-hoc one, `make package` included —
    /// cannot sync, and the choice between "remembered here" and "forgotten
    /// entirely" is not close. The Settings window says so where the setting is,
    /// which is the only place the answer is of any use.
    static func save(
        _ settings: ConnectionSettings, password: String, to storage: ConnectionStorage,
        store: UserDefaults = .standard
    ) {
        if storage == .iCloud, ConnectionKeychain.saveSynced(settings, password: password) {
            return
        }
        store.set(
            [
                "scheme": settings.scheme,
                "host": settings.host, "port": settings.port,
                "database": settings.database, "user": settings.user,
                "path": settings.path
            ], forKey: key)
        ConnectionKeychain.save(password, for: settings)
    }

    /// The connection to open without asking, if there is one. The password is
    /// empty rather than absent when the Keychain will not give it up — see
    /// `ConnectionKeychain` for when that happens and why it is survivable.
    static func remembered(from storage: ConnectionStorage)
        -> (settings: ConnectionSettings, password: String)?
    {
        if storage == .iCloud, let synced = ConnectionKeychain.syncedConnection() {
            return synced
        }
        guard let settings = load(from: .thisMac) else { return nil }
        return (settings, ConnectionKeychain.password(for: settings) ?? "")
    }
}

/// The password for a remembered connection.
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

    static func password(for settings: ConnectionSettings) -> String? {
        var query = item(for: settings)
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
    /// A failed write is not reported: it costs the convenience of not typing
    /// the password again, and there is nothing the user could do about it from
    /// the form that they are not already doing.
    static func save(_ password: String, for settings: ConnectionSettings) {
        let query = item(for: settings)
        _ = SecItemDelete(query as CFDictionary)
        // An empty password is a trust or peer connection. Storing nothing for
        // it is not a gap: there is nothing to remember.
        guard !password.isEmpty else { return }
        var add = query
        add[kSecValueData as String] = Data(password.utf8)
        _ = SecItemAdd(add as CFDictionary, nil)
    }

    // MARK: - The iCloud half

    /// The one item the iCloud setting keeps, holding the fields as well as the
    /// password.
    ///
    /// Not the local pair with a flag on the password: fields left in this Mac's
    /// UserDefaults would reach the other Mac as nothing at all, and a password
    /// for a host that machine has never heard of is a password for nothing. The
    /// fields are not secret, and putting them inside the item that already
    /// carries the secret does not widen what is exposed — it narrows it, since
    /// the plist copy stops being written.
    ///
    /// A fixed account rather than the identity the local items are keyed by:
    /// the question this answers is "which connection was last open", and a
    /// launch cannot look that up under a key it would have to already know.
    private static let syncedAccount = "lastConnection"

    /// The remembered connection, as it goes into and comes out of one item.
    private struct SyncedConnection: Codable {
        let settings: ConnectionSettings
        let password: String
    }

    static func syncedConnection() -> (settings: ConnectionSettings, password: String)? {
        var query = syncedItem()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var found: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &found) == errSecSuccess,
            let data = found as? Data,
            let synced = try? JSONDecoder().decode(SyncedConnection.self, from: data)
        else { return nil }
        return (synced.settings, synced.password)
    }

    /// Writes it, and says whether it landed.
    ///
    /// The answer is what `ConnectionStore.save` needs: a refused write has to
    /// fall through to the local pair, and this is the only side that knows the
    /// difference between "macOS will not sync for this build" and "written".
    /// Deletes first for the reason the local write does.
    static func saveSynced(_ settings: ConnectionSettings, password: String) -> Bool {
        let query = syncedItem()
        _ = SecItemDelete(query as CFDictionary)
        guard
            let data = try? JSONEncoder().encode(
                SyncedConnection(settings: settings, password: password))
        else { return false }
        var add = query
        add[kSecValueData as String] = data
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    /// Why this build cannot sync through the iCloud Keychain, or nil when it
    /// can.
    ///
    /// Asked by writing a throwaway item, because there is no API that answers
    /// it. Measured under this project's ad-hoc signature: an add carrying
    /// `kSecAttrSynchronizable` comes back `errSecMissingEntitlement` (-34018)
    /// while the same add without the flag succeeds — so the refusal arrives as a
    /// code rather than as a silent no-op, and the Settings window can say what
    /// is happening instead of leaving the user to discover it on another Mac.
    ///
    /// `NSUbiquitousKeyValueStore` was the other candidate for the fields and
    /// this is why it is not used: without its entitlement it accepts a write,
    /// answers false to `synchronize()`, and reads back nil. A store that
    /// swallows what it is given is worse than one that refuses it.
    static func iCloudRefusal() -> String? {
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
                "This build cannot reach the iCloud Keychain: macOS gives synchronised items "
                + "only to an application signed with a Developer ID and the entitlement for "
                + "them, and this one is signed ad-hoc. Connections are being kept on this Mac."
        default:
            return
                "macOS refused a synchronised Keychain item (OSStatus \(status)). Connections "
                + "are being kept on this Mac."
        }
    }

    private static func syncedItem() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: syncedAccount,
            kSecAttrSynchronizable as String: true
        ]
    }

    /// One item per user, host, port and database, so that keeping two
    /// databases on one server does not have each login overwrite the other's
    /// password.
    private static func item(for settings: ConnectionSettings) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            // The scheme is part of the identity because the same host and port
            // can front two databases: 127.0.0.1:5432 is PostgreSQL today and
            // could be a tunnel to something else tomorrow, and the two logins
            // must not overwrite each other's password.
            kSecAttrAccount as String:
                "\(settings.scheme)://\(settings.user)@\(settings.host):\(settings.port)"
                + "/\(settings.database)\(settings.path)"
        ]
    }
}
