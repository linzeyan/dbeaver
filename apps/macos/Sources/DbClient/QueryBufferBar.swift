import SwiftUI

/// The buffers open in the Query pane, along its top edge.
///
/// It lives inside the pane rather than above the three view tabs, which is
/// where it started. A strip up there was the window's second row of tabs, and
/// the two rows meant different things — one named places to write SQL, the
/// other named which face of the selected table was showing — so a reader had
/// to work out which row was which before either could be used. Down here it is
/// furniture of the one pane it acts on, and the strip at the top of the window
/// is unambiguously the connections.
///
/// Underlined rather than capped with the accent, for the same reason: the mark
/// points down at the editor the buffer belongs to, and does not repeat the
/// shape `ConnectionTabBar` uses for the window's own tabs.
struct QueryBufferBar: View {
    var model: AppModel

    var body: some View {
        HStack(spacing: 0) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 1) {
                    ForEach(Array(model.queryBuffers.enumerated()), id: \.element.id) { entry in
                        QueryBufferItem(
                            title: entry.element.name,
                            isActive: entry.offset == model.activeQueryBufferIndex,
                            canClose: model.queryBuffers.count > 1,
                            select: { model.selectQueryBuffer(entry.offset) },
                            close: { model.closeQueryBuffer(entry.offset) })
                    }
                    // Inside the scroller, after the last buffer, rather than
                    // pinned to the right of the strip. A horizontal
                    // `ScrollView` takes all the width it is offered, so a
                    // button beside it lands at the far edge of the pane — a
                    // hand's travel from the tabs it adds to. The cost is that
                    // enough buffers scroll it off; ⌘T is the answer then, and
                    // it is in the File menu saying so.
                    Button {
                        model.addQueryBuffer()
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 10, weight: .medium))
                            .foregroundStyle(Theme.textSecondary.color)
                            .frame(width: 24, height: 26)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("New query buffer (⌘T)")
                    .accessibilityLabel("New query buffer")
                }
            }
        }
        .frame(height: 26)
        .background(Theme.background.color)
        .overlay(alignment: .bottom) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
    }
}

/// One buffer. The active one is named in the primary tone and underlined; the
/// rest are secondary text on the same surface.
///
/// The close button exists only on hover, so a strip of buffers reads as a row
/// of names rather than as a row of ✕s — and the last one has none at all,
/// since an editor with nowhere to type is not a state this window has.
private struct QueryBufferItem: View {
    let title: String
    let isActive: Bool
    let canClose: Bool
    let select: () -> Void
    let close: () -> Void
    @State private var isHovering = false

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
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
        .padding(.horizontal, Theme.Space.sm + 2)
        .frame(height: 26)
        .contentShape(Rectangle())
        .overlay(alignment: .bottom) {
            if isActive {
                Rectangle().fill(Theme.accent.color).frame(height: 2)
            }
        }
        .onTapGesture(perform: select)
        .onHover { isHovering = $0 }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(title)
        .accessibilityAddTraits(isActive ? [.isSelected] : [])
    }
}
