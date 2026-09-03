import SwiftUI

// Shared chrome. Everything here exists because it appears in more than one
// pane; a component used once belongs next to its use site instead.

// MARK: - Tabs

/// The detail pane's tab bar.
///
/// Hand-built rather than a `Picker(.segmented)` so the tabs can carry icons at
/// the density the rest of the window uses, and so the selected tab reads as a
/// location rather than as a toggle. Each tab is a real `Button`, which is what
/// gives it the accessibility role and the keyboard shortcut.
struct TabBar: View {
    @Binding var selection: DetailTab
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            // ⌘1/⌘2/⌘3 are declared in the View menu, not here. They used to be
            // `keyboardShortcut` modifiers on these buttons, which worked and
            // was findable only by resting the pointer on a tab and reading the
            // tooltip — while the place a Mac user looks to learn what a window
            // can do listed nothing. Declared once, in the menu, for the reason
            // `AppMenu`'s Run note gives: one key equivalent claimed by both
            // AppKit's menu and SwiftUI's modifier is a race.
            ForEach(DetailTab.allCases) { tab in
                TabButton(tab: tab, isSelected: selection == tab) {
                    withAnimation(Theme.Motion.ease(reduceMotion, Theme.Motion.quick)) {
                        selection = tab
                    }
                }
            }
            Spacer()
        }
        .padding(.horizontal, Theme.Space.sm)
        .padding(.vertical, Theme.Space.xs + 1)
        .background(Theme.Surface.raised.color)
    }
}

private struct TabButton: View {
    let tab: DetailTab
    let isSelected: Bool
    let action: () -> Void

    @State private var isHovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: Theme.Space.xs + 2) {
                Image(systemName: tab.symbol)
                    .font(.system(size: 11, weight: .medium))
                Text(tab.rawValue)
                    .font(Theme.Typography.bodyEmphasis)
            }
            .foregroundStyle(foreground)
            .padding(.horizontal, Theme.Space.md)
            // 24pt tall: the macOS control rhythm. The 44pt minimum in the
            // mobile guidelines is a fingertip measurement and does not
            // transfer to a pointer-driven desktop app.
            .frame(height: 24)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.control)
                    .fill(background)
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.control))
        }
        .buttonStyle(.plain)
        .onHover { isHovering = $0 }
        .help("\(tab.rawValue) (⌘\(tabNumber))")
        .accessibilityLabel(tab.rawValue)
        .accessibilityAddTraits(isSelected ? [.isSelected, .isButton] : .isButton)
    }

    private var tabNumber: Int {
        (DetailTab.allCases.firstIndex(of: tab) ?? 0) + 1
    }

    private var foreground: Color {
        if isSelected { return Theme.Text.primary.color }
        return isHovering ? Theme.Text.primary.color : Theme.Text.secondary.color
    }

    /// The hover fill is deliberately weaker than the selected fill: hovering
    /// must never be mistakable for "this is the tab I am on".
    private var background: Color {
        if isSelected { return Theme.Accent.selection.opacity(0.30).color }
        return isHovering ? Theme.Surface.overlay.color : .clear
    }
}

// MARK: - Banner

/// An error shown in place rather than as a sheet.
///
/// A failed statement is routine in this application — a typo in a WHERE clause
/// produces one. A modal alert stops the session to report something the user is
/// about to fix, and throws away the text they need to read while fixing it.
struct InlineBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: Theme.Space.sm) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 11))
                .foregroundStyle(Theme.Semantic.dangerText.color)
                .padding(.top, 1)

            // Monospaced because the content is a database error, which quotes
            // SQL and points at column positions.
            Text(message)
                .font(Theme.Typography.monoSmall)
                .foregroundStyle(Theme.Semantic.dangerText.color)
                .textSelection(.enabled)
                .lineLimit(3)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                // Three lines is a bound on how much of the window an error may
                // take, not a claim that errors are three lines long. A server
                // that answers with a hint and a context stack loses the end of
                // it here, and the banner is the only place a browse or a
                // connection failure is ever shown — the Query pane has
                // `StatementNote` and nothing else does. The tooltip is what
                // makes the rest reachable, the same way it does for every
                // truncated name in the Structure tab.
                .help(message)

            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Theme.Semantic.dangerText.color)
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help("Dismiss")
            .accessibilityLabel("Dismiss error")
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
        .background(Theme.Semantic.danger.opacity(0.14).color)
        .overlay(alignment: .leading) {
            Rectangle()
                .fill(Theme.Semantic.danger.color)
                .frame(width: 2)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Error: \(message)")
    }
}

// MARK: - Empty state

/// What a pane shows when it has nothing.
///
/// Carries an icon and a next action rather than a bare sentence: an empty pane
/// is where a user is most likely to be unsure what the application wants from
/// them.
struct EmptyState: View {
    let symbol: String
    let title: String
    let hint: String

