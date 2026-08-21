import Foundation

/// What the connection now open is allowed to do.
///
/// A value of its own rather than two flags read off the model, because the two
/// questions are asked from four places between them — the grid, the value pane,
/// the import target, the editor's Run — and a refusal worded differently in each
/// of them would read as four separate faults.
///
/// Held as it was at the moment of connecting and never read back out of the
/// form: somebody who unticks Read-only while a session is open has changed what
/// the *next* connection will be, and a window that unlocked underneath them
/// would be enforcing a mark it had already stopped showing.
struct ConnectionSafety {
    var isReadOnly: Bool
    var isProduction: Bool

    init(isReadOnly: Bool = false, isProduction: Bool = false) {
        self.isReadOnly = isReadOnly
        self.isProduction = isProduction
    }

    /// The marks of a saved connection, as a window will carry them.
    init(of connection: SavedConnection) {
        self.init(isReadOnly: connection.isReadOnly, isProduction: connection.isProduction)
    }

    /// Why a write this application controls is refused, or nil where it is not.
    ///
    /// One sentence for all of them — grid edits, generated DDL, being an import
    /// target — because they are refused for one reason, and what a person reads
    /// is the reason rather than the path. It names the mark and where to change
    /// it: a control that has silently stopped working is the failure this is
    /// written to avoid, and "nothing happens when I press Save" is how that
    /// failure is reported.
    ///
    /// Production is not consulted here. It is the other question — see
    /// `SavedConnection.isProduction` — and a mark that refused would make the
    /// connection useless rather than careful.
    var writeRefusal: String? {
        guard isReadOnly else { return nil }
        return
            "This connection is marked read-only. Clear the mark in the connection form to write to it."
    }

    /// Whether a statement this dangerous is worth stopping to ask about.
    ///
    /// Reads never are, on any connection. A question in front of every SELECT
    /// is a question nobody reads by the third one, and a mark whose dialog is
    /// dismissed reflexively protects nothing — it only teaches the reflex that
    /// will dismiss the one that mattered.
    ///
    /// Read-only is not consulted, and the asymmetry is the point: it has
    /// already refused the writes this application controls, and the ones it has
    /// not refused are statements somebody typed themselves. Marking a
    /// connection read-only is not a claim that its user cannot write SQL.
    func asks(about danger: SQLScript.Danger) -> Bool {
        isProduction && danger >= .modify
    }
}

/// A connection that a person kept, with a name and color.
///
/// The list of these is what the file holds, not a single record. Each has an
/// identity of its own so that renaming a host does not lose its password.
struct SavedConnection: Identifiable, Equatable, Codable {
    var id: UUID
    var name: String  // what the user called it; "" means untitled
    var color: ConnectionColor

    /// Whether this connection refuses the writes this application is in charge
    /// of.
    ///
    /// Grid edits, generated DDL and being an import target — the three places
    /// this application decides on its own to write something. Deliberately not
    /// the editor: a client that promised read-only and then let a `DELETE`
    /// through because it could not parse the statement would be worse than one
    /// that promised nothing, and parsing arbitrary SQL to find out is a promise
    /// this cannot keep. What the editor gets is `isProduction`, which asks
    /// instead of refusing.
    var isReadOnly: Bool

    /// Whether a write here is worth stopping to confirm.
    ///
    /// A different question from `isReadOnly`, which is why both exist: one is
    /// about what this connection is for, the other about what it costs to be
    /// wrong. A staging database can be read-only for an afternoon; the
    /// production one is never safe to write to by accident, and the answer to
    /// that is a question in front of the statement rather than a refusal that
    /// would make the connection useless.
    var isProduction: Bool

    /// What answered, the last time this connection was opened or tested.
    ///
    /// A record and not a setting: nobody types it, nothing compares it when
    /// deciding whether the form has unsaved edits, and a connection nothing has
    /// ever answered has none. It is kept because it is the only thing that tells
    /// two rows apart when both say `postgres://` and one of them is CockroachDB
    /// — the scheme names a wire protocol, and the product is what somebody
    /// actually has open.
    ///
    /// One string, as it was shown: "PostgreSQL 17.0". Kept as a product and a
    /// version it would be a structure nothing reads as one, in a file somebody
    /// opens and reads.
    var server: String

    var settings: ConnectionSettings

