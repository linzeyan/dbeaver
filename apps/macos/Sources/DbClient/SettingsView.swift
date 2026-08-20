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
                preferences: preferences, syncCaveat: ConnectionStore.syncCaveat()))
        // The window takes its height from the rows rather than a number written
        // here, so an explanation that wraps to a third line is not clipped.
        panel.setContentSize(view.fittingSize)
        panel.contentView = view
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
    /// Handed in rather than asked for here, and that is about the window's
    /// height. `SettingsWindow` measures this view once and sizes the panel to
    /// it, so a sentence that appeared later — from a `.task`, or on selecting
    /// the option it is about — would be a sentence drawn past the bottom edge.
    /// Standing under both answers rather than only under iCloud, for the same
    /// reason: it is a fact about the choice, not about the current pick.
    let syncCaveat: String?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.lg) {
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

            SettingsToggle(
                title: "Translucent sidebar",
                explanation:
                    "The sidebar takes the system's translucency, which is what a Mac sidebar "
                    + "usually looks like. It samples whatever the detail pane draws behind it, "
                    + "so full-width bands on the right — the Structure tab's section strip — "
                    + "show through the object tree as a stripe at their own height.",
                isOn: $preferences.usesTranslucentSidebar)

            SettingsToggle(
                title: "Remember passwords",
                explanation:
                    "A saved connection's password is kept in your login Keychain and filled in "
                    + "when you connect. Off, because this build is signed ad-hoc: its signature "
                    + "changes every time it is rebuilt, so macOS treats each build as a new "
                    + "application and asks you to authorise the read again — Always Allow does "
                    + "not hold. While this is off nothing is written to the Keychain and "
                    + "nothing is read from it; type the password when you connect.",
                isOn: $preferences.remembersPasswords)

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
        }
        .padding(Theme.Space.xl)
        .frame(width: 460, alignment: .leading)
        .background(Theme.background.color)
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
private struct SettingsChoice: View {
    let title: String
    let explanation: String
    let caveat: String?
    @Binding var selection: ConnectionStorage

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
                ForEach(ConnectionStorage.allCases) { place in
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
