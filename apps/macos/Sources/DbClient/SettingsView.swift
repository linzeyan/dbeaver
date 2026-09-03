import AppKit
import SwiftUI

/// The Settings window, and the one object that owns it.
///
/// A panel of its own rather than a sheet on the session window: most of these
/// settings change what the window in front of them is already showing — a
/// hidden column comes back, a refusal stops being one, the sidebar changes
/// material — and a sheet covers the thing the reader is checking against.
///
/// Held here rather than made on each press, so the second ⌘, raises the window
/// that is already open instead of stacking a second one. `NSWindow` releases
/// itself on close by default, which would leave this reference dangling, so
/// this is one of the few places that has to say otherwise.
@MainActor
final class SettingsWindow {
    private var window: NSWindow?

    /// `pane` exists for `--settings`, which captures a pane it cannot click
    /// to; the menu's own call never names one.
    func present(_ preferences: Preferences, pane: SettingsPane = .general) {
        if let window {
            window.makeKeyAndOrderFront(nil)
            return
        }
        // One size for every pane, and the Editor pane is taller than it: the
        // column scrolls inside the window rather than the window growing to
        // meet it. A panel that resized as the switcher moved put the row under
        // the pointer somewhere else on every click, and the tallest pane had
        // grown past the bottom edge of a laptop screen.
        let panel = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 460, height: 560),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false)
        panel.title = "Settings"
        panel.isReleasedWhenClosed = false
        panel.backgroundColor = NSColor(Theme.Surface.canvas.color)
        // Asked here, once per window rather than once per draw: finding out
        // whether this build may sync means looking for iCloud Drive and writing a
        // throwaway Keychain item, and neither answer changes while the panel is
        // open.
        let view = NSHostingView(
            rootView: SettingsView(
                preferences: preferences, syncCaveat: ConnectionStore.syncCaveat(),
                pane: pane))
        panel.contentView = view
        // Centred once, when the window is made. Every later ⌘, raises the
        // window that is already there, wherever the person left it.
        panel.center()
        panel.makeKeyAndOrderFront(nil)
        window = panel
    }
}

/// The settings, each with the sentence that says what turning it on costs.
///
/// Every one of them costs something — a hidden column is data off screen, a
/// skipped confirmation is a delete with nothing between it and the server, a
/// row of defaults is an INSERT the database may not accept, a translucent
/// sidebar samples the pane beside it — and a checkbox with only its own name
/// beside it leaves the reader to find that out by switching it on.
struct SettingsView: View {
    @Bindable var preferences: Preferences
    /// Which half of syncing is unavailable here, if either is.
    ///
    /// Handed in rather than asked for here, because the answer costs a look at
    /// iCloud Drive and a throwaway Keychain item and neither changes while the
    /// panel is open: it is asked once, on the way in, rather than from a
    /// `.task` that would arrive after the rows around it had been laid out.
    /// Standing under both answers rather than only under iCloud: it is a fact
    /// about the choice, not about the current pick.
    let syncCaveat: String?
    /// Whether the pane's rows are wrapped in a scroll view and held to the
    /// window's height.
    ///
    /// True everywhere but the checks, which measure how tall a pane wants to
    /// be: a scroll view reports the height it was handed rather than the
    /// height of what it holds, so a caveat that adds a line would stop showing
    /// up in the very measurement that exists to catch it.
    let scrolls: Bool

    /// Which pane is showing. Not remembered between openings: the panel is
    /// opened to change one particular thing, and the pane it was last left on
    /// is a worse guess at that thing than the first one is.
    @State private var pane: SettingsPane

    /// The window always opens on General; the parameter exists for
    /// `--verify-preferences`, which measures a pane it cannot click to.
    init(
        preferences: Preferences, syncCaveat: String?, scrolls: Bool = true,
        pane: SettingsPane = .general
    ) {
        self.preferences = preferences
        self.syncCaveat = syncCaveat
        self.scrolls = scrolls
        _pane = State(initialValue: pane)
    }

