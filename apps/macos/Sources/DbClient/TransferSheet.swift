import SwiftUI

/// The Transfer picker: which open connection this result's rows go to, and
/// which table on it they go into.
///
/// Palette-shaped, like `GoToPalette`, and for the same reason: the answer is a
/// table name, the window already holds every table name on the other
/// connection, and typing three letters beats reading a list. What it adds above
/// the field is the thing Go To has no equivalent of — a transfer has two
/// databases in it, and the second one has to be named before a table on it can
/// be.
///
/// Tables only, and only tables that are already there. The core writes with
/// `INSERT` and creates nothing, so a view in this list would be a row that
/// answers Return with an error from the server, and a free-text field would be
/// a way to spell a table that does not exist.
struct TransferSheet: View {
    @Bindable var model: AppModel
    /// Which connection is selected, by id rather than by position: the list is
    /// live, and a tab that finishes what it was doing joins it.
    @State private var chosen: UUID?
    @State private var needle = ""
    /// Which row Return would send to. Back to the top on every keystroke, for
    /// the reason `GoToPalette` gives.
    @State private var highlighted = 0
    @FocusState private var typing: Bool

    private var targets: [Session] { model.transferTargets }

    /// Falls back to the first target rather than to nothing, so the sheet still
    /// names a database after the selected connection has been closed under it.
    private var target: Session? { targets.first { $0.id == chosen } ?? targets.first }

    private var matches: [GoToTarget] {
        guard let target else { return [] }
        let tables =
            target.relations.values.flatMap { $0 }
            .filter { $0.kind != .view && $0.kind != .materializedView }
            .map { GoToTarget(schema: $0.schema, name: $0.name) }
        return GoTo.ranked(tables, matching: needle)
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            field
            Rectangle().fill(Theme.separator.color).frame(height: 1)
            list
        }
        .frame(width: 460, height: 360)
        .background(Theme.surface.color)
        // Escape closes it, as it does the go-to palette: a sheet has no title
        // bar, and this one has no Cancel button.
        .onExitCommand { model.isTransferPickerOpen = false }
        .onAppear {
            chosen = targets.first?.id
            typing = true
        }
    }

    /// What is being sent, and where. Both sentences, because a transfer is the
    /// one thing in this window that writes into a database the person is not
    /// looking at — and the count and the destination are what they would check.
    private var header: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(model.transferMessage)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textSecondary.color)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: Theme.Space.sm) {
                Text("Into")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                Picker("", selection: $chosen) {
                    ForEach(targets) { session in
                        Text(session.connectionLabel).tag(Optional(session.id))
                    }
                }
                .labelsHidden()
                .frame(maxWidth: 280)
                .onChange(of: chosen) { highlighted = 0 }
                .accessibilityLabel("Target connection")
            }
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var field: some View {
        TextField("Table on \(target?.connectionLabel ?? "the other connection")", text: $needle)
            .textFieldStyle(.plain)
            .font(Theme.Typography.title)
            .focused($typing)
            .padding(Theme.Space.md)
            .onChange(of: needle) { highlighted = 0 }
            .onSubmit { send(highlighted) }
            .onKeyPress(.upArrow) { move(-1) }
            .onKeyPress(.downArrow) { move(1) }
    }

    @ViewBuilder
    private var list: some View {
        if matches.isEmpty {
            // A reason rather than a blank panel. The two cases look identical
            // and are not: one is a database whose tables this window has not
            // read, the other is a needle that matches none of them.
            Text(emptyReason)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.textTertiary.color)
                .padding(Theme.Space.md)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        } else {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(matches.indices, id: \.self) { index in
                            row(at: index).id(index)
                        }
                    }
                }
                .onChange(of: highlighted) { proxy.scrollTo(highlighted, anchor: .center) }
            }
        }
    }

    private var emptyReason: String {
        guard let target else { return "No other connection is open." }
        guard needle.isEmpty else { return "No table on \(target.connectionLabel) matches." }
        return "\(target.connectionLabel) has no tables this window has read."
    }

    private func row(at index: Int) -> some View {
        Button {
            send(index)
        } label: {
            HStack(spacing: Theme.Space.sm) {
                Text(matches[index].name)
                    .font(Theme.Typography.body)
                    .foregroundStyle(Theme.text.color)
                    .lineLimit(1)
                Text(matches[index].detail)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.textTertiary.color)
                    .lineLimit(1)
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

    private func move(_ delta: Int) -> KeyPress.Result {
        guard !matches.isEmpty else { return .handled }
        highlighted = min(max(highlighted + delta, 0), matches.count - 1)
        return .handled
    }

    /// Closes first, then starts. The transfer may ask a question of its own —
    /// the target's production mark puts an alert up — and an alert raised
    /// behind a sheet that is still on screen is a window nobody can answer.
    private func send(_ index: Int) {
        guard matches.indices.contains(index), let target else { return }
        let table = matches[index].qualified
        model.isTransferPickerOpen = false
        model.transferCurrentResult(to: target, table: table)
    }
}
