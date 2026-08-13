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
    /// a form missing the database produces a libpq error about a field the
    /// user can see is empty, which is a worse answer than a disabled button.
    var isComplete: Bool {
        ![host, port, database, user].contains {
            $0.trimmingCharacters(in: .whitespaces).isEmpty
        }
    }

    /// The libpq string these settings and that password describe.
    ///
    /// The four fields are trimmed; the password is not. A hostname pasted with
    /// a trailing space is the commonest way to spend five minutes on a
    /// connection error, while whitespace inside a password is a character the
    /// server is going to check.
    func connectionString(password: String) -> String {
        func trimmed(_ value: String) -> String {
            value.trimmingCharacters(in: .whitespaces)
        }
        return ConnectionString.build([
            ("host", trimmed(host)), ("port", trimmed(port)),
            ("dbname", trimmed(database)), ("user", trimmed(user)),
            ("password", password)
        ])
    }

    /// The form's values for a string this did not write.
    ///
    /// `--conn` comes from a person or a Makefile, and one that does not connect
    /// has to land back in the form for them to correct. Retyping five fields to
    /// fix one character is the difference between a client and a demonstration.
    init(connectionString text: String) {
        let pairs = ConnectionString.parse(text)
        host = pairs["host"] ?? ""
        port = pairs["port"] ?? ""
        database = pairs["dbname"] ?? ""
        user = pairs["user"] ?? ""
    }
}

/// A libpq connection string, in its `keyword=value` form.
///
/// Written and read in one place because the two directions have to agree: a
/// password containing a space has to be quoted on the way out, and a `--conn`
/// string that failed has to be read back into the form on the way in. Split
/// across two call sites they would drift, and the symptom would be a password
/// that silently connects as half of itself.
enum ConnectionString {
    static func build(_ pairs: [(key: String, value: String)]) -> String {
        pairs
            .filter { !$0.value.isEmpty }
            .map { "\($0.key)=\(escaped($0.value))" }
            .joined(separator: " ")
    }

    /// The keywords in `text`. A repeated keyword takes its last value, which is
    /// what libpq itself does.
    static func parse(_ text: String) -> [String: String] {
        var pairs: [String: String] = [:]
        let chars = Array(text)
        var i = 0

        func skipSpaces() {
            while i < chars.count, chars[i].isWhitespace { i += 1 }
        }

        while true {
            skipSpaces()
            guard i < chars.count else { break }

            var key = ""
            while i < chars.count, chars[i] != "=", !chars[i].isWhitespace {
                key.append(chars[i])
                i += 1
            }
            skipSpaces()
            // A keyword with no `=` after it is not a pair, and guessing at what
            // was meant would put a hostname in the password field.
            guard i < chars.count, chars[i] == "=" else { break }
            i += 1
            skipSpaces()

            var value = ""
            if i < chars.count, chars[i] == "'" {
                i += 1
                while i < chars.count, chars[i] != "'" {
                    if chars[i] == "\\", i + 1 < chars.count { i += 1 }
                    value.append(chars[i])
                    i += 1
                }
                if i < chars.count { i += 1 }
            } else {
                while i < chars.count, !chars[i].isWhitespace {
                    if chars[i] == "\\", i + 1 < chars.count { i += 1 }
                    value.append(chars[i])
                    i += 1
                }
            }
            if !key.isEmpty { pairs[key] = value }
        }
        return pairs
    }

    /// libpq needs single quotes around a value holding whitespace or a quote,
    /// and a backslash before a quote or a backslash inside them. Applied only
    /// where it is needed, so that a `--conn` argument a human has to type stays
    /// readable.
    private static func escaped(_ value: String) -> String {
        let plain = value.allSatisfy { !$0.isWhitespace && $0 != "'" && $0 != "\\" }
        guard !plain else { return value }
        var out = "'"
        for character in value {
            if character == "'" || character == "\\" { out.append("\\") }
            out.append(character)
        }
        return out + "'"
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