    var body: some View {
        VStack(spacing: 0) {
            switcher
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            if scrolls {
                // The switcher stays above the scroll rather than inside it:
                // the way to another pane is not something to scroll up for.
                ScrollView {
                    paneColumn
                }
                // No rubber band under a pane that already fits, which is most
                // of them: a bounce is the window saying there is more below.
                .scrollBounceBehavior(.basedOnSize)
            } else {
                paneColumn
            }
        }
        .frame(width: 460, height: scrolls ? 560 : nil)
        .background(Theme.Surface.canvas.color)
    }

    /// One pane's rows, as a column. Its own property because the body puts it
    /// inside a scroll view or hands it over whole, and the two must not be two
    /// spellings of the same column.
    private var paneColumn: some View {
        VStack(alignment: .leading, spacing: Theme.Space.lg) {
            switch pane {
            case .general: general
            case .connections: connections
            case .grid: grid
            case .editor: editor
            case .sidebar: sidebar
            case .mcp: mcp
            }
        }
        .padding(Theme.Space.xl)
        .frame(width: 460, alignment: .leading)
    }

    /// The pane switcher: a symbol over a name, one button per pane.
    ///
    /// Along the top rather than down the side, which is where a Mac has kept
    /// its preference tabs for twenty years — and at 460 points a sidebar would
    /// take a third of the width these explanations need.
    private var switcher: some View {
        // The tightest spacing, because six 72-point buttons at any looser
        // one no longer fit the window's 460 points.
        HStack(spacing: Theme.Space.xs) {
            ForEach(SettingsPane.allCases) { candidate in
                Button {
                    pane = candidate
                } label: {
                    VStack(spacing: Theme.Space.xs) {
                        Image(systemName: candidate.symbol)
                            .font(.system(size: 15))
                        Text(candidate.rawValue)
                            .font(Theme.Typography.caption)
                    }
                    .foregroundStyle(
                        candidate == pane
                            ? Theme.Accent.selection.color : Theme.Text.secondary.color
                    )
                    .frame(width: 72)
                    .padding(.vertical, Theme.Space.sm)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("\(candidate.rawValue) settings")
                .accessibilityAddTraits(candidate == pane ? [.isSelected] : [])
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.top, Theme.Space.sm)
    }

    /// What the application keeps between launches, and where it keeps it.
    @ViewBuilder private var general: some View {
        SettingsChoice(
            title: "Appearance",
            explanation:
                "Match the system follows the menu bar, including when it changes at "
                + "sunset. Light and Dark hold this window where you put it, which is "
                + "what a bright room or a projector wants and what the system's clock "
                + "knows nothing about. The grid changes with the rest of the window: "
                + "every colour it draws is read at the moment it is drawn.",
            caveat: nil,
            selection: $preferences.appearance)

        SettingsToggle(
            title: "Reopen the last window's tabs",
            explanation:
                "A launch puts back the tabs you had open, with the SQL you had typed in "
                + "each. None of them is connected — every one is its connection's form, "
                + "filled in, waiting for Return — because this application never opens a "
                + "database you did not ask for this time. The tabs are written to "
                + "~/.config/dbclient/session.json when you quit, and turning this off "
                + "deletes that file.",
            isOn: $preferences.restoresSession)

        SettingsChoice(
            title: "Keep connections",
            explanation:
                "The last connection that opened is remembered so the next launch does not "
                + "ask again. On this Mac, the fields are a JSON file under XDG_CONFIG_HOME — "
                + "~/.config/dbclient/connection.json — which you can read, edit and keep in "
                + "your dotfiles. In iCloud, that same file goes to iCloud Drive instead, so "
                + "another Mac signed in to the same Apple Account opens the same database. "
                + "The password is in your login Keychain either way, never in the file.",
            caveat: syncCaveat,
            selection: $preferences.connectionStorage)

        SettingsChoice(
            title: "Keep passwords",
            explanation:
                "Ask every time keeps nothing anywhere. On this Mac writes them to "
                + "~/.config/dbclient/credentials, encrypted with a key derived from this "
                + "machine and this account and stored nowhere — the file is unreadable on "
                + "another Mac, in a backup, or in a dotfiles repository, though not against "
                + "something already running as you. In the login Keychain is the system's "
                + "own store and the stronger answer, except that this build is signed "
                + "ad-hoc: its signature changes on every rebuild, so macOS asks you to "
                + "authorise the read again and Always Allow does not hold. Whichever you "
                + "pick, saving a connection clears the other one.",
            caveat: nil,
            selection: $preferences.passwordStorage)
    }

    /// What happens to a connection while nobody is using it.
    @ViewBuilder private var connections: some View {
        SettingsNumber(
            title: "Keep connections alive",
            explanation:
                "An idle connection is pinged this often so that a server timeout or a "
                + "NAT table does not quietly drop it. The number here is the default; "
                + "each connection's form can name its own, and 0 in either place turns "
                + "the pinging off. Only wire-protocol databases are pinged — on the "
                + "cloud warehouses a ping is a billed API call keeping nothing alive.",
            unit: "seconds",
            value: $preferences.keepAliveSeconds)

        SettingsToggle(
            title: "Notify when a connection drops",
            explanation:
                "If a connection goes red while this window is in the background, macOS "
                + "posts one notification naming it. Nothing is posted while the window "
                + "is in front — the red dot and the status line already say it there.",
            isOn: $preferences.notifiesOnDisconnect)
    }

    /// The data surface: what it leaves out, and what it asks before sending.
    @ViewBuilder private var grid: some View {
        SettingsToggle(
            title: "Hide columns that are empty",
            explanation:
                "A column that is null in every row fetched so far is left out of the "
                + "grid. The column is still in the result, so Copy and Export keep it. "
                + "Decided from the first few pages and then kept, so a value further "
                + "down than that will not bring the column back.",
            isOn: $preferences.hidesEmptyColumns)

        SettingsToggle(
            title: "Confirm before deleting rows",
            explanation:
                "Save asks before it sends the rows marked for deletion. Marked rows are "
                + "already struck through and counted beside Save, so this is a second "
                + "confirmation — for the press of Save that was about something else.",
            isOn: $preferences.confirmsDeletions)

        SettingsToggle(
            title: "Insert a row of defaults for an empty new row",
            explanation:
                "A new row nobody typed into is sent as the table's own defaults, in "
                + "whichever way this database spells that. Off, it is refused here and "
                + "the row is named. Databases with no spelling for it refuse either way.",
            isOn: $preferences.insertsRowOfDefaults)
    }

    /// The SQL editor's habits: its type, its indentation, what typing does.
    @ViewBuilder private var editor: some View {
        SettingsStepper(
            title: "Editor font size",
            explanation:
                "The size the SQL editor and its completion list draw at. Bigger "
                + "type is fewer lines on screen: the editor pane holds about ten "
                + "at 13 points and about seven at 18.",
            unit: "pt",
            range: Preferences.editorFontSizes,
            value: $preferences.editorFontSize)

        SettingsChoice(
            title: "Tab width",
            explanation:
                "How many columns a tab is worth: the width a tab character "
                + "displays at, and the stop indentation lands on. Four is what "
                + "most SQL in circulation is formatted to; a file written at one "
                + "width reads differently at the other.",
            caveat: nil,
            selection: $preferences.editorTabWidth)

        SettingsToggle(
            title: "Indent with spaces",
            explanation:
                "Tab writes spaces up to the next tab stop instead of a tab "
                + "character, so the indentation holds its columns in every editor "
                + "the SQL ever visits. Deleting it is space by space, and the Tab "
                + "key no longer types the character itself.",
            isOn: $preferences.editorSoftTabs)

        SettingsToggle(
            title: "Auto-indent new lines",
            explanation:
                "Return carries the current line's leading whitespace onto the new "
                + "line, so a clause stays under its clause without retyping the "
                + "indent. Leaving an indented block means deleting the indent "
                + "Return just gave you.",
            isOn: $preferences.editorAutoIndent)

        SettingsToggle(
            title: "Pair brackets and quotes",
            explanation:
                "Typing ( [ ' or \" also writes its partner, around the caret or "
                + "around the selection, and typing the closer walks past the one "
                + "already there. That last part is the cost: the closing keystroke "
                + "moves the caret instead of adding a character.",
            isOn: $preferences.editorAutoPairs)

        SettingsToggle(
            title: "Uppercase keywords as you type",
            explanation:
                "Finishing a word with a space or Return lifts it to upper case "
                + "when the dialect calls it a keyword. This rewrites text you "
                + "typed — the only setting here that does — and an unquoted "
                + "column named order is lifted along with the real keywords.",
            isOn: $preferences.editorUppercasesKeywords)

        SettingsEditorColours(preferences: preferences)
    }

    /// The MCP server: the switch, its two numbers, and — while it runs —
    /// the two strings a client needs to connect.
    @ViewBuilder private var mcp: some View {
        SettingsToggle(
            title: "Serve exposed connections over MCP",
            explanation:
                "An HTTP server on this Mac lets AI tools — Claude Code, Cursor and "
                + "anything else that speaks MCP — read the connections you have marked "
                + "Expose to MCP on their forms. Reads only: statements that write are "
                + "refused before any database sees them. The cost is a listening port "
                + "on 127.0.0.1 and, for passwords kept in the Keychain, an "
                + "authorisation panel the first time each connection opens.",
            isOn: $preferences.mcpServerEnabled)

        SettingsNumber(
            title: "Port",
            explanation:
                "Where the server listens on 127.0.0.1. Changing it restarts the "
                + "server, which mints a new token.",
            unit: "1024–65535",
            value: $preferences.mcpServerPort)

        SettingsNumber(
            title: "Row cap",
            explanation:
                "The most rows one query answers with. These rows land in a model's "
                + "context window, where more is not better — a truncated result says "
                + "so, and the model can ask a narrower question.",
            unit: "rows",
            value: $preferences.mcpRowCap)

        if preferences.mcpServerEnabled {
            MCPStatus(port: preferences.mcpServerPort)
        }
    }

    /// The object tree down the left of the window.
    @ViewBuilder private var sidebar: some View {
        SettingsToggle(
            title: "Translucent sidebar",
            explanation:
                "The sidebar takes the system's translucency, which is what a Mac sidebar "
                + "usually looks like. It samples whatever the detail pane draws behind it, "
                + "so full-width bands on the right — the Structure tab's section strip — "
                + "show through the object tree as a stripe at their own height.",
            isOn: $preferences.usesTranslucentSidebar)

        SettingsToggle(
            title: "Show system schemas",
            explanation:
                "The schemas the engine keeps for itself — pg_catalog, information_schema, "
                + "MySQL's four, Mongo's admin and local — appear in the tree, sorted after "
                + "your own. They are worth reading and there are a lot of them: pg_catalog "
                + "alone holds a few thousand objects, and the editor completes from the "
                + "same names.",
            isOn: $preferences.showsSystemSchemas)
    }
}

/// Which page of the Settings window is showing.
///
/// Three pages rather than one because six settings about four different parts
/// of the window had accumulated in a single column, and a column of six
/// checkboxes is read by nobody who came looking for one of them.
enum SettingsPane: String, CaseIterable, Identifiable {
    case general = "General"
    case connections = "Connections"
    case grid = "Grid"
    case editor = "Editor"
    case sidebar = "Sidebar"
    case mcp = "MCP"

    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .connections: return "network"
        case .grid: return "tablecells"
        case .editor: return "text.cursor"
        case .sidebar: return "sidebar.left"
        case .mcp: return "server.rack"
        }
    }
}

