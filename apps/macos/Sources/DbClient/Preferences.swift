import Foundation
import Observation

/// The settings this application has, and the one place their defaults are
/// stated.
///
/// Each of these exists because a question in the notes was answered with "make
/// it a setting", and the answer named the default alongside it. This is a list
/// of preferences rather than a framework for preferences: each new one is a
/// property here, a line in `registered`, a line in `init` and a row in the
/// window, which has stayed cheaper than the machinery that would have
/// anticipated it — the fourth and fifth arrived that way.
///
/// The defaults live in `UserDefaults`' registration domain, which is the
/// platform's own answer to the problem of a default stated twice. Nothing is
/// written to disk until somebody changes something, an unset preference reads
/// back as what `registered` says, and a read site cannot supply a fallback of
/// its own because `bool(forKey:)` never needs one — so the two cannot drift
/// apart the day one of them is corrected.
@Observable
@MainActor
final class Preferences {
    /// Whether the browse grid hides a column that is null in every row it holds.
    ///
    /// Off, so the empty column is drawn. It exists to answer the cost of the
    /// decision that every MongoDB result carries an `_extra` column: a
    /// collection whose documents all have the same shape leaves that column
    /// empty in every row, and a grid showing it spends a column of screen on a
    /// value that is never there. Off by default because hiding is a claim made
    /// from the rows fetched so far, and the rows fetched so far are not the
    /// table — see `AppModel.emptyBrowseColumns` for what that costs.
    var hidesEmptyColumns: Bool {
        didSet { store.set(hidesEmptyColumns, forKey: Key.hidesEmptyColumns) }
    }

    /// Whether Save asks before it sends the DELETEs it is carrying.
    ///
    /// On. Everything about a staged deletion already says it is coming — the
    /// row stays on screen struck through in red, the count beside Save includes
    /// it, and Revert takes it all back — so this is a second confirmation, and
    /// that is the argument against it. The argument for it is the one case
    /// those signals do not cover: somebody pressing Save for a cell they just
    /// edited, carrying rows they marked ten minutes ago out with it. Deleted
    /// rows are the only staged change with nothing to undo it afterwards, which
    /// is what tips the default.
    var confirmsDeletions: Bool {
        didSet { store.set(confirmsDeletions, forKey: Key.confirmsDeletions) }
    }

    /// Whether a new row nobody typed into is sent as a row of the table's own
    /// defaults, rather than refused here by name.
    ///
    /// Off. A draft row leaves every untouched column out of the INSERT so the
    /// schema decides it, so a row where nothing was touched is a row where the
    /// schema decides everything — which the databases here spell three ways,
    /// and one of them cannot spell at all. Refusing names the row while the
    /// user is still looking at it; sending would name it in whatever the server
    /// says back, on a database that has an answer, and nowhere at all on one
    /// that does not.
    var insertsRowOfDefaults: Bool {
        didSet { store.set(insertsRowOfDefaults, forKey: Key.insertsRowOfDefaults) }
    }

    /// Whether the sidebar keeps the system's translucency.
    ///
    /// Off, which is the opaque sidebar. `NavigationSplitView` lets the detail
    /// column's backgrounds run under the sidebar and the sidebar's vibrancy
    /// samples them, so with this on, every full-width band on the right — the
    /// Structure tab's section strip most visibly — is smeared across the object
    /// tree at its own height. That is a defect this window was shipped with and
    /// a platform signal some people would rather have than not, which is what
    /// makes it a setting; off by default because a stripe through the tree at
    /// the y of a control in a different pane reads as a rendering fault, and
    /// nothing on screen would explain it.
    var usesTranslucentSidebar: Bool {
        didSet { store.set(usesTranslucentSidebar, forKey: Key.usesTranslucentSidebar) }
    }

    /// Where the connection this window remembers is kept.
    ///
    /// On this Mac: the connection stays in `~/.config` and nothing about it
    /// leaves the machine. The other answer copies the fields into the user's
    /// iCloud Drive and their database password into their iCloud Keychain, which
    /// is a decision about their credentials rather than about their convenience
    /// and is not one to make on their behalf. See `ConnectionStorage`, and
    /// `ConnectionStore.syncCaveat` for what a Mac that cannot do one half of it
    /// does instead.
    var connectionStorage: ConnectionStorage {
        didSet { store.set(connectionStorage.rawValue, forKey: Key.connectionStorage) }
    }

