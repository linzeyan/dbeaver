import AppKit
import SwiftUI

/// The Settings window, and the one object that owns it.
///
/// A panel of its own rather than a sheet on the session window: two of these
/// three settings change what the window in front of them is already showing —
/// a hidden column comes back, a refusal stops being one — and a sheet covers
/// the thing the reader is checking against.
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
        let view = NSHostingView(rootView: SettingsView(preferences: preferences))
        // The window takes its height from the rows rather than a number written
        // here, so an explanation that wraps to a third line is not clipped.
        panel.setContentSize(view.fittingSize)
        panel.contentView = view
        panel.center()
        panel.makeKeyAndOrderFront(nil)
        window = panel
    }
}

/// The three settings, each with the sentence that says what turning it on
/// costs.
///
/// Every one of them is a trade rather than a taste — a hidden column is data
/// off screen, a skipped confirmation is a delete with nothing between it and
/// the server, a row of defaults is an INSERT the database may not accept — and
/// a checkbox with only its own name beside it leaves the reader to find that
/// out by switching it on.
struct SettingsView: View {
    @Bindable var preferences: Preferences

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
        }
        .padding(Theme.Space.xl)
        .frame(width: 460, alignment: .leading)
        .background(Theme.background.color)
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
