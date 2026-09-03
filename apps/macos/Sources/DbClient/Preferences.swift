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

    /// Which of the palette's two sets of values every window draws with.
    ///
    /// System, which is the platform's own answer and the one that keeps a Mac
    /// looking like itself when the menu bar flips at sunset. The other two are
    /// here because an appearance is also a working condition — a bright room, a
    /// dark one, a projector — and those do not change when the system's clock
    /// says they should.
    ///
    /// Resolved to a palette by `AppearanceController`, which asks AppKit what
    /// the app is actually drawing in rather than reading this: under `system`
    /// only AppKit knows, and it is also what changes without anything here
    /// being told.
    var appearance: Appearance.Setting {
        didSet { store.set(appearance.rawValue, forKey: Key.appearance) }
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
    /// Whether the tree draws the schemas the engine keeps for itself.
    ///
    /// Off. `pg_catalog` alone is a few thousand objects and the tree is for
    /// getting to a table, so on by default would bury every user schema under
    /// the server's own. It is a setting rather than a fixed rule because
    /// reading `pg_catalog` is a real thing to want, and the previous
    /// arrangement — four drivers with the list in their `WHERE` clause — made
    /// it a thing this client could not do at all.
    var showsSystemSchemas: Bool {
        didSet { store.set(showsSystemSchemas, forKey: Key.showsSystemSchemas) }
    }

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

    /// The colours the SQL editor draws with, each kept as a hex string —
    /// `#RRGGBB`, or `#RRGGBBAA` where the tone is translucent.
    ///
    /// Eleven properties rather than a theme object in the store, because that
    /// is what every preference here is: a key somebody can read and edit in
    /// the plist, defaulted in `registered`, one line each. Hex rather than the
    /// archived `NSColor` data Sequel Ace keeps under its
    /// `SPCustomQueryEditor*Color` keys, because a colour a person can read in
    /// a plist is one they can carry in dotfiles and fix by hand. The resolved
    /// layer the editor draws from is `editorTheme`, below; a value that fails
    /// to parse is folded back to the default at init, the way an out-of-range
    /// font size is, so no misspelt plist draws black.
    var editorBackgroundColor: String {
        didSet { store.set(editorBackgroundColor, forKey: Key.editorBackgroundColor) }
    }

    var editorTextColor: String {
        didSet { store.set(editorTextColor, forKey: Key.editorTextColor) }
    }

    var editorKeywordColor: String {
        didSet { store.set(editorKeywordColor, forKey: Key.editorKeywordColor) }
    }

    var editorStringColor: String {
        didSet { store.set(editorStringColor, forKey: Key.editorStringColor) }
    }

    var editorDollarQuotedColor: String {
        didSet { store.set(editorDollarQuotedColor, forKey: Key.editorDollarQuotedColor) }
    }

    var editorNumberColor: String {
        didSet { store.set(editorNumberColor, forKey: Key.editorNumberColor) }
    }

    var editorQuotedIdentifierColor: String {
        didSet { store.set(editorQuotedIdentifierColor, forKey: Key.editorQuotedIdentifierColor) }
    }

    var editorCommentColor: String {
        didSet { store.set(editorCommentColor, forKey: Key.editorCommentColor) }
    }

    var editorCaretColor: String {
        didSet { store.set(editorCaretColor, forKey: Key.editorCaretColor) }
    }

    var editorSelectionColor: String {
        didSet { store.set(editorSelectionColor, forKey: Key.editorSelectionColor) }
    }

    var editorStatementColor: String {
        didSet { store.set(editorStatementColor, forKey: Key.editorStatementColor) }
    }

    /// Whether the MCP server is listening.
    ///
    /// Off, and everything else about it is fenced behind this: no port is
    /// bound, no token exists, and the exposed connections are exposed to
    /// nothing. On, agents on this machine can read the connections somebody
    /// marked — reads only, localhost only, bearer token required — and the
    /// costs are a listening port and, for Keychain-stored passwords, an
    /// authorisation panel the first time each connection is opened.
    var mcpServerEnabled: Bool {
        didSet { store.set(mcpServerEnabled, forKey: Key.mcpServerEnabled) }
    }

    /// The port the MCP server binds on 127.0.0.1.
    ///
    /// 8765, and folded into 1024–65535 on read: below 1024 needs privileges
    /// this process does not have, and a hand-edited plist should move the
    /// port, not disable the server in a way nothing reports.
    var mcpServerPort: Int {
        didSet { store.set(mcpServerPort, forKey: Key.mcpServerPort) }
    }

    /// The most rows one MCP query answers with.
    ///
    /// 1000, deliberately a tenth of what Sequel Ace allows: these rows land
    /// in a language model's context, where ten thousand rows of JSON do not
    /// inform, they drown. The reply says when it was cut short, and an agent
    /// that needs more can ask a narrower question.
    ///
    /// Held as typed, and folded by `foldedRowCap` wherever it is used — the
    /// field can hold a 0 for as long as somebody is mid-edit, and 0 is the
    /// one number this must never be read as: it would answer every query
    /// with no rows and a `truncated: true`, which is a server that looks
    /// like it is working.
    var mcpRowCap: Int {
        didSet { store.set(mcpRowCap, forKey: Key.mcpRowCap) }
    }

    /// What a row cap outside the sensible range is read as.
    ///
    /// Stated once because two places ask: this type at launch, reading a
    /// plist somebody may have edited, and the coordinator on every keystroke
    /// in the field. A rule spelled in both would drift the day one is
    /// corrected — which is the same reason `registered` is the only
    /// statement of the defaults.
    static let defaultRowCap = 1000
    static func foldedRowCap(_ raw: Int) -> Int { raw <= 0 ? defaultRowCap : raw }

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

    /// Whether a launch puts back the tabs the last window had open.
    ///
    /// On. What comes back is the shell and not the connections — every restored
    /// tab is a form filled in, and nothing is dialled until somebody presses
    /// Enter — so there is no surprise here to protect anybody from, and the
    /// worst it can do is put a strip of tabs on screen that somebody closes.
    /// That is what makes it different from the settings above that default to
    /// off: those change what a statement does, and this changes what a window
    /// opens showing.
    ///
    /// Off is for somebody who wants each launch to start from nothing, and
    /// turning it off deletes what was kept rather than only stopping the next
    /// write — see `SessionRestoreStore.clear`.
    var restoresSession: Bool {
        didSet { store.set(restoresSession, forKey: Key.restoresSession) }
    }

    /// What a fresh installation does. The only statement of these values.
    private static let registered: [String: Any] = [
        Key.restoresSession: true,
        Key.hidesEmptyColumns: false,
        Key.confirmsDeletions: true,
        Key.insertsRowOfDefaults: false,
        Key.passwordStorage: PasswordStorage.never.rawValue,
        Key.usesTranslucentSidebar: false,
        Key.showsSystemSchemas: false,
        Key.connectionStorage: ConnectionStorage.thisMac.rawValue,
        Key.appearance: Appearance.Setting.system.rawValue,
        Key.shutConnectionFolders: [String](),
        Key.editorFontSize: 13,
        Key.editorTabWidth: EditorTabWidth.four.rawValue,
        Key.editorSoftTabs: false,
        Key.editorAutoIndent: true,
        Key.editorAutoPairs: true,
        Key.editorUppercasesKeywords: false,
        Key.keepAliveSeconds: 60,
        Key.notifiesOnDisconnect: true,
        Key.editorBackgroundColor: EditorTheme.defaults.background.hex,
        Key.editorTextColor: EditorTheme.defaults.text.hex,
        Key.editorKeywordColor: EditorTheme.defaults.keyword.hex,
        Key.editorStringColor: EditorTheme.defaults.string.hex,
        Key.editorDollarQuotedColor: EditorTheme.defaults.dollarQuoted.hex,
        Key.editorNumberColor: EditorTheme.defaults.number.hex,
        Key.editorQuotedIdentifierColor: EditorTheme.defaults.quotedIdentifier.hex,
        Key.editorCommentColor: EditorTheme.defaults.comment.hex,
        Key.editorCaretColor: EditorTheme.defaults.caret.hex,
        Key.editorSelectionColor: EditorTheme.defaults.selection.hex,
        Key.editorStatementColor: EditorTheme.defaults.statement.hex,
        Key.mcpServerEnabled: false,
        Key.mcpServerPort: 8765,
        Key.mcpRowCap: defaultRowCap
    ]

    /// The sizes the Settings window offers, and therefore the sizes the value
    /// on disk is folded back into. The plist is a file somebody can edit, and
    /// a 0 read literally draws no text at all while a 96 leaves three lines on
    /// screen — both states the window that wrote the value could never reach.
    static let editorFontSizes = 10...18

    private enum Key {
        static let restoresSession = "dev.dbclient.restoresSession"
        static let hidesEmptyColumns = "dev.dbclient.hidesEmptyColumns"
        static let confirmsDeletions = "dev.dbclient.confirmsDeletions"
        static let insertsRowOfDefaults = "dev.dbclient.insertsRowOfDefaults"
        static let usesTranslucentSidebar = "dev.dbclient.usesTranslucentSidebar"
        static let showsSystemSchemas = "dev.dbclient.showsSystemSchemas"
        static let connectionStorage = "dev.dbclient.connectionStorage"
        static let appearance = "dev.dbclient.appearance"
        static let passwordStorage = "dev.dbclient.passwordStorage"
        static let keepAliveSeconds = "dev.dbclient.keepAliveSeconds"
        static let notifiesOnDisconnect = "dev.dbclient.notifiesOnDisconnect"
        static let mcpServerEnabled = "dev.dbclient.mcpServerEnabled"
        static let mcpServerPort = "dev.dbclient.mcpServerPort"
        static let mcpRowCap = "dev.dbclient.mcpRowCap"
        static let shutConnectionFolders = "dev.dbclient.shutConnectionFolders"
        static let editorFontSize = "dev.dbclient.editorFontSize"
        static let editorTabWidth = "dev.dbclient.editorTabWidth"
        static let editorSoftTabs = "dev.dbclient.editorSoftTabs"
        static let editorAutoIndent = "dev.dbclient.editorAutoIndent"
        static let editorAutoPairs = "dev.dbclient.editorAutoPairs"
        static let editorUppercasesKeywords = "dev.dbclient.editorUppercasesKeywords"
        static let editorBackgroundColor = "dev.dbclient.editorBackgroundColor"
        static let editorTextColor = "dev.dbclient.editorTextColor"
        static let editorKeywordColor = "dev.dbclient.editorKeywordColor"
        static let editorStringColor = "dev.dbclient.editorStringColor"
        static let editorDollarQuotedColor = "dev.dbclient.editorDollarQuotedColor"
        static let editorNumberColor = "dev.dbclient.editorNumberColor"
        static let editorQuotedIdentifierColor = "dev.dbclient.editorQuotedIdentifierColor"
        static let editorCommentColor = "dev.dbclient.editorCommentColor"
        static let editorCaretColor = "dev.dbclient.editorCaretColor"
        static let editorSelectionColor = "dev.dbclient.editorSelectionColor"
        static let editorStatementColor = "dev.dbclient.editorStatementColor"
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

    /// What the editor draws with, resolved: each slot the user's colour where
    /// one was set, the palette's where not. Gathered once, for the reason
    /// `editorTyping` is.
    ///
    /// A stored value equal to the default's own spelling resolves to the
    /// palette tone itself rather than through the codec, so the Default theme
    /// is *exactly* the colours this build has always drawn — not a copy
    /// quantised through hex digits, which for the translucent selection band
    /// would be off by a part in a thousand. The parse fallback is that same
    /// palette tone: after init the two cannot disagree, but a resolver that
    /// could hand back black would be one bug away from doing it.
    var editorTheme: EditorTheme {
        EditorTheme(
            background: Self.tone(editorBackgroundColor, or: EditorTheme.defaults.background),
            text: Self.tone(editorTextColor, or: EditorTheme.defaults.text),
            keyword: Self.tone(editorKeywordColor, or: EditorTheme.defaults.keyword),
            string: Self.tone(editorStringColor, or: EditorTheme.defaults.string),
            dollarQuoted: Self.tone(editorDollarQuotedColor, or: EditorTheme.defaults.dollarQuoted),
            number: Self.tone(editorNumberColor, or: EditorTheme.defaults.number),
            quotedIdentifier: Self.tone(
                editorQuotedIdentifierColor, or: EditorTheme.defaults.quotedIdentifier),
            comment: Self.tone(editorCommentColor, or: EditorTheme.defaults.comment),
            caret: Self.tone(editorCaretColor, or: EditorTheme.defaults.caret),
            selection: Self.tone(editorSelectionColor, or: EditorTheme.defaults.selection),
            statement: Self.tone(editorStatementColor, or: EditorTheme.defaults.statement))
    }

    /// Whether any editor colour differs from the palette: the fact the Theme
    /// menu shows. Derived rather than stored, so it cannot disagree with the
    /// colours — a kept "Custom" flag would be a twelfth value to reset.
    var editorThemeIsCustom: Bool { editorTheme != EditorTheme.defaults }

    /// Moves every editor slot that is still the palette's onto the palette the
    /// appearance now in force resolves to, and leaves a colour somebody chose
    /// alone.
    ///
    /// Per slot rather than all-or-nothing, because the two cases have to be
    /// told apart: a keyword colour the user picked is theirs in both
    /// appearances, and the eight slots they never touched would otherwise stay
    /// at values chosen to be read on a near-black canvas. Called with the
    /// palette as it stood a moment ago, which is the only way to know which of
    /// those a stored spelling was.
    func followEditorPalette(from previous: EditorTheme) {
        let next = EditorTheme.defaults
        if editorBackgroundColor == previous.background.hex {
            editorBackgroundColor = next.background.hex
        }
        if editorTextColor == previous.text.hex { editorTextColor = next.text.hex }
        if editorKeywordColor == previous.keyword.hex { editorKeywordColor = next.keyword.hex }
        if editorStringColor == previous.string.hex { editorStringColor = next.string.hex }
        if editorDollarQuotedColor == previous.dollarQuoted.hex {
            editorDollarQuotedColor = next.dollarQuoted.hex
        }
        if editorNumberColor == previous.number.hex { editorNumberColor = next.number.hex }
        if editorQuotedIdentifierColor == previous.quotedIdentifier.hex {
            editorQuotedIdentifierColor = next.quotedIdentifier.hex
        }
        if editorCommentColor == previous.comment.hex { editorCommentColor = next.comment.hex }
        if editorCaretColor == previous.caret.hex { editorCaretColor = next.caret.hex }
        if editorSelectionColor == previous.selection.hex {
            editorSelectionColor = next.selection.hex
        }
        if editorStatementColor == previous.statement.hex {
            editorStatementColor = next.statement.hex
        }
    }

    /// Every editor colour back to the palette: the Reset control, and what
    /// choosing Default in the Theme menu means.
    func resetEditorTheme() {
        editorBackgroundColor = EditorTheme.defaults.background.hex
        editorTextColor = EditorTheme.defaults.text.hex
        editorKeywordColor = EditorTheme.defaults.keyword.hex
        editorStringColor = EditorTheme.defaults.string.hex
        editorDollarQuotedColor = EditorTheme.defaults.dollarQuoted.hex
        editorNumberColor = EditorTheme.defaults.number.hex
        editorQuotedIdentifierColor = EditorTheme.defaults.quotedIdentifier.hex
        editorCommentColor = EditorTheme.defaults.comment.hex
        editorCaretColor = EditorTheme.defaults.caret.hex
        editorSelectionColor = EditorTheme.defaults.selection.hex
        editorStatementColor = EditorTheme.defaults.statement.hex
    }

    /// One slot resolved; see `editorTheme`.
    private static func tone(_ kept: String, or fallback: Theme.Tone) -> Theme.Tone {
        kept == fallback.hex ? fallback : (Theme.Tone(hex: kept) ?? fallback)
    }

    /// What a colour read off the disk becomes: its canonical spelling, or the
    /// default's where it does not parse. Canonical because "is this still the
    /// default?" is asked of strings, and `#a78bfa` hand-typed in lower case
    /// is the default keyword colour, not a custom theme.
    private static func colour(_ raw: String?, or fallback: Theme.Tone) -> String {
        guard let raw, let parsed = Theme.Tone(hex: raw) else { return fallback.hex }
        return parsed.hex
    }

    /// The store is injectable so that a check can be given a scratch one; see
    /// `--verify-preferences`, which has to set each of these both ways and must
    /// not leave a developer's own window changed behind it. Everything else
    /// takes the standard defaults.
    init(store: UserDefaults = .standard) {
        store.register(defaults: Self.registered)
        self.store = store
        restoresSession = store.bool(forKey: Key.restoresSession)
        hidesEmptyColumns = store.bool(forKey: Key.hidesEmptyColumns)
        confirmsDeletions = store.bool(forKey: Key.confirmsDeletions)
        insertsRowOfDefaults = store.bool(forKey: Key.insertsRowOfDefaults)
        usesTranslucentSidebar = store.bool(forKey: Key.usesTranslucentSidebar)
        showsSystemSchemas = store.bool(forKey: Key.showsSystemSchemas)
        // An unrecognised value reads as "ask every time" rather than as a
        // crash, which is what a plist edited by hand or written by a later
        // version offering a fourth place would otherwise be.
        passwordStorage =
            PasswordStorage(rawValue: store.string(forKey: Key.passwordStorage) ?? "") ?? .never
        connectionStorage = Self.connectionStorage(in: store)
        // An unrecognised appearance is the system's, for the reason
        // `passwordStorage` gives.
        appearance =
            Appearance.Setting(rawValue: store.string(forKey: Key.appearance) ?? "") ?? .system
        // A negative number in a hand-edited plist is read as the default
        // rather than as an interval: there is no pinging backwards in time.
        let pinging = store.integer(forKey: Key.keepAliveSeconds)
        keepAliveSeconds = pinging < 0 ? 60 : pinging
        notifiesOnDisconnect = store.bool(forKey: Key.notifiesOnDisconnect)
        mcpServerEnabled = store.bool(forKey: Key.mcpServerEnabled)
        // Folded rather than trusted, for the reason the font size is: a port
        // this process cannot bind, read literally, would be the server failing
        // every start over a number nothing on screen can show is wrong.
        mcpServerPort = min(max(store.integer(forKey: Key.mcpServerPort), 1024), 65535)
        mcpRowCap = Self.foldedRowCap(store.integer(forKey: Key.mcpRowCap))
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
        // A colour that does not parse reads as the palette's own, for the
        // reason `passwordStorage` gives — and never as black, which is what
        // most colour APIs quietly make of a misspelt string.
        editorBackgroundColor = Self.colour(
            store.string(forKey: Key.editorBackgroundColor), or: EditorTheme.defaults.background)
        editorTextColor = Self.colour(
            store.string(forKey: Key.editorTextColor), or: EditorTheme.defaults.text)
        editorKeywordColor = Self.colour(
            store.string(forKey: Key.editorKeywordColor), or: EditorTheme.defaults.keyword)
        editorStringColor = Self.colour(
            store.string(forKey: Key.editorStringColor), or: EditorTheme.defaults.string)
        editorDollarQuotedColor = Self.colour(
            store.string(forKey: Key.editorDollarQuotedColor),
            or: EditorTheme.defaults.dollarQuoted)
        editorNumberColor = Self.colour(
            store.string(forKey: Key.editorNumberColor), or: EditorTheme.defaults.number)
        editorQuotedIdentifierColor = Self.colour(
            store.string(forKey: Key.editorQuotedIdentifierColor),
            or: EditorTheme.defaults.quotedIdentifier)
        editorCommentColor = Self.colour(
            store.string(forKey: Key.editorCommentColor), or: EditorTheme.defaults.comment)
        editorCaretColor = Self.colour(
            store.string(forKey: Key.editorCaretColor), or: EditorTheme.defaults.caret)
        editorSelectionColor = Self.colour(
            store.string(forKey: Key.editorSelectionColor), or: EditorTheme.defaults.selection)
        editorStatementColor = Self.colour(
            store.string(forKey: Key.editorStatementColor), or: EditorTheme.defaults.statement)
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