/// One setting with more than two answers: its name, what each answer costs, and
/// the radio group.
///
/// A radio group rather than a checkbox, because "somewhere else" is not the
/// negation of "here" — a box labelled "Sync with iCloud" leaves the reader to
/// infer where the connection goes when it is clear, and both answers here are
/// places worth naming.
///
/// The caveat is a sentence under the control for the case where the answer
/// chosen cannot be honoured. It is drawn in the warning tone and only when there
/// is one: a control that quietly does something other than what it says is the
/// failure this exists to prevent.
/// What a setting with more than two answers looks like.
///
/// Generic over the choice because there are two of them now and they differ in
/// nothing but their cases. A second copy of this view would be a second place
/// to change when the row's spacing does, and the two would drift.
private protocol SettingsOption: CaseIterable, Identifiable, Hashable {
    var label: String { get }
}

extension Appearance.Setting: SettingsOption {}
extension ConnectionStorage: SettingsOption {}
extension PasswordStorage: SettingsOption {}
extension EditorTabWidth: SettingsOption {}

private struct SettingsChoice<Option: SettingsOption>: View
where Option.AllCases: RandomAccessCollection {
    let title: String
    let explanation: String
    let caveat: String?
    @Binding var selection: Option

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(title)
                .font(Theme.Typography.bodyEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
            Text(explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .fixedSize(horizontal: false, vertical: true)
            Picker("", selection: $selection) {
                ForEach(Option.allCases) { place in
                    Text(place.label).tag(place)
                }
            }
            .pickerStyle(.radioGroup)
            .labelsHidden()
            // The two labels keep the system control font, which is a point
            // larger than the titles above them. Tried and abandoned: a
            // `.radioGroup` draws its own labels, so neither `.font` on the
            // option's `Text` nor on the picker reaches them, and the only way
            // to that point would be hand-drawing two radio buttons.
            .accessibilityLabel(title)
            .accessibilityHint(explanation)

            if let caveat {
                HStack(alignment: .top, spacing: Theme.Space.xs) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.Semantic.warning.color)
                        .padding(.top, 1)
                    Text(caveat)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.Semantic.warning.color)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }
}

