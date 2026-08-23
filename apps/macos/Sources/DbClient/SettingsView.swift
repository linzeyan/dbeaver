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

    func present(_ preferences: Preferences) {
        if let window {
            window.makeKeyAndOrderFront(nil)
            return
        }
        let panel = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 460, height: 10),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false)
        panel.title = "Settings"
        panel.isReleasedWhenClosed = false
        panel.backgroundColor = NSColor(Theme.background.color)
        // Asked here, once per window rather than once per draw: finding out
        // whether this build may sync means looking for iCloud Drive and writing a
        // throwaway Keychain item, and neither answer changes while the panel is
        // open.
        let view = NSHostingView(
            rootView: SettingsView(
                preferences: preferences, syncCaveat: ConnectionStore.syncCaveat(),
                onPaneChange: { [weak self] in self?.fitToPane() }))
        // The window takes its height from the rows rather than a number written
        // here, so an explanation that wraps to a third line is not clipped.
        panel.setContentSize(view.fittingSize)
        panel.contentView = view
        panel.center()
        panel.makeKeyAndOrderFront(nil)
        window = panel
    }

    /// Re-takes the panel's height from the pane now showing.
    ///
    /// The panes are not the same height — Grid holds three settings and
    /// Sidebar one — and a window sized once to the tallest of them would stand
    /// two thirds empty on the others.
    ///
    /// Deferred by a turn of the run loop rather than measured on the spot: the
    /// notice arrives from inside the SwiftUI update that is changing the pane,
    /// and until that update has laid the new pane out the hosting view's
    /// `fittingSize` still describes the pane being left.
    private func fitToPane() {
        DispatchQueue.main.async { [weak self] in
            guard let window = self?.window, let view = window.contentView else { return }
            window.setContentSize(view.fittingSize)
        }
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
    /// Handed in rather than asked for here, and that is about the window's
    /// height. `SettingsWindow` measures this view once and sizes the panel to
    /// it, so a sentence that appeared later — from a `.task`, or on selecting
    /// the option it is about — would be a sentence drawn past the bottom edge.
    /// Standing under both answers rather than only under iCloud, for the same
    /// reason: it is a fact about the choice, not about the current pick.
    let syncCaveat: String?
    /// Told when the switcher moves to another pane, so that the window can
    /// take that pane's height.
    ///
    /// A closure out rather than a size read back in: the panel is the only
    /// thing that knows how it is sized, and it already measures this view once
    /// when it opens.
    var onPaneChange: () -> Void = {}

    /// Which pane is showing. Not remembered between openings: the panel is
    /// opened to change one particular thing, and the pane it was last left on
    /// is a worse guess at that thing than the first one is.
    @State private var pane: SettingsPane

    /// The window always opens on General; the parameter exists for
    /// `--verify-preferences`, which measures a pane it cannot click to.
    init(
        preferences: Preferences, syncCaveat: String?,
        onPaneChange: @escaping () -> Void = {}, pane: SettingsPane = .general
    ) {
        self.preferences = preferences
        self.syncCaveat = syncCaveat
        self.onPaneChange = onPaneChange
        _pane = State(initialValue: pane)
    }

    var body: some View {
        VStack(spacing: 0) {
            switcher
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            VStack(alignment: .leading, spacing: Theme.Space.lg) {
                switch pane {
                case .general: general
                case .grid: grid
                case .editor: editor
                case .sidebar: sidebar
                }
            }
            .padding(Theme.Space.xl)
            .frame(width: 460, alignment: .leading)
        }
        .frame(width: 460)
        .background(Theme.background.color)
        .onChange(of: pane) { onPaneChange() }
    }

    /// The pane switcher: a symbol over a name, one button per pane.
    ///
    /// Along the top rather than down the side, which is where a Mac has kept
    /// its preference tabs for twenty years — and at 460 points a sidebar would
    /// take a third of the width these explanations need.
    private var switcher: some View {
        HStack(spacing: Theme.Space.lg) {
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
                        candidate == pane ? Theme.accent.color : Theme.textSecondary.color
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
    }
}

/// Which page of the Settings window is showing.
///
/// Three pages rather than one because six settings about four different parts
/// of the window had accumulated in a single column, and a column of six
/// checkboxes is read by nobody who came looking for one of them.
enum SettingsPane: String, CaseIterable, Identifiable {
    case general = "General"
    case grid = "Grid"
    case editor = "Editor"
    case sidebar = "Sidebar"

    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .grid: return "tablecells"
        case .editor: return "text.cursor"
        case .sidebar: return "sidebar.left"
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
                .foregroundStyle(Theme.text.color)
            Text(explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textSecondary.color)
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
                        .foregroundStyle(Theme.warning.color)
                        .padding(.top, 1)
                    Text(caveat)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(Theme.warning.color)
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
                .foregroundStyle(Theme.text.color)
            Text(explanation)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textSecondary.color)
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
                    .foregroundStyle(Theme.text.color)
                    .accessibilityHidden(true)
            }
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
                    .foregroundStyle(Theme.text.color)
                Text(explanation)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textSecondary.color)
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
