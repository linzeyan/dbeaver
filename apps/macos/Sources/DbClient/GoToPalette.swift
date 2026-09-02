import SwiftUI

/// The ⇧⌘O palette: type a name, arrow to it, Return to open it.
///
/// Over everything the window holds rather than over the connection in front of
/// it — every tab's tables, the other tabs themselves, and the saved statements.
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
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            footer
        }
        .frame(width: 460, height: 320)
        .background(Theme.surface.color)
        // Escape closes it. A sheet has no title bar to close and this one has
        // no Cancel button, so without this the only way out is to pick
        // something — which is a palette that punishes opening it by accident.
        .onExitCommand { model.isGoToOpen = false }
    }

    /// The count and the three keys, along the bottom.
    ///
    /// A sheet has no title bar, and this one has no Cancel button — the way
    /// out is a key nothing on screen mentions. The arrows and Return are in
    /// the same position: they work, and a palette opened by somebody who has
    /// not read the source has no way to find out that they do.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            // Spelled out: the default plural is the singular plus an s, and
            // "matchs" is not a word.
            Text(AppModel.pluralized(matches.count, "match", "matches"))
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.textTertiary.color)
            Spacer(minLength: Theme.Space.sm)
            Text("↑↓ move · ↩ open · ⎋ close")
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.textTertiary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 22)
        .accessibilityHidden(true)
    }

    private var field: some View {
        TextField("Go to table, connection or saved query", text: $needle)
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
                    .lineLimit(1)
                // The schema, or the statement a saved query would type,
                // quietly. Two schemas can hold a table of the same name and
                // two favorites can be named alike, and without this the
                // palette would offer the same row twice with nothing to
                // choose between them. Tail-truncated, because a statement is
                // as long as somebody wrote it.
                Text(matches[index].detail)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 0)
                if let label = matches[index].kind.label {
                    Text(label)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.textTertiary.color)
                }
                connection(matches[index])
            }
            .padding(.horizontal, Theme.Space.md)
            .padding(.vertical, Theme.Space.xs + 1)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(index == highlighted ? Theme.accent.opacity(0.35).color : Color.clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    /// Which database the row is in, at the end of the row.
    ///
    /// The seam `spec.md` left when the badge was written: a mark saying which
    /// driver a row belongs to is noise in a list that can only hold one, and
    /// this list can now hold several. So it is drawn exactly where it means
    /// something — on the rows that are not in the tab in front, and on the
    /// connections themselves — and a window with one connection open looks
    /// exactly as it did.
    ///
    /// The family shape and then the name, which is the order the tab bar draws
    /// the same two marks in. Nothing at all for a tab still holding a form: an
    /// unmapped scheme is what `DriverBadge` gives a driver nobody named yet,
    /// and its fallback cylinder here would be the palette guessing.
    @ViewBuilder
    private func connection(_ target: GoToTarget) -> some View {
        if DriverBadge.isMapped(scheme: target.scheme) {
            Image(systemName: DriverBadge.familySymbol(forScheme: target.scheme))
                .font(.system(size: 10))
                .foregroundStyle(Theme.textTertiary.color)
        }
        if !target.connection.isEmpty {
            Text(target.connection)
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.textTertiary.color)
                .lineLimit(1)
        }
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
