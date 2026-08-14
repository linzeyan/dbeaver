import Foundation
import Security

/// Where to connect, as the connection form collects it.
///
/// Deliberately holds no password. This is the value that gets written to
/// UserDefaults, and a type that cannot carry a secret cannot leak one into a
/// plist through a later edit; the password travels beside it and lives in the
/// Keychain. `connectionString(password:)` is the one place the two meet.
struct ConnectionSettings: Equatable {
    var host: String
    var port: String
    var database: String
    var user: String

    init(host: String, port: String, database: String, user: String) {
        self.host = host
        self.port = port
        self.database = database
        self.user = user
    }

    /// What an empty form offers. A loopback host and the standard port are the
    /// two fields with a defensible default; a guessed database or user name
    /// would only be a value to delete.
    static let suggested = ConnectionSettings(
        host: "127.0.0.1", port: "5432", database: "", user: "")

    /// Whether these name a database worth trying. The Connect button reads it:
    /// a form missing the database produces a server error about a field the
    /// user can see is empty, which is a worse answer than a disabled button.
    var isComplete: Bool {
        ![host, port, database, user].contains {
            $0.trimmingCharacters(in: .whitespaces).isEmpty
        }
    }

    /// The connection URL these settings and that password describe.
    ///
    /// A URL rather than the `keyword=value` string libpq takes, because the
    /// core now has more than one database behind it and the scheme is how it
    /// knows which: `postgres://` reaches PostgreSQL, `sqlite://` reaches a
    /// file. A bare `host=… port=…` names no driver, and a client that guessed
    /// between them would be one that connects to the wrong database without
    /// saying so.
    ///
    /// The four fields are trimmed; the password is not. A hostname pasted with
    /// a trailing space is the commonest way to spend five minutes on a
    /// connection error, while whitespace inside a password is a character the
    /// server is going to check.
    func connectionString(password: String) -> String {
        func trimmed(_ value: String) -> String {
            value.trimmingCharacters(in: .whitespaces)
        }
        var url = URLComponents()
        url.scheme = "postgres"
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
    init(connectionString text: String) {
        let url = URLComponents(string: text)
        host = url?.host ?? ""
        port = url?.port.map(String.init) ?? ""
        database = url?.path.hasPrefix("/") == true ? String(url!.path.dropFirst()) : ""
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

/// The last connection that worked, remembered so the next launch does not ask
/// again.
///
/// UserDefaults holds everything except the password: it is a plist in the
/// user's Library that anything running as them can read, which is exactly the
/// place a database password must not be. That half goes to
/// `ConnectionKeychain`.
enum ConnectionStore {
    private static let key = "lastConnection"

    static func load() -> ConnectionSettings? {
        guard let stored = UserDefaults.standard.dictionary(forKey: key) as? [String: String],
            let host = stored["host"], let port = stored["port"],
            let database = stored["database"], let user = stored["user"]
        else { return nil }
        return ConnectionSettings(host: host, port: port, database: database, user: user)
    }

    static func save(_ settings: ConnectionSettings) {
        UserDefaults.standard.set(
            [
                "host": settings.host, "port": settings.port,
                "database": settings.database, "user": settings.user
            ], forKey: key)
    }

    /// The connection to open without asking, if there is one. The password is
    /// empty rather than absent when the Keychain will not give it up — see
    /// `ConnectionKeychain` for when that happens and why it is survivable.
    static func remembered() -> (settings: ConnectionSettings, password: String)? {
        guard let settings = load() else { return nil }
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

    /// One item per user, host, port and database, so that keeping two
    /// databases on one server does not have each login overwrite the other's
    /// password.
    private static func item(for settings: ConnectionSettings) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String:
                "\(settings.user)@\(settings.host):\(settings.port)/\(settings.database)"
        ]
    }
}