    /// Where a saved connection's password is kept, or that it is not kept.
    ///
    /// Asks every time on a fresh installation, for the reason `PasswordStorage`
    /// gives. Whichever of the other two is chosen, the one not chosen is
    /// cleared when a connection is saved: two stores holding one password are
    /// two to forget when somebody stops wanting it kept, and the stale one is
    /// always the one nobody looks at.
    var passwordStorage: PasswordStorage {
        didSet { store.set(passwordStorage.rawValue, forKey: Key.passwordStorage) }
    }

    /// Which connection folders the sidebar draws shut.
    ///
    /// Not a setting — no row in the Settings window sets it, a folder's own
    /// disclosure does — but it belongs beside them for the two reasons they are
    /// here: it is this Mac's, and it has to outlive the window. Not in
    /// `connections.json`, because a file carried to another machine would carry
    /// one person's idea of which folders are interesting with it; not `@State`
    /// on the list, because the list is drawn once per tab and two tabs would
    /// then disagree about which folders are shut.
    ///
    /// Sorted on the way out so that the file a person may read holds a folder
    /// list rather than whichever order the set happened to hash into.
    var shutConnectionFolders: Set<String> {
        didSet {
            store.set(shutConnectionFolders.sorted(), forKey: Key.shutConnectionFolders)
        }
    }

    /// What a fresh installation does. The only statement of these values.
    private static let registered: [String: Any] = [
        Key.hidesEmptyColumns: false,
        Key.confirmsDeletions: true,
        Key.insertsRowOfDefaults: false,
        Key.passwordStorage: PasswordStorage.never.rawValue,
        Key.usesTranslucentSidebar: false,
        Key.connectionStorage: ConnectionStorage.thisMac.rawValue,
        Key.shutConnectionFolders: [String]()
    ]

    private enum Key {
        static let hidesEmptyColumns = "dev.dbclient.hidesEmptyColumns"
        static let confirmsDeletions = "dev.dbclient.confirmsDeletions"
        static let insertsRowOfDefaults = "dev.dbclient.insertsRowOfDefaults"
        static let usesTranslucentSidebar = "dev.dbclient.usesTranslucentSidebar"
        static let connectionStorage = "dev.dbclient.connectionStorage"
        static let passwordStorage = "dev.dbclient.passwordStorage"
        static let shutConnectionFolders = "dev.dbclient.shutConnectionFolders"
    }

    /// Where the remembered connection is kept, read straight out of a store.
    ///
    /// For a caller with no window and therefore no `Preferences` to ask:
    /// `--bench` looks up the remembered connection before anything is on the
    /// main actor, which is why this is `nonisolated` — reading one default is
    /// not window state. An unset or unrecognised value is the local one, which
    /// is also what `registered` says, so a plist edited by hand or written by a
    /// later version offering a third place reads back as this Mac rather than
    /// as a crash.
    nonisolated static func connectionStorage(in store: UserDefaults = .standard)
        -> ConnectionStorage
    {
        ConnectionStorage(rawValue: store.string(forKey: Key.connectionStorage) ?? "") ?? .thisMac
    }

    @ObservationIgnored private let store: UserDefaults

    /// The store is injectable so that a check can be given a scratch one; see
    /// `--verify-preferences`, which has to set each of these both ways and must
    /// not leave a developer's own window changed behind it. Everything else
    /// takes the standard defaults.
    init(store: UserDefaults = .standard) {
        store.register(defaults: Self.registered)
        self.store = store
        hidesEmptyColumns = store.bool(forKey: Key.hidesEmptyColumns)
        confirmsDeletions = store.bool(forKey: Key.confirmsDeletions)
        insertsRowOfDefaults = store.bool(forKey: Key.insertsRowOfDefaults)
        usesTranslucentSidebar = store.bool(forKey: Key.usesTranslucentSidebar)
        // An unrecognised value reads as "ask every time" rather than as a
        // crash, which is what a plist edited by hand or written by a later
        // version offering a fourth place would otherwise be.
        passwordStorage =
            PasswordStorage(rawValue: store.string(forKey: Key.passwordStorage) ?? "") ?? .never
        connectionStorage = Self.connectionStorage(in: store)
        shutConnectionFolders = Set(store.stringArray(forKey: Key.shutConnectionFolders) ?? [])
    }
}
