import SwiftUI

/// The object tree with the tree taken out.
///
/// What survives at this width is the sidebar's own chrome, in the places it
/// already occupies: the filter at the top where the field is, the count and
/// Refresh at the bottom where the footer is. Nothing of the tree itself is
/// here, and nothing of it could be — a column of schema and relation rows
/// squeezed to 44pt is a column of identical glyphs, and the names are the
/// entire content of those rows.
///
/// So this is not a smaller navigator. It is the two commands that would
/// otherwise leave the window with the tree, kept where the hand already
/// reaches for them, in exchange for the width of a table name.
struct SidebarRail: View {
    var model: AppModel

    /// Wide enough for a 16pt glyph in a 24pt hit area with the sidebar's own
    /// margin either side, and no wider: every point here is one the pane on the
    /// right does not get, which is the only reason the rail exists.
    static let width: CGFloat = 44

    var body: some View {
        VStack(spacing: 0) {
            RailButton(
                symbol: "magnifyingglass",
                help: "Filter the objects (⌥⌘F)",
                label: "Filter objects",
                isEnabled: model.canFilterObjects,
                action: model.focusNavigatorFilter
            )
            // The same glyph the filter field wears, at the same height as the
            // field it stands in for: this button is that field, folded up.
            .padding(.top, Theme.Space.sm)

            Spacer(minLength: 0)

            footer
        }
        // Filling the column rather than measuring a width of its own: the width
        // is the column's, set where the column is chosen, and a frame here as
        // well would be a second place for it to be decided.
        //
        // What the system does with that width is worth knowing before reading
        // a picture of this: it insets the column's content and clips it to a
        // rounded shape, so 44pt of rail costs about 64pt of window and the
        // corners are a visible curve rather than a hint of one. The tree does
        // the same thing — at 250pt the same curve is a detail, and at 44pt it
        // is most of the outline. Both are the platform's, not this file's.
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // `surface` rather than the `background` the tree wears, which is the
        // one place this column deliberately differs from the one it replaces.
        // The tree is content and takes the canvas tone; a rail is chrome, and
        // on the canvas tone it disappeared — 44pt of exactly the colour of the
        // pane beside it, with two glyphs apparently floating in the grid. The
        // raised tone is what every other strip of controls in this window is
        // drawn on.
        //
        // Still switched off by the translucency setting, for the reason
        // `NavigatorView` records at length: with it on, the detail column's
        // bands are sampled through this column too, and a rail is narrow enough
        // that one stripe is most of what is on it.
        .background(
            model.preferences.usesTranslucentSidebar ? Color.clear : Theme.surface.color
        )
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Objects, collapsed")
    }

    /// The sidebar footer, stacked instead of laid out across. The count over
    /// the button rather than beside it because there is no beside — and in that
    /// order, so the two sit at the same heights they do when the tree is open.
    private var footer: some View {
        VStack(spacing: 2) {
            Text(AppModel.formatted(model.matchedRelationCount))
                .font(Theme.Typography.digits)
                .foregroundStyle(Theme.textTertiary.color)
                .lineLimit(1)
                // A five-figure table count is wider than this column. Shrinking
                // is the right failure: the alternative is "12,3…", which is not
                // a number.
                .minimumScaleFactor(0.6)
                .help(countHelp)
                .accessibilityLabel(countHelp)

            RailButton(
                symbol: "arrow.clockwise",
                help: "Reload \(model.containerNoun)s and objects from the database (⇧⌘R)",
                label: "Refresh objects",
                isEnabled: model.canRefresh,
                action: model.refresh)
        }
        .padding(.vertical, Theme.Space.xs + 2)
        .frame(maxWidth: .infinity)
        // No fill of its own: the rail is already the tone the sidebar footer
        // has, so the rule is the whole of what marks this off as the footer.
        .overlay(alignment: .top) {
            Rectangle().fill(Theme.separator.color).frame(height: 1)
        }
    }

    /// The words the footer has room for and the rail does not. A bare figure in
    /// a 44pt column says how many of something, and this is the only place left
    /// to say of what — and to carry the "from last time" the tree's dimming
    /// stands for, which no amount of dimming a number would say.
    private var countHelp: String {
        let counted = AppModel.pluralized(model.matchedRelationCount, "object")
        return model.isTreeStale ? "\(counted), from the last time this was open" : counted
    }
}

/// One command in the rail: a glyph in a square, dimmed when it cannot run.
///
/// Coloured rather than left to the button style's own dimming, which is the
/// same choice the sidebar footer's Refresh makes and for the same reason — a
/// 10pt glyph at the system's disabled alpha is invisible against this
/// background rather than merely quiet.
private struct RailButton: View {
    let symbol: String
    let help: String
    let label: String
    let isEnabled: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 12, weight: .medium))
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // Focus drawn by nobody, which is what every other button in this window
        // does and what the rail in particular needs: it holds the first
        // focusable control in the column, so SwiftUI parks focus here whenever
        // the pane on the right does not claim it — and the system's indicator
        // is a filled rounded rectangle that, at this size, reads as a control
        // stuck in its pressed state. The first capture of this stage was one.
        .focusEffectDisabled()
        .foregroundStyle(isEnabled ? Theme.textSecondary.color : Theme.textTertiary.color)
        .disabled(!isEnabled)
        .help(help)
        .accessibilityLabel(label)
    }
}
