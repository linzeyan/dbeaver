import SwiftUI

/// The ⇧⌘O palette: type a table's name, arrow to it, Return to open it.
///
/// A sheet rather than a floating panel like `CompletionPopup`. That one has to
/// hang under a caret in a scrolled text view and leave the keyboard where it
/// was, which is what forces a panel; this covers the window and takes the
/// keyboard, which is what a sheet already is.
///
/// The rows are buttons as well as arrow targets. This is a keyboard tool
/// reached by a shortcut, but a list that can only be driven from the keyboard
/// ignores half the ways people use a mouse — the same reason `CompletionPopup`
/// answers clicks.
struct GoToPalette: View {
    @Bindable var model: AppModel
    @State private var needle = ""
    /// Which row Return would open, as an index into `matches`.
    ///
    /// Back to the top on every keystroke rather than kept: the list underneath
    /// has changed, so the row that was third is not the row that is third now,
    /// and a highlight that stayed where it was would move under somebody's
    /// hands.
    @State private var highlighted = 0
    @FocusState private var typing: Bool

    private var matches: [GoToTarget] { GoTo.ranked(model.goToTargets, matching: needle) }

    var body: some View {
        VStack(spacing: 0) {
            field
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            list
        }
        .frame(width: 460, height: 320)
        .background(Theme.surface.color)
        // Escape closes it. A sheet has no title bar to close and this one has
        // no Cancel button, so without this the only way out is to pick
        // something — which is a palette that punishes opening it by accident.
        .onExitCommand { model.isGoToOpen = false }
    }

    private var field: some View {
        TextField("Go to table", text: $needle)
            .textFieldStyle(.plain)
            .font(Theme.Typography.title)
            .focused($typing)
            .padding(Theme.Space.md)
            .onAppear { typing = true }
            .onChange(of: needle) { highlighted = 0 }
            .onSubmit { open(highlighted) }
            .onKeyPress(.upArrow) { move(-1) }
            .onKeyPress(.downArrow) { move(1) }
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(matches.indices, id: \.self) { index in
                        row(at: index).id(index)
                    }
                }
            }
            // The arrow keys move a highlight that may be off screen, and a
            // selection you cannot see is the same as no selection.
            .onChange(of: highlighted) { proxy.scrollTo(highlighted, anchor: .center) }
        }
    }

    private func row(at index: Int) -> some View {
        Button {
            open(index)
        } label: {
            HStack(spacing: Theme.Space.sm) {
                Text(matches[index].name)
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.text.color)
                // The schema, quietly. Two schemas can hold a table of the same
                // name, and without this the palette would offer the same row
                // twice with nothing to choose between them.
                Text(matches[index].schema)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.xs + 1)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(index == highlighted ? Theme.accent.opacity(0.35).color : Color.clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    /// Moves the highlight, stopping at either end.
    ///
    /// Stopping rather than wrapping, for the reason `CompletionPopup.move`
    /// gives: a list that jumps from the last row to the first has taken a key
    /// pressed to go down and gone up with it.
    private func move(_ delta: Int) -> KeyPress.Result {
        guard !matches.isEmpty else { return .handled }
        highlighted = min(max(highlighted + delta, 0), matches.count - 1)
        return .handled
    }

    private func open(_ index: Int) {
        guard matches.indices.contains(index) else { return }
        model.goTo(matches[index])
    }
}
