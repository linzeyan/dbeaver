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

    /// What a fresh installation does. The only statement of these values.
    private static let registered: [String: Any] = [
        Key.hidesEmptyColumns: false,
        Key.confirmsDeletions: true,
        Key.insertsRowOfDefaults: false,
        Key.usesTranslucentSidebar: false
    ]

    private enum Key {
        static let hidesEmptyColumns = "dev.dbclient.hidesEmptyColumns"
        static let confirmsDeletions = "dev.dbclient.confirmsDeletions"
        static let insertsRowOfDefaults = "dev.dbclient.insertsRowOfDefaults"
        static let usesTranslucentSidebar = "dev.dbclient.usesTranslucentSidebar"
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
    }
}
