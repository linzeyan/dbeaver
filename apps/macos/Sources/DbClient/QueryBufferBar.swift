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
                            isRenaming: model.renamingQueryBuffer == entry.element.id,
                            select: { model.selectQueryBuffer(entry.offset) },
                            close: { model.closeQueryBuffer(entry.offset) },
                            beginRename: { model.renamingQueryBuffer = entry.element.id },
                            commitRename: { name in
                                model.renameQueryBuffer(entry.offset, to: name)
                                model.renamingQueryBuffer = nil
                            },
                            cancelRename: { model.renamingQueryBuffer = nil })
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
                            .foregroundStyle(Theme.Text.secondary.color)
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
        .background(Theme.Surface.canvas.color)
        .overlay(alignment: .bottom) {
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
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
    let isRenaming: Bool
    let select: () -> Void
    let close: () -> Void
    let beginRename: () -> Void
    let commitRename: (String) -> Void
    let cancelRename: () -> Void
    @State private var isHovering = false
    /// The name being typed. Seeded when the field appears rather than bound to
    /// the buffer, so Escape has something to abandon: a binding straight to the
    /// model would have renamed the buffer on every keystroke and left cancel
    /// with nothing to undo.
    @State private var draft = ""
    /// Set by Escape, read by the focus-loss handler below. Without it the two
    /// paths fight: Escape puts the field away, putting it away drops focus, and
    /// the focus-loss commit would write back the text Escape just abandoned.
    @State private var isAbandoning = false
    @FocusState private var isEditing: Bool

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            if isRenaming {
                TextField("", text: $draft)
                    .textFieldStyle(.plain)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(Theme.Text.primary.color)
                    .focused($isEditing)
                    .frame(minWidth: 44)
                    .fixedSize()
                    // A box and a ring, because a plain field in this strip is
                    // indistinguishable from the name it replaced — the first
                    // capture showed it legible only while the seeded text was
                    // still selected, which is to say only until the first
                    // keystroke. The ring is the accent rather than the control
                    // border: this field has the keyboard, and the accent is
                    // what says so everywhere else in the window.
                    .padding(.horizontal, 4)
                    .padding(.vertical, 1)
                    .background(
                        RoundedRectangle(cornerRadius: Theme.Radius.control)
                            .fill(Theme.Surface.raised.color)
                            .overlay(
                                RoundedRectangle(cornerRadius: Theme.Radius.control)
                                    .strokeBorder(Theme.Accent.selection.color, lineWidth: 1))
                    )
                    .onSubmit { commitRename(draft) }
                    .onExitCommand {
                        isAbandoning = true
                        cancelRename()
                    }
                    // Clicking away is a commit, not a cancel. The field is
                    // opened by a double-click on a name somebody meant to
                    // change, and losing the typing to a stray click elsewhere
                    // in the window is the worse of the two mistakes.
                    .onChange(of: isEditing) { _, focused in
                        guard !focused else { return }
                        if isAbandoning {
                            isAbandoning = false
                        } else {
                            commitRename(draft)
                        }
                    }
                    .task {
                        draft = title
                        isEditing = true
                    }
            } else {
                Text(title)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(
                        isActive ? Theme.Text.primary.color : Theme.Text.secondary.color
                    )
                    .lineLimit(1)
            }
            if isHovering && canClose && !isRenaming {
                Button(action: close) {
                    Image(systemName: "xmark")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(Theme.Text.tertiary.color)
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
                Rectangle().fill(Theme.Accent.selection.color).frame(height: 2)
            }
        }
        .onTapGesture(perform: select)
        // Simultaneous rather than a second `onTapGesture`, which is the shape
        // `ConnectionRow` already uses for select-then-open: the single tap is
        // not wasted work, because renaming the buffer you are not looking at
        // and then looking at it is two gestures for one intent.
        .simultaneousGesture(TapGesture(count: 2).onEnded(beginRename))
        .onHover { isHovering = $0 }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(title)
        .accessibilityAddTraits(isActive ? [.isSelected] : [])
    }
}
