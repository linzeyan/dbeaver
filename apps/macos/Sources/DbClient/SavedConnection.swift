import Foundation

/// A connection that a person kept, with a name and color.
///
/// The list of these is what the file holds, not a single record. Each has an
/// identity of its own so that renaming a host does not lose its password.
struct SavedConnection: Identifiable, Equatable, Codable {
    var id: UUID
    var name: String  // what the user called it; "" means untitled
    var color: ConnectionColor
    var settings: ConnectionSettings

    init(
        id: UUID = UUID(), name: String = "", color: ConnectionColor = .none,
        settings: ConnectionSettings
    ) {
        self.id = id
        self.name = name
        self.color = color
        self.settings = settings
    }

    /// What a list row calls this connection.
    ///
    /// A name the user typed wins, because two connections to the same server
    /// under the same account are told apart by nothing else. Without one the row
    /// is named after what it opens — `database@host`, the shape the session tab
    /// already uses, so a row and the tab it becomes read the same. "Untitled" is
    /// the last resort rather than a blank row: a row with no text is one nobody
    /// can click with any confidence.
    var title: String {
        if !name.isEmpty {
            return name
        }

        // A file database is named by its file: there is no host to name it after,
        // and the path in full is the subtitle's job.
        if settings.driver?.shape == .file {
            return settings.path.split(separator: "/").last.map(String.init) ?? "Untitled"
        }

        // For a server, use database@host or host alone
        if !settings.database.isEmpty && !settings.host.isEmpty {
            return "\(settings.database)@\(settings.host)"
        } else if !settings.host.isEmpty {
            return settings.host
        } else {
            return "Untitled"
        }
    }

    /// The line under the heading: enough of the connection to tell two rows apart.
    ///
    /// Built here rather than by asking `connectionString` for a URL, because a URL
    /// would carry the scheme and the percent-encoding and this is a label. Each
    /// separator belongs to the part after it, so a connection with no port reads
    /// `ana@db.example/sales` rather than `ana@db.example:/sales` — punctuation with
    /// nothing behind it looks like the line was cut off.
    var subtitle: String {
        if settings.driver?.shape == .file {
            return settings.path
        }

        var result = ""

        if !settings.user.isEmpty {
            result += settings.user
        }

        if !settings.host.isEmpty {
            if !result.isEmpty {
                result += "@"
            }
            result += settings.host
        }

        if !settings.port.isEmpty {
            if !result.isEmpty {
                result += ":"
            }
            result += settings.port
        }

        if !settings.database.isEmpty {
            if !result.isEmpty {
                result += "/"
            }
            result += settings.database
        }

        return result
    }

    /// This connection as the file writes it: one flat object.
    ///
    /// Flat because the file is one a developer opens, diffs and keeps in their
    /// dotfiles, and a nested `settings` object is the program's shape rather than
    /// the reader's. A separate type rather than `CodingKeys` on the connection
    /// itself, so that what the file looks like can change without the rest of the
    /// application hearing about it.
    struct Raw: Codable {
        var color: String
        var database: String
        var host: String
        var id: String
        var name: String
        var path: String
        var port: String
        var scheme: String
        var user: String

        init(
            color: String, database: String, host: String, id: String, name: String, path: String,
            port: String, scheme: String, user: String
        ) {
            self.color = color
            self.database = database
            self.host = host
            self.id = id
            self.name = name
            self.path = path
            self.port = port
            self.scheme = scheme
            self.user = user
        }

        init(from connection: SavedConnection) {
            self.color = connection.color.rawValue
            self.database = connection.settings.database
            self.host = connection.settings.host
            self.id = connection.id.uuidString
            self.name = connection.name
            self.path = connection.settings.path
            self.port = connection.settings.port
            self.scheme = connection.settings.scheme
            self.user = connection.settings.user
        }

        /// Everything the user might not have typed is optional.
        ///
        /// This file is meant to be edited by hand, and a synthesized decoder throws
        /// on a missing key — a throw anywhere in the array fails the whole document,
        /// so one forgotten `"color"` would empty somebody's list of connections. The
        /// five that stay required are the ones an entry means nothing without.
        init(from decoder: Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)