/// One setting that is a number from a short range: its name, what moving it
/// costs, and a stepper with the value spelled out beside it.
///
/// A stepper rather than a slider or a field, because the range is nine
/// integers: every value is one click from its neighbour, nothing needs
/// typing, and — unlike a slider — the control cannot land between answers.
private struct SettingsStepper: View {
    let title: String
    let explanation: String
    let unit: String
    let range: ClosedRange<Int>
    @Binding var value: Int

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(title)
                .font(Theme.Typography.bodyEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
            Text(explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: Theme.Space.sm) {
                // The label, value and hint ride on the control itself, so
                // VoiceOver lands on something it can actually increment; the
                // text beside it repeats the value and is hidden from the
                // tree rather than read twice.
                Stepper("", value: $value, in: range)
                    .labelsHidden()
                    .accessibilityLabel(title)
                    .accessibilityValue("\(value) \(unit)")
                    .accessibilityHint(explanation)
                Text("\(value) \(unit)")
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.Text.primary.color)
                    .accessibilityHidden(true)
            }
        }
    }
}

/// One setting that is a number without a short range: its name, what the
/// number does, and a field the width of one. Where the range is a handful of
/// integers, `SettingsStepper` is the control; this one is for values worth
/// typing.
///
/// Shaped like `SettingsToggle` so the panes read as one column, with the
/// control on its own line under the explanation — a checkbox reads at any
/// width, but a text field pushed to the right edge of a paragraph looks like
/// a typo.
private struct SettingsNumber: View {
    let title: String
    let explanation: String
    /// Said beside the field rather than folded into the title, so the number
    /// somebody types has its meaning at the point they type it.
    let unit: String
    @Binding var value: Int

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(title)
                .font(Theme.Typography.bodyEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
            Text(explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: Theme.Space.sm) {
                TextField("", text: digits)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 56)
                    .accessibilityLabel(title)
                    .accessibilityHint(explanation)
                Text(unit)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .accessibilityHidden(true)
            }
        }
    }

    /// The number as the field holds it: digits only, and an emptied box is
    /// zero — for this control that is "off", which is the only thing an empty
    /// default could honestly mean.
    private var digits: Binding<String> {
        Binding(
            get: { String(value) },
            set: { typed in
                let digits = typed.filter(\.isNumber)
                value = digits.isEmpty ? 0 : (Int(digits) ?? 0)
            })
    }
}