    var body: some View {
        VStack(spacing: Theme.Space.md) {
            Image(systemName: symbol)
                .font(.system(size: 26, weight: .light))
                .foregroundStyle(Theme.Text.tertiary.color)
            VStack(spacing: Theme.Space.xs) {
                Text(title)
                    .font(Theme.Typography.title)
                    .foregroundStyle(Theme.Text.secondary.color)
                Text(hint)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(Theme.Space.xl)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.Surface.canvas.color)
        .accessibilityElement(children: .combine)
    }
}

// MARK: - Status dot

/// Connection state as shape plus colour. The symbol changes with the state, so
/// the indicator still reads without colour vision.
struct StatusDot: View {
    enum State {
        case connecting, connected, failed

        var tone: Theme.Tone {
            switch self {
            case .connecting: return Theme.Semantic.warning
            case .connected: return Theme.Accent.execute
            case .failed: return Theme.Semantic.danger
            }
        }

        var symbol: String {
            switch self {
            case .connecting: return "circle.dotted"
            case .connected: return "circle.fill"
            case .failed: return "exclamationmark.circle.fill"
            }
        }

        var label: String {
            switch self {
            case .connecting: return "Connecting"
            case .connected: return "Connected"
            case .failed: return "Disconnected"
            }
        }
    }

    let state: State

    var body: some View {
        Image(systemName: state.symbol)
            .font(.system(size: 7))
            .foregroundStyle(state.tone.color)
            .accessibilityLabel(state.label)
    }
}

// MARK: - Sidebar filter

/// Name filter for the navigator.
///
/// A schema with hundreds of objects is the normal case, and scrolling a tree to
/// find one of them is the slowest thing a user does in a database client.
struct SidebarFilterField: View {
    @Binding var text: String
    @FocusState.Binding var focus: FocusArea?
    /// What the level above the relations is called on this connection, from
    /// `AppModel.containerNoun`. Both sentences below name it, and on the
    /// engines whose schemas are their databases the word "schema" would be
    /// telling somebody to search for a row the tree does not draw.
    let noun: String

    var body: some View {
        HStack(spacing: Theme.Space.xs + 2) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Theme.Text.tertiary.color)

            // Named for what it matches. "Filter" alone, over a tree of two
            // levels, leaves the user to discover by experiment that the schema
            // row is searched too. "Objects" rather than "tables" since the
            // routines joined the tree: it is the collective noun the footer
            // beneath already counts in, and naming all three kinds would not
            // fit a column this narrow.
            TextField("Filter objects and \(noun)s", text: $text)
                .textFieldStyle(.plain)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.Text.primary.color)
                .focused($focus, equals: .navigatorFilter)

            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.Text.tertiary.color)
                }
                .buttonStyle(.plain)
                .help("Clear filter (⎋)")
                .accessibilityLabel("Clear filter")
            }
        }
        .padding(.horizontal, Theme.Space.sm)
        .frame(height: 22)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.control)
                .fill(Theme.Surface.canvas.opacity(0.6).color)
        )
        .overlay(
            RoundedRectangle(cornerRadius: Theme.Radius.control)
                .strokeBorder(
                    focus == .navigatorFilter
                        ? Theme.Accent.selection.color : Theme.Border.hairline.color,
                    lineWidth: 1)
        )
        // Escape empties the field, which is the reflex every macOS search
        // field trains. The button is the visible way out and this is the one
        // the hands already know; a filter is easy to leave switched on by
        // accident, and both of them end with the whole tree back.
        .onExitCommand { text = "" }
        .help("Show only \(noun)s, tables and routines whose name contains this (⌥⌘F)")
    }
}

// MARK: - Field label

/// The small uppercase caption that names a control in the filter bar.
struct FieldLabel: View {
    let text: String

    var body: some View {
        Text(text)
            .font(Theme.Typography.micro.weight(.semibold))
            .foregroundStyle(Theme.Text.tertiary.color)
            .textCase(.uppercase)
            .accessibilityHidden(true)
    }
}

/// A bordered text field matching the window's density.
///
/// `.roundedBorder` is sized for a settings sheet; at 22pt with a 1pt border the
/// field sits on the same rhythm as the tabs and the status bar.
struct CompactField: View {
    let placeholder: String
    @Binding var text: String
    let area: FocusArea
    @FocusState.Binding var focus: FocusArea?
    let onSubmit: () -> Void
    /// Draws the value as dots. The connection form's password field is the one
    /// place in this window where showing what was typed is the wrong default.
    var isSecure = false

    var body: some View {
        entry
            .textFieldStyle(.plain)
            .font(Theme.Typography.monoSmall)
            .foregroundStyle(Theme.Text.primary.color)
            .focused($focus, equals: area)
            .onSubmit(onSubmit)
            .padding(.horizontal, Theme.Space.sm)
            .frame(height: 22)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.control)
                    .fill(Theme.Surface.canvas.opacity(0.6).color)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.Radius.control)
                    .strokeBorder(
                        focus == area ? Theme.Accent.selection.color : Theme.Border.hairline.color,
                        lineWidth: 1))
    }

    /// `SecureField` and `TextField` are different types, so the choice has to
    /// be made before the shared styling rather than as a modifier on it.
    @ViewBuilder
    private var entry: some View {
        if isSecure {
            SecureField(placeholder, text: $text)
        } else {
            TextField(placeholder, text: $text)
        }
    }
}
