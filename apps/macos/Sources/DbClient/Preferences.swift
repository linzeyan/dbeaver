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

    /// The SQL editor's type size, in points.
    ///
    /// 13, which is the size the editor has always drawn at. The editor is the
    /// one surface somebody reads for hours at a stretch, which is what earns
    /// it a size setting; the grid is deliberately not covered, because its
    /// metrics are measured into the glyph atlas and resizing it is its own
    /// piece of work. The completion popup follows along a point behind, so
    /// the list under the caret keeps its relation to the text it completes.
    /// The cost is lines: bigger type means fewer of them in the same pane.
    var editorFontSize: Int {
        didSet { store.set(editorFontSize, forKey: Key.editorFontSize) }
    }

    /// How many columns a tab is worth in the SQL editor: the width a hard tab
    /// displays at, and the stop soft tabs indent to.
    ///
    /// Four, which is Sequel Ace's default and the width most SQL in
    /// circulation is formatted to; eight is offered because it is the other
    /// answer terminals have ever given. A choice of two rather than a number
    /// field, because every value in between is a width nobody's files use and
    /// a control that offers it invites drift from what the rest of the team
    /// sees.
    var editorTabWidth: EditorTabWidth {
        didSet { store.set(editorTabWidth.rawValue, forKey: Key.editorTabWidth) }
    }

    /// Whether Tab in the SQL editor writes spaces up to the next tab stop
    /// instead of a tab character.
    ///
    /// Off, which is the tab character — Sequel Ace's default. Soft tabs make
    /// the indentation the same columns in every editor the file ever visits,
    /// at the cost that Backspace undoes it a space at a time.
    var editorSoftTabs: Bool {
        didSet { store.set(editorSoftTabs, forKey: Key.editorSoftTabs) }
    }

    /// Whether Return in the SQL editor carries the current line's leading
    /// whitespace onto the new line.
    ///
    /// On, Sequel Ace's default: multi-line SQL is written indented under its
    /// clause, and re-typing the indent on every line is the cost of turning
    /// this off. The cost of on is one habit — leaving an indented block means
    /// deleting the indent Return just gave you.
    var editorAutoIndent: Bool {
        didSet { store.set(editorAutoIndent, forKey: Key.editorAutoIndent) }
    }

    /// Whether typing an opening bracket or quote in the SQL editor also
    /// writes its partner.
    ///
    /// On, Sequel Ace's default. The partner arrives around the caret, a
    /// selection is wrapped instead of replaced, and typing the closer walks
    /// past the one already there. The cost falls on the habit of typing both
    /// halves oneself — the closing keystroke moves the caret instead of
    /// adding a character, which reads as a swallowed key until the habit
    /// adjusts.
    var editorAutoPairs: Bool {
        didSet { store.set(editorAutoPairs, forKey: Key.editorAutoPairs) }
    }

    /// Whether SQL keywords are lifted to upper case as they are typed.
    ///
    /// Off, Sequel Ace's default, and the only editor habit here that rewrites
    /// characters the user typed — which is why it starts off: an editor that
    /// changes your text is a claim to know better, made on every word. On,
    /// finishing a word with a space or Return uppercases it when the core's
    /// lexer calls it a keyword — the same opinion the colours run on — and
    /// the lexer reads without context, so an unquoted column deliberately
    /// named `order` is lifted too.
    var editorUppercasesKeywords: Bool {
        didSet { store.set(editorUppercasesKeywords, forKey: Key.editorUppercasesKeywords) }
    }

    /// How often an idle connection is pinged when its own entry does not say,
    /// in seconds. Zero turns the default off.
    ///
    /// Sixty, which is under every idle timeout worth worrying about — cloud
    /// load balancers start dropping quiet connections at around four minutes,
    /// and the common on-premise middleboxes later than that — while costing
    /// one empty round trip a minute on connections that are open anyway. Per
    /// connection on the form, with this as the answer for the entries where
    /// nobody typed one; see `ConnectionSettings.keepAliveSeconds` for how nil
    /// defers here.
    var keepAliveSeconds: Int {
        didSet { store.set(keepAliveSeconds, forKey: Key.keepAliveSeconds) }
    }

    /// Whether a connection going red while the window is in the background
    /// posts a system notification.
    ///
    /// On, because the person who switched away is exactly the person the
    /// window's own signals cannot reach: the dot turns red and the status
    /// line names the way back, and nobody is looking at either. Off is for
    /// whoever finds any notification too many — it is their notification
    /// centre — and turning it off costs only that the dead tab waits to be
    /// discovered, which is what it did before this existed.
    var notifiesOnDisconnect: Bool {
        didSet { store.set(notifiesOnDisconnect, forKey: Key.notifiesOnDisconnect) }
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
        Key.shutConnectionFolders: [String](),
        Key.editorFontSize: 13,
        Key.editorTabWidth: EditorTabWidth.four.rawValue,
        Key.editorSoftTabs: false,
        Key.editorAutoIndent: true,
        Key.editorAutoPairs: true,
        Key.editorUppercasesKeywords: false,
        Key.keepAliveSeconds: 60,
        Key.notifiesOnDisconnect: true
    ]

    /// The sizes the Settings window offers, and therefore the sizes the value
    /// on disk is folded back into. The plist is a file somebody can edit, and
    /// a 0 read literally draws no text at all while a 96 leaves three lines on
    /// screen — both states the window that wrote the value could never reach.
    static let editorFontSizes = 10...18

    private enum Key {
        static let hidesEmptyColumns = "dev.dbclient.hidesEmptyColumns"
        static let confirmsDeletions = "dev.dbclient.confirmsDeletions"
        static let insertsRowOfDefaults = "dev.dbclient.insertsRowOfDefaults"
        static let usesTranslucentSidebar = "dev.dbclient.usesTranslucentSidebar"
        static let connectionStorage = "dev.dbclient.connectionStorage"
        static let passwordStorage = "dev.dbclient.passwordStorage"
        static let keepAliveSeconds = "dev.dbclient.keepAliveSeconds"
        static let notifiesOnDisconnect = "dev.dbclient.notifiesOnDisconnect"
        static let shutConnectionFolders = "dev.dbclient.shutConnectionFolders"
        static let editorFontSize = "dev.dbclient.editorFontSize"
        static let editorTabWidth = "dev.dbclient.editorTabWidth"
        static let editorSoftTabs = "dev.dbclient.editorSoftTabs"
        static let editorAutoIndent = "dev.dbclient.editorAutoIndent"
        static let editorAutoPairs = "dev.dbclient.editorAutoPairs"
        static let editorUppercasesKeywords = "dev.dbclient.editorUppercasesKeywords"
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

    /// What the editor's typing rules read, gathered once. The editor takes
    /// this value rather than the object around it, so the rules stay checkable
    /// as plain data — see `EditorTyping.Rules`.
    var editorTyping: EditorTyping.Rules {
        EditorTyping.Rules(
            tabWidth: editorTabWidth.rawValue,
            softTabs: editorSoftTabs,
            autoIndent: editorAutoIndent,
            autoPairs: editorAutoPairs,
            uppercasesKeywords: editorUppercasesKeywords)
    }

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
        // A negative number in a hand-edited plist is read as the default
        // rather than as an interval: there is no pinging backwards in time.
        let pinging = store.integer(forKey: Key.keepAliveSeconds)
        keepAliveSeconds = pinging < 0 ? 60 : pinging
        notifiesOnDisconnect = store.bool(forKey: Key.notifiesOnDisconnect)
        shutConnectionFolders = Set(store.stringArray(forKey: Key.shutConnectionFolders) ?? [])
        editorFontSize = min(
            max(store.integer(forKey: Key.editorFontSize), Self.editorFontSizes.lowerBound),
            Self.editorFontSizes.upperBound)
        // A width the window never offered reads as the default rather than as
        // a crash or a zero, for the reason `passwordStorage` gives.
        editorTabWidth =
            EditorTabWidth(rawValue: store.integer(forKey: Key.editorTabWidth)) ?? .four
        editorSoftTabs = store.bool(forKey: Key.editorSoftTabs)
        editorAutoIndent = store.bool(forKey: Key.editorAutoIndent)
        editorAutoPairs = store.bool(forKey: Key.editorAutoPairs)
        editorUppercasesKeywords = store.bool(forKey: Key.editorUppercasesKeywords)
    }
}

/// What "tab width" is allowed to mean, as the Settings window offers it. See
/// `Preferences.editorTabWidth` for why there are two answers and not a number
/// field. Top level like `ConnectionStorage`, so the Settings window's radio
/// group can conform it without crossing the class's isolation.
enum EditorTabWidth: Int, CaseIterable, Identifiable {
    case four = 4
    case eight = 8

    var id: Int { rawValue }

    var label: String { "\(rawValue) columns" }
}