/// The editor's colours: the theme they follow, a well per slot, and the way
/// back.
///
/// The menu holds two answers because two exist — the palette, and whatever
/// the wells now hold. It is derived from the colours rather than stored
/// beside them: it is a fact about them, and a stored copy could go on saying
/// Default over customised wells. Choosing Default is Reset by another name;
/// Custom is not chosen, it is what touching any well makes true. The button
/// says the same thing for the reader who does not think to ask a menu to
/// undo.
private struct SettingsEditorColours: View {
    @Bindable var preferences: Preferences

    /// What customising costs, said once above the wells rather than once per
    /// well: the Default palette's contrast is measured in `Theme`, and a
    /// custom colour walks out from under those measurements.
    private static let explanation =
        "One well per thing the editor colours: the syntax tokens, the "
        + "paper they sit on, the caret, the selection, and the band "
        + "under the statement ⌘R would run in a script. The Default "
        + "palette is checked for contrast against its background; a "
        + "Custom theme is trusted to have done its own checking."

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text("Colours")
                .font(Theme.Typography.bodyEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
            Text(Self.explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .fixedSize(horizontal: false, vertical: true)
            controls
            wells
        }
    }

    private var controls: some View {
        HStack(spacing: Theme.Space.sm) {
            Picker("", selection: themeChoice) {
                Text("Default").tag(EditorThemeChoice.standard)
                Text("Custom").tag(EditorThemeChoice.custom)
            }
            .pickerStyle(.menu)
            .labelsHidden()
            .frame(width: 120)
            .accessibilityLabel("Editor theme")
            Button("Reset") { preferences.resetEditorTheme() }
                .disabled(!preferences.editorThemeIsCustom)
                .accessibilityHint("Returns every colour to the Default theme.")
        }
        .padding(.bottom, Theme.Space.xs)
    }

