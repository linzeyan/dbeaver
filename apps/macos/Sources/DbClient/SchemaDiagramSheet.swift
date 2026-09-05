import SwiftUI

/// One schema's tables and the keys between them.
///
/// A sheet rather than a fourth view tab or a window of its own. The three view
/// tabs are the same three for every family and answer "which side of the
/// relation in front am I looking at" (spec §5.1); a diagram is about a schema,
/// not about the selected relation, and adding a tab for it would put a control
/// on screen that is empty for most of what the sidebar can select. A window of
/// its own would be a second place a connection lives, which is the thing the tab
/// strip exists to prevent. So it goes where the other two schema-wide, read-only
/// answers already are: opened from the Database menu, over the window it is
/// about, closed when the question has been answered.
///
/// The canvas is drawn in two layers on purpose. The lines are a `Canvas`, which
/// is a picture and says nothing to a screen reader; the boxes are ordinary views
/// with labels, and each one reads out the lines attached to it
/// (`SchemaDiagram.spoken`). Drawing the boxes into the canvas too would have
/// been fewer parts and an unreadable diagram for anybody not looking at it.
struct SchemaDiagramSheet: View {
    @Bindable var model: AppModel

    private var diagram: SchemaDiagram? { model.schemaDiagram }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            content(of: diagram)
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        // Wider and taller than the sheets that are read as a list: this one is
        // read as a picture, and a picture of six tables that needs scrolling in
        // both directions is a picture of one table at a time.
        .frame(width: 900, height: 560)
        .background(Theme.Surface.raised.color)
        .onExitCommand { model.closeSchemaDiagram() }
    }

    /// The connection, and which of its schemas is drawn.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.connectionLabel)
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .frame(maxWidth: 160, alignment: .leading)

            Picker("", selection: $model.schemaDiagramSchema) {
                ForEach(model.schemas) { schema in
                    Text(schema.name).tag(schema.name)
                }
            }
            .labelsHidden()
            .frame(width: 160)
            // The picker is the whole of "draw a different one": changing it is
            // the request, so there is no second control to press afterwards.
            .onChange(of: model.schemaDiagramSchema) { model.drawSchemaDiagram() }
            .accessibilityLabel("Schema to draw")

            Spacer(minLength: Theme.Space.sm)

            Text("Keys only — a box lists the columns a key touches")
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
                .lineLimit(1)
        }
        .padding(Theme.Space.md)
    }

    /// The picture, or which of the three things that are not one this is.
    @ViewBuilder
    private func content(of diagram: SchemaDiagram?) -> some View {
        if model.isDrawingSchemaDiagram {
            // Being read is not the same as having nothing in it — the lesson
            // the six Structure sections taught (ui-review §二). It also says
            // what it is doing, because on a wide schema this is one round trip
            // per table and there is nothing to poll.
            waiting(
                "Reading \(model.schemaDiagramSchema)…",
                "Every table in the schema is asked for its foreign keys, one at "
                    + "a time. On a large schema, or a server that is not nearby, "
                    + "this takes a while.")
        } else if let diagram {
            if diagram.isEmpty {
                waiting("No relationships in \(diagram.schema).", diagram.summary)
            } else {
                canvas(diagram)
            }
        } else {
            waiting("Nothing drawn yet.", "Choose a schema.")
        }
    }

    private func waiting(_ title: String, _ detail: String) -> some View {
        VStack(spacing: Theme.Space.xs) {
            Text(title)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.Text.secondary.color)
            Text(detail)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 460)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func canvas(_ diagram: SchemaDiagram) -> some View {
        ScrollView([.horizontal, .vertical]) {
            ZStack(alignment: .topLeading) {
                // Sized explicitly rather than left to fill the stack: a canvas
                // takes the size it is proposed, and the proposal here is made by
                // the boxes — which are offset rather than laid out, so the stack
                // is as big as one box and every line would be clipped to it.
                lines(diagram)
                    .frame(width: diagram.canvas.width, height: diagram.canvas.height)
                ForEach(diagram.tables) { table in
                    box(table, in: diagram)
                        .frame(width: SchemaDiagram.boxWidth, height: table.height)
                        .offset(x: table.x, y: table.y)
                }
            }
            .frame(
                width: diagram.canvas.width, height: diagram.canvas.height, alignment: .topLeading)
        }
        .background(Theme.Surface.canvas.color)
    }

    /// The keys, under the boxes they join.
    ///
    /// One `Canvas` for all of them rather than a shape per edge: a line is two
    /// points and a colour, and a hundred views to hold that is a hundred views
    /// SwiftUI lays out every time the sheet is resized.
    private func lines(_ diagram: SchemaDiagram) -> some View {
        Canvas { context, _ in
            for edge in diagram.edges {
                guard let from = diagram.table(named: edge.from),
                    let to = diagram.table(named: edge.to)
                else { continue }
                if edge.isSelfReference {
                    context.stroke(
                        Path(ellipseIn: loop(around: from)), with: .color(stroke), lineWidth: 1)
                    continue
                }
                let (start, end) = SchemaDiagram.link(from: from, to: to)
                var path = Path()
                path.move(to: start)
                path.addLine(to: end)
                context.stroke(path, with: .color(stroke), lineWidth: 1)
                // A dot on the referenced end and nothing on the other: the fact
                // this side has is direction — which table declares the key —
                // and not cardinality. Drawing a crow's foot would claim the
                // referencing column is not unique, which is a question about
                // that table's indexes and was never asked.
                context.fill(
                    Path(ellipseIn: CGRect(x: end.x - 3, y: end.y - 3, width: 6, height: 6)),
                    with: .color(stroke))
            }
        }
        .accessibilityHidden(true)
    }

    private var stroke: Color { Theme.Accent.selection.opacity(0.65).color }

    /// Where a key that points at its own table is drawn: a small circle off the
    /// box's right edge, which is a shape and not a line of zero length.
    private func loop(around table: SchemaDiagram.Table) -> CGRect {
        CGRect(x: table.frame.maxX - 8, y: table.frame.midY - 8, width: 22, height: 16)
    }

    private func box(_ table: SchemaDiagram.Table, in diagram: SchemaDiagram) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(table.name)
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .truncationMode(.middle)
                .padding(.horizontal, Theme.Space.sm)
                .frame(height: SchemaDiagram.headerHeight, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.Surface.overlay.color)

            VStack(alignment: .leading, spacing: 0) {
                ForEach(table.columns, id: \.self) { column in
                    Text(column)
                        .font(Theme.Typography.monoSmall)
                        .foregroundStyle(Theme.Text.secondary.color)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(height: SchemaDiagram.rowHeight, alignment: .leading)
                }
                if table.hiddenColumns > 0 {
                    Text("+\(table.hiddenColumns) more")
                        .font(Theme.Typography.micro)
                        .foregroundStyle(Theme.Text.tertiary.color)
                        .frame(height: SchemaDiagram.rowHeight, alignment: .leading)
                }
            }
            .padding(.horizontal, Theme.Space.sm)
            .padding(.vertical, SchemaDiagram.boxPadding)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Theme.Surface.raised.color)
        .overlay(
            RoundedRectangle(cornerRadius: Theme.Radius.card)
                .stroke(Theme.Border.control.color, lineWidth: 1)
        )
        .clipShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(diagram.spoken(for: table))
    }

    /// What the picture leaves out, and the way off it.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            if let diagram, !model.isDrawingSchemaDiagram {
                Text(diagram.summary)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.Space.sm)
            Button("Done") { model.closeSchemaDiagram() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
    }
}