    init(
        id: UUID = UUID(), name: String = "", color: ConnectionColor = .none,
        isReadOnly: Bool = false, isProduction: Bool = false, server: String = "",
        settings: ConnectionSettings
    ) {
        self.id = id
        self.name = name
        self.color = color
        self.isReadOnly = isReadOnly
        self.isProduction = isProduction
        self.server = server
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
    /// What answered comes first and the address last, because this line truncates
    /// in its middle: the product and the database are the two ends somebody reads
    /// a row by, and the port between them is the part that can afford to go. A
    /// connection nothing has answered yet is the address alone, rather than a
    /// separator with nothing in front of it.
    var subtitle: String {
        server.isEmpty ? address : "\(server) · \(address)"
    }

    /// Where this connection points, as a label.
    ///
    /// Built here rather than by asking `connectionString` for a URL, because a URL
    /// would carry the scheme and the percent-encoding and this is a label. Each
    /// separator belongs to the part after it, so a connection with no port reads
    /// `ana@db.example/sales` rather than `ana@db.example:/sales` — punctuation with
    /// nothing behind it looks like the line was cut off.
    private var address: String {
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
        /// Written even when false, unlike the fields somebody might not have
        /// typed. This file is one a person opens and edits, and a flag that
        /// appeared only once it had been switched on would be a setting nobody
        /// could discover from the file itself.
        var production: Bool
        var readOnly: Bool
        var scheme: String
        /// What answered, the last time. Written even when empty, for the reason
        /// the flags above it are: a key that appears only sometimes is one
        /// nobody reading the file can rely on being told about.
        var server: String
        /// libpq's word for how much of the server's identity to insist on, and
        /// the CA file to insist on it with. Kept as the word rather than as a
        /// number, because this file is one somebody opens and reads, and
        /// `"verify-ca"` says what `3` does not.
        var sslMode: String
        var sslRootCert: String
        var user: String

        init(
            color: String, database: String, host: String, id: String, name: String, path: String,
            port: String, production: Bool = false, readOnly: Bool = false, scheme: String,
            server: String = "", sslMode: String = "prefer", sslRootCert: String = "",
            user: String
        ) {
            self.color = color
            self.database = database
            self.host = host
            self.id = id
            self.name = name
            self.path = path
            self.port = port
            self.production = production
            self.readOnly = readOnly
            self.scheme = scheme
            self.server = server
            self.sslMode = sslMode
            self.sslRootCert = sslRootCert
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
            self.production = connection.isProduction
            self.readOnly = connection.isReadOnly
            self.scheme = connection.settings.scheme
            self.server = connection.server
            self.sslMode = connection.settings.sslMode.rawValue
            self.sslRootCert = connection.settings.sslRootCert
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

            // Absent means neither, which is what an entry written before these
            // existed meant. Safe in that direction and only that direction: a
            // missing flag read as "production" would put a confirmation in front
            // of every statement on every connection in somebody's file.
            self.production = try container.decodeIfPresent(Bool.self, forKey: .production) ?? false
            self.readOnly = try container.decodeIfPresent(Bool.self, forKey: .readOnly) ?? false

            // Nothing, for an entry nothing has answered — including every entry
            // written before this was recorded at all. The next connection fills
            // it in, and until then the row says only where it points.
            self.server = try container.decodeIfPresent(String.self, forKey: .server) ?? ""

            // `prefer` for an entry written before this key existed, which is
            // what that entry has been connecting with all along: it is the
            // driver's own default, so reading it this way changes nothing about
            // a connection somebody saved and has been using.
            self.sslMode = try container.decodeIfPresent(String.self, forKey: .sslMode) ?? "prefer"
            self.sslRootCert =
                try container.decodeIfPresent(String.self, forKey: .sslRootCert) ?? ""

            // An entry somebody typed has no id, and one is minted here rather than
            // at the call site: an entry that arrived without an identity still has
            // to have one before anything can keep its password.
            let idString = try container.decodeIfPresent(String.self, forKey: .id) ?? ""
            self.id = idString.isEmpty ? UUID().uuidString : idString
        }

        private enum CodingKeys: String, CodingKey {
            case color, database, host, id, name, path, port, production, readOnly, scheme, server,
                sslMode, sslRootCert, user
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
                path: self.path,
                // A word this build does not have is read as the default rather
                // than throwing: one throw anywhere empties the whole list, and
                // losing every saved connection over one unrecognised setting is
                // a worse answer than connecting the way libpq would.
                sslMode: SslMode(rawValue: self.sslMode) ?? .prefer,
                sslRootCert: self.sslRootCert
            )
            return SavedConnection(
                id: id, name: self.name, color: color, isReadOnly: self.readOnly,
                isProduction: self.production, server: self.server, settings: settings)
        }
    }
}

/// The colour a connection can be marked with.
///
/// `none` is a case rather than an optional because it is a choice made in the same
/// row of swatches as the others, and the way back after picking one by mistake.
enum ConnectionColor: String, Codable, CaseIterable, Identifiable, Sendable {
    case none, red, orange, yellow, green, blue, purple, grey

    var id: String { rawValue }

    /// What it is drawn in, or nil for the one that is not drawn at all.
    ///
    /// The tones live in `Theme` rather than here, for the reason that file's own
    /// comment gives: one source of truth for colour, read by both rendering stacks.
    var tone: Theme.Tone? {
        switch self {
        case .none: return nil
        case .red: return Theme.Connection.red
        case .orange: return Theme.Connection.orange
        case .yellow: return Theme.Connection.yellow
        case .green: return Theme.Connection.green
        case .blue: return Theme.Connection.blue
        case .purple: return Theme.Connection.purple
        case .grey: return Theme.Connection.grey
        }
    }

    /// What a screen reader calls the swatch. "No colour" rather than "none", which
    /// on its own is read as the answer to a question nobody heard.
    var label: String {
        self == .none ? "No colour" : rawValue.capitalized
    }
}

/// What somebody decided about edits that are about to be left behind.
///
/// Three answers, because two would make one of them a lie: a window that offered
/// only "discard" and "cancel" would be asking somebody to choose between losing
/// their work and being stuck on the row they are trying to leave.
enum UnsavedConnectionChoice {
    case save
    case discard
    case cancel
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

        if self.isReadOnly != draft.isReadOnly {
            changedFields.append("Read-only")
        }

        if self.isProduction != draft.isProduction {
            changedFields.append("Production")
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

        if self.settings.sslMode != draft.settings.sslMode {
            changedFields.append("SSL")
        }

        if self.settings.sslRootCert != draft.settings.sslRootCert {
            changedFields.append("CA")
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