    /// A `Grid` rather than a lazy one: the Settings panel is sized to this
    /// view's `fittingSize`, and laziness is exactly the promise not to know
    /// your height until scrolled to.
    private var wells: some View {
        Grid(
            alignment: .leading, horizontalSpacing: Theme.Space.lg,
            verticalSpacing: Theme.Space.xs
        ) {
            GridRow {
                well("Background", $preferences.editorBackgroundColor, \.background)
                well("Text", $preferences.editorTextColor, \.text)
            }
            GridRow {
                well("Keywords", $preferences.editorKeywordColor, \.keyword)
                well("Strings", $preferences.editorStringColor, \.string)
            }
            GridRow {
                well("Dollar quotes", $preferences.editorDollarQuotedColor, \.dollarQuoted)
                well("Numbers", $preferences.editorNumberColor, \.number)
            }
            GridRow {
                well(
                    "Quoted identifiers", $preferences.editorQuotedIdentifierColor,
                    \.quotedIdentifier)
                well("Comments", $preferences.editorCommentColor, \.comment)
            }
            GridRow {
                well("Caret", $preferences.editorCaretColor, \.caret)
                well("Selection", $preferences.editorSelectionColor, \.selection)
            }
            GridRow {
                well("Current statement", $preferences.editorStatementColor, \.statement)
            }
        }
    }