            self.scheme = try container.decode(String.self, forKey: .scheme)
            self.host = try container.decode(String.self, forKey: .host)
            self.port = try container.decode(String.self, forKey: .port)
            self.database = try container.decode(String.self, forKey: .database)
            self.user = try container.decode(String.self, forKey: .user)

            self.name = try container.decodeIfPresent(String.self, forKey: .name) ?? ""
            self.color = try container.decodeIfPresent(String.self, forKey: .color) ?? "none"
            self.path = try container.decodeIfPresent(String.self, forKey: .path) ?? ""

            // An entry somebody typed has no id, and one is minted here rather than
            // at the call site: an entry that arrived without an identity still has
            // to have one before anything can keep its password.
            let idString = try container.decodeIfPresent(String.self, forKey: .id) ?? ""
            self.id = idString.isEmpty ? UUID().uuidString : idString
        }

        private enum CodingKeys: String, CodingKey {
            case color, database, host, id, name, path, port, scheme, user
        }

        func toSavedConnection() -> SavedConnection {
            let id = UUID(uuidString: self.id) ?? UUID()
            let color = ConnectionColor(rawValue: self.color) ?? .none
            let settings = ConnectionSettings(
                scheme: self.scheme,
                host: self.host,
                port: self.port,
                database: self.database,
                user: self.user,
                path: self.path
            )
            return SavedConnection(id: id, name: self.name, color: color, settings: settings)
        }
    }
}

/// The color a connection can be given.
///
/// A value rather than a function, so the checks can name the cases.
enum ConnectionColor: String, Codable, CaseIterable, Identifiable, Sendable {
    case none, red, orange, yellow, green, blue, purple, grey
    var id: String { rawValue }
}

/// What editing a saved connection has changed and not written back.
///
/// It carries the sentences rather than only the flags, for the reason `UnsavedWork`
/// does: they are read once, by somebody deciding whether to lose what they typed,
/// and a dialog that asks "Are you sure?" gives them nothing to decide with.
struct UnsavedConnectionEdits: Equatable {
    /// The connection being edited, by whatever the list calls it.
    let title: String
    /// The fields that differ, in the order the form shows them, so that the
    /// sentence walks down the form rather than in whatever order the comparison
    /// happened to run.
    let fields: [String]
    var isEmpty: Bool { fields.isEmpty }
    /// What the dialog asks.
    let question: String
    /// What it says would happen, named field by field — "Host, Port and Password
    /// would go back to what was saved" is a sentence somebody can act on, and
    /// "Discard changes?" is not.
    let detail: String

    init(title: String, fields: [String]) {
        self.title = title
        self.fields = fields
        self.question = "Discard changes to \(title)?"
        self.detail = "\(Self.english(fields)) would go back to what was saved."
    }

    /// Formats a list of field names as "a, b and c" English.
    private static func english(_ fields: [String]) -> String {
        guard let last = fields.last, fields.count > 1 else { return fields.first ?? "" }
        return fields.dropLast().joined(separator: ", ") + " and " + last
    }
}

extension SavedConnection {
    /// What is different between this saved connection and the form's current
    /// values, or nil when nothing is.
    func unsavedEdits(against draft: SavedConnection, passwordChanged: Bool)
        -> UnsavedConnectionEdits?
    {
        var changedFields: [String] = []

        if self.name != draft.name {
            changedFields.append("Name")
        }

        if self.color != draft.color {
            changedFields.append("Colour")
        }

        if self.settings.scheme != draft.settings.scheme {
            changedFields.append("Kind")
        }

        if self.settings.host != draft.settings.host {
            changedFields.append("Host")
        }

        if self.settings.port != draft.settings.port {
            changedFields.append("Port")
        }

        if self.settings.database != draft.settings.database {
            changedFields.append("Database")
        }

        if self.settings.user != draft.settings.user {
            changedFields.append("User")
        }

        if self.settings.path != draft.settings.path {
            changedFields.append("File")
        }

        if passwordChanged {
            changedFields.append("Password")
        }

        guard !changedFields.isEmpty else {
            return nil
        }

        return UnsavedConnectionEdits(title: self.title, fields: changedFields)
    }
}
