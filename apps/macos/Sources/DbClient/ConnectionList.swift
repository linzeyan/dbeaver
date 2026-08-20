import Foundation

/// The saved connections, as the window works with them.
///
/// A value with the decisions on it rather than an array the view reaches into,
/// for the reason `StagedChanges` is one: which connection a filter matches, where
/// an edited one lands, and what a removal hands back are each answerable with no
/// window on screen, and each of them can be wrong in a way that compiles and looks
/// right until somebody loses a connection.
struct ConnectionList: Equatable {
    var connections: [SavedConnection]

    init(_ connections: [SavedConnection] = []) {
        self.connections = connections
    }

    /// The connections a filter leaves on screen.
    ///
    /// Title and subtitle rather than the name field, because most rows are named by
    /// what they open rather than by anything anybody typed — a filter that searched
    /// only names would find nothing in a list where nothing has been named, which is
    /// what a list looks like until somebody starts naming things.
    ///
    /// Whitespace is not a filter. A field holding a stray space would otherwise
    /// empty the sidebar and leave no visible cause.
    func matching(_ filter: String) -> [SavedConnection] {
        guard !filter.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return connections
        }

        let lowerFilter = filter.lowercased()
        return connections.filter { connection in
            connection.title.lowercased().contains(lowerFilter)
                || connection.subtitle.lowercased().contains(lowerFilter)
        }
    }

    /// Where a connection is, or nil when it is not in the list.
    func index(of id: UUID) -> Int? {
        connections.firstIndex { $0.id == id }
    }

    /// The connection with this id, or nil.
    func connection(_ id: UUID) -> SavedConnection? {
        connections.first { $0.id == id }
    }

    /// Puts an edited connection back where it was, or adds one that is new.
    ///
    /// In place, and that is the whole point of the method: the sidebar is drawn in
    /// the order this array holds, so a save that removed and appended would move the
    /// row somebody had just been editing out from under their pointer, every time
    /// they pressed Save.
    mutating func save(_ connection: SavedConnection) {
        if let index = index(of: connection.id) {
            connections[index] = connection
        } else {
            connections.append(connection)
        }
    }

    /// Takes a connection out and hands it back.
    ///
    /// Returned rather than discarded because the caller has one more thing to do
    /// with it: a deleted connection's password has to be forgotten too, and the id
    /// is the only way to find it. Nil when there was nothing there, so that a second
    /// press of Delete is not a second question about a connection that is gone.
    @discardableResult
    mutating func remove(_ id: UUID) -> SavedConnection? {
        guard let index = index(of: id) else { return nil }
        return connections.remove(at: index)
    }

    /// Whether a draft holds anything a person put there.
    ///
    /// Save reads this, and it cannot be asked field by field: the form is never
    /// empty. It opens on the driver's own suggestion — a loopback host and that
    /// driver's default port — so a check for "any field filled in" would call an
    /// untouched form a connection worth keeping, and every launch would offer to
    /// save one.
    ///
    /// Asked against the suggestion instead, which is the only reading under which an
    /// untouched form is untouched. The colour counts: picking one is something
    /// somebody did, even before they typed a host.
    static func isWorthSaving(_ connection: SavedConnection) -> Bool {
        // The two flags count for the reason the colour does: switching one on is
        // something somebody did, and a form holding a decision about safety is
        // the last one to discard as untouched.
        guard connection.name.isEmpty, connection.color == .none, !connection.isReadOnly,
            !connection.isProduction
        else { return true }
        // A scheme this build has no driver for arrived from a file somebody edited
        // or from `--conn`, and there is no suggestion to compare it with. Anything
        // at all in it was typed by somebody, so the empty settings are the baseline.
        guard let driver = connection.settings.driver else {
            return connection.settings != ConnectionSettings(scheme: connection.settings.scheme)
        }
        return connection.settings != ConnectionSettings.suggested(for: driver)
    }
}
