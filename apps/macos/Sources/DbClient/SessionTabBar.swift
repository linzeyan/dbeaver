import SwiftUI

/// The strip of session tabs across the top of the detail column. One tab per
/// query buffer; selecting one lands on the Query view of that buffer.
///
/// Its own view rather than part of `DetailPane` because of what joins it
/// later: object tabs share this strip, so the strip is a surface in its own
/// right rather than a row of the pane below it.
struct SessionTabBar: View {
    var model: AppModel

    var body: some View {
        HStack(spacing: 0) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 1) {
                    ForEach(Array(model.queryBuffers.enumerated()), id: \.element.id) { entry in
                        SessionTabItem(
                            title: entry.element.name,
                            isActive: entry.offset == model.activeQueryBufferIndex
                                && model.activeTab == .query,
                            canClose: model.queryBuffers.count > 1,
                            select: {
                                model.selectQueryBuffer(entry.offset)
                                model.activeTab = .query
                            },
                            close: { model.closeQueryBuffer(entry.offset) })
                    }
                    // Inside the scroller, after the last tab, rather than
                    // pinned to the right of the strip. A horizontal
                    // `ScrollView` takes all the width it is offered, so a
                    // button beside it lands at the far edge of the window —
                    // a hand's travel from the tabs it adds to, and reading
                    // as a window control rather than as part of the strip.
                    // The cost is that enough tabs scroll it off; ⌘T is the
                    // answer then, and it is in the File menu saying so.
                    Button {
                        model.addQueryBuffer()
                        model.activeTab = .query
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Theme.textSecondary.color)
                            .frame(width: 28, height: 32)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("New query tab (⌘T)")
                    .accessibilityLabel("New query tab")
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

/// One tab. Active: the raised surface with the selection accent along its top
/// edge. Inactive: the canvas surface, secondary text.
///
/// The close button exists only on hover, so a strip of tabs reads as a row of
/// names rather than as a row of ✕s — and the last tab has none at all, since
/// an editor with nowhere to type is not a state this window has.
private struct SessionTabItem: View {
    let title: String
    let isActive: Bool
    let canClose: Bool
    let select: () -> Void
    let close: () -> Void
    @State private var isHovering = false

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            Image(systemName: "chevron.left.forwardslash.chevron.right")
                .font(.system(size: 9))
                .foregroundStyle(Theme.textTertiary.color)
            Text(title)
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
                .accessibilityLabel("Close \(title)")
            }
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 32)
        // `ignoresSafeAreaEdges` defaults to `.all`, and this strip sits directly
        // under the titlebar — so the active tab's raised surface was filling the
        // 52pt above it as well, putting a lighter patch behind the connection
        // chip that appeared only while the Query view was up. The fill belongs
        // to the tab, not to the window's top edge.
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
        .accessibilityLabel(title)
        .accessibilityAddTraits(isActive ? [.isSelected] : [])
    }
}