    /// Derived, not stored: see the type comment. Setting it to Default is a
    /// reset; setting it to Custom changes nothing, because Custom is a state
    /// the wells put the theme in, not one to opt into.
    private var themeChoice: Binding<EditorThemeChoice> {
        Binding(
            get: { preferences.editorThemeIsCustom ? .custom : .standard },
            set: { choice in
                if choice == .standard { preferences.resetEditorTheme() }
            })
    }

    /// One slot: the well, then its name. The label rides on the control for
    /// VoiceOver and stands beside it for everyone else, the way
    /// `SettingsStepper` does it.
    private func well(
        _ label: String, _ hex: Binding<String>, _ slot: KeyPath<EditorTheme, Theme.Tone>
    ) -> some View {
        HStack(spacing: Theme.Space.xs) {
            ColorPicker("", selection: colour(of: hex, or: EditorTheme.defaults[keyPath: slot]))
                .labelsHidden()
                .accessibilityLabel("\(label) colour")
            Text(label)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .accessibilityHidden(true)
        }
    }

    /// The hex string as the `Color` a well speaks, canonical both ways.
    ///
    /// Reading cannot fail after `Preferences.init` folded the store's values,
    /// but a binding that could show black would be one bug away from doing
    /// it, so the fallback is the slot's default. Writing keeps the old value
    /// for a colour with no sRGB reading, which the system picker does not
    /// offer anyway.
    private func colour(of hex: Binding<String>, or fallback: Theme.Tone) -> Binding<Color> {
        Binding(
            get: { (Theme.Tone(hex: hex.wrappedValue) ?? fallback).color },
            set: { picked in
                if let tone = Theme.Tone(picked) { hex.wrappedValue = tone.hex }
            })
    }
}

/// The two answers the Theme menu offers. `standard` rather than `default`,
/// which is a keyword worth more than backticks here.
private enum EditorThemeChoice {
    case standard
    case custom
}

/// What a running MCP server tells the person pairing a client to it: the
/// endpoint, the token, and a Copy button for each.
///
/// The token appears here and nowhere else — never on disk, never in a log —
/// so this row is the pairing ceremony: copy, paste into the client, done.
/// Shown only while the box above is ticked, and read fresh from the
/// coordinator on every render, which the tick itself forces.
private struct MCPStatus: View {
    let port: Int

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            if let token = MCPCoordinator.shared.token {
                credential("Endpoint", value: "http://127.0.0.1:\(port)/mcp")
                credential("Token", value: token)
            } else {
                HStack(alignment: .top, spacing: Theme.Space.xs) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.Semantic.warning.color)
                        .padding(.top, 1)
                    Text(
                        "Not running. Usually the port: something else already "
                            + "listens there, or the number is out of range."
                    )
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Semantic.warning.color)
                    .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }

    private func credential(_ name: String, value: String) -> some View {
        HStack(spacing: Theme.Space.sm) {
            Text(name)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .frame(width: 64, alignment: .leading)
            Text(value)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .truncationMode(.middle)
            Button("Copy") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(value, forType: .string)
            }
            .font(Theme.Typography.caption)
            .accessibilityLabel("Copy \(name)")
        }
    }
}

/// One setting: its name, what it does to the window, and the switch.
private struct SettingsToggle: View {
    let title: String
    let explanation: String
    @Binding var isOn: Bool

    var body: some View {
        Toggle(isOn: $isOn) {
            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                Text(title)
                    .font(Theme.Typography.bodyEmphasis)
                    .foregroundStyle(Theme.Text.primary.color)
                Text(explanation)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .toggleStyle(.checkbox)
        // The label is the whole block, so the explanation is part of the
        // clickable target rather than text beside a small square.
        .accessibilityLabel(title)
        .accessibilityHint(explanation)
    }
}
