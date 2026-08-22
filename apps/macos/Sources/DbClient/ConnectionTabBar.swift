import SwiftUI

/// The strip of open connections across the top of the window.
///
/// This is the window's tab bar, and the only place a connection is switched.
/// The chip that used to do it from the toolbar is gone: a menu that switches
/// and a label that names are two answers to one question, and two answers is
/// how they come to disagree.
///
/// Full width, above the split rather than over the detail column. The tree in
/// the sidebar belongs to whichever connection is in front, so a strip that
/// stopped at the divider would claim to change half of what it changes.
struct ConnectionTabBar: View {
    var model: AppModel

    var body: some View {
        HStack(spacing: 0) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 1) {
                    ForEach(Array(model.sessions.enumerated()), id: \.element.id) { entry in
                        ConnectionTabItem(
                            session: entry.element,
                            isActive: entry.offset == model.activeSession,
                            // A window always has a tab, so the only one there
                            // is carries no ✕ until it has a connection to
                            // close. Closing it then is what Disconnect is.
                            canClose: model.sessions.count > 1 || entry.element.db != nil,
                            select: { model.selectSession(entry.offset) },
                            close: { model.closeSession(entry.offset) })
                    }
                    // Inside the scroller, after the last tab, for the reason
                    // the query buffer strip records: a horizontal `ScrollView`
                    // takes all the width it is offered, so a button beside it
                    // lands at the window's far edge and reads as window chrome
                    // rather than as the end of the strip.
                    Button {
                        model.presentConnection()
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Theme.textSecondary.color)
                            .frame(width: 28, height: 32)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .disabled(model.isConnecting)
                    .help("Open another connection (⌘K)")
                    .accessibilityLabel("Open another connection")
                }
            }
        }
        .frame(height: 32)
        .background(Theme.background.color)
        .overlay(alignment: .bottom) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
    }
}

/// One connection's tab: the three marks the toolbar chip carried — colour,
/// state, name — now in the control that also switches between them.
///
/// Active: the raised surface with the selection accent along its top edge.
/// Inactive: the canvas surface, secondary text. The ✕ appears on hover, so a
/// row of connections reads as a row of names.
private struct ConnectionTabItem: View {
    let session: Session
    let isActive: Bool
    let canClose: Bool
    let select: () -> Void
    let close: () -> Void
    @State private var isHovering = false

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            // Absent rather than grey when no colour was picked: a bar on every
            // tab would train the eye to stop seeing the one that means
            // something. The rule the chip already followed.
            if let tone = session.connectionColor.tone {
                RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                    .fill(tone.color)
                    .frame(width: 3, height: 12)
            }
            StatusDot(state: session.connectionState)
            Text(session.connectionLabel)
                .font(Theme.Typography.caption)
                .foregroundStyle(isActive ? Theme.text.color : Theme.textSecondary.color)
                .lineLimit(1)
            if isHovering && canClose {
                Button(action: close) {
                    Image(systemName: "xmark")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(Theme.textTertiary.color)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close \(session.connectionLabel)")
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 32)
        // `ignoresSafeAreaEdges` defaults to `.all`, and this strip sits under
        // the titlebar — so an active tab's raised surface fills the band above
        // it as well, which is the defect the query buffer strip records.
        .background(
            isActive ? Theme.surface.color : Theme.background.color, ignoresSafeAreaEdges: []
        )
        .overlay(alignment: .top) {
            if isActive {
                Rectangle().fill(Theme.accent.color).frame(height: 2)
            }
        }
        .contentShape(Rectangle())
        .onTapGesture(perform: select)
        .onHover { isHovering = $0 }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "Connection \(session.connectionLabel), \(session.connectionState.label)"
        )
        .accessibilityAddTraits(isActive ? [.isSelected] : [])
    }
}
