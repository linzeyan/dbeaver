import SwiftUI

/// Two schemas side by side, and what they do not agree about.
///
/// A table rather than a palette, for the reason `ProcessesSheet` is one: the
/// answer is not a name to pick but a list to read across — which object, in
/// which relation, and what each side says about it. The two rightmost columns
/// are the whole report; everything to their left is there to find the row.
///
/// The pair is chosen in the sheet rather than before it. A comparison is asked
/// for twice as often as it is set up right the first time — the wrong schema on
/// the right is the ordinary mistake — and a picker that closed itself before
/// showing anything would make correcting that a trip back through the menu.
struct SchemaDiffSheet: View {
    @Bindable var model: AppModel

    private var choices: [ConnectionChoice] { model.schemaDiffChoices }
    /// The model's reading rather than this view's, so the connection named
    /// above the table and the one Compare would ask cannot be two connections.
    private var target: ConnectionChoice? { model.schemaDiffChoice }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            report(model.schemaComparison)
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        // Wider than the sheets that ask a question, because this one is read:
        // six columns, two of which hold a database's own words about a column
        // and are as long as those words happen to be.
        .frame(width: 860, height: 480)
        .background(Theme.Surface.raised.color)
        .onExitCommand { model.closeSchemaDiff() }
    }

    /// The pair, read as a sentence: this connection and its schema, then the
    /// one it is being compared with and its.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.connectionLabel)
                .font(Theme.Typography.captionEmphasis)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .frame(maxWidth: 130, alignment: .leading)
            schemaPicker(
                model.schemas, selection: $model.schemaDiffLeftSchema,
                spoken: "Schema on this connection")

            Text("with")
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.tertiary.color)

            Picker("", selection: $model.schemaDiffTarget) {
                ForEach(choices) { choice in
                    Text(choice.label).tag(Optional(choice.id))
                }
            }
            .labelsHidden()
            .frame(width: 170)
            .onChange(of: model.schemaDiffTarget) { model.schemaDiffTargetChanged() }
            .accessibilityLabel("Connection to compare with")
            schemaPicker(
                target?.session.schemas ?? [], selection: $model.schemaDiffRightSchema,
                spoken: "Schema on the other connection")

            Spacer(minLength: Theme.Space.sm)

            Button("Compare") { model.compareSchemas() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                // Greyed rather than merely inert, for the reason the kill
                // buttons are: a plain button given an explicit colour keeps it
                // through `disabled`, and one that looks pressable while a
                // comparison is already running reads as a button that is broken.
                .foregroundStyle(armed ? Theme.Accent.selection.color : Theme.Text.tertiary.color)
                .disabled(!armed)
        }
        .padding(Theme.Space.md)
    }

    private var armed: Bool {
        !model.isComparingSchemas && !model.schemaDiffLeftSchema.isEmpty
            && !model.schemaDiffRightSchema.isEmpty
    }

    private func schemaPicker(
        _ schemas: [SchemaInfo], selection: Binding<String>, spoken: String
    ) -> some View {
        Picker("", selection: selection) {
            ForEach(schemas) { schema in
                Text(schema.name).tag(schema.name)
            }
        }
        .labelsHidden()
        .frame(width: 140)
        .accessibilityLabel(spoken)
    }

    @ViewBuilder
    private func report(_ comparison: SchemaComparison?) -> some View {
        if model.isComparingSchemas {
            // What it is doing and why it may sit there. A comparison is several
            // hundred round trips on a schema of any size, there is nothing to
            // poll, and a panel that only said "Comparing…" would leave somebody
            // deciding at thirty seconds whether the application had stopped.
            waiting(
                "Comparing…",
                "Every relation on both sides is read in turn. On a large schema, "
                    + "or a server that is not nearby, this takes a while.")
        } else if let comparison {
            if comparison.report.differences.isEmpty {
                waiting(
                    "No differences.",
                    comparison.report.summary(left: comparison.left, right: comparison.right))
            } else {
                table(comparison)
            }
        } else {
            waiting(
                "Nothing compared yet.",
                "Choose a schema on each side and press Compare.")
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
                .frame(maxWidth: 420)
        }
        .padding(Theme.Space.md)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func table(_ comparison: SchemaComparison) -> some View {
        VStack(spacing: 0) {
            headings(comparison)
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(comparison.report.differences) { difference in
                        row(difference, in: comparison)
                    }
                }
            }
        }
    }

    /// The two rightmost headings are the pair itself, qualified by schema —
    /// which is what says which column the ◀ is pointing at.
    private func headings(_ comparison: SchemaComparison) -> some View {
        HStack(spacing: Theme.Space.sm) {
            Color.clear.frame(width: Self.markerWidth)
            heading("Relation").frame(width: Self.tableWidth, alignment: .leading)
            heading("Object").frame(width: Self.objectWidth, alignment: .leading)
            heading("Kind").frame(width: Self.kindWidth, alignment: .leading)
            heading(comparison.left).frame(maxWidth: .infinity, alignment: .leading)
            heading(comparison.right).frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 20)
        .accessibilityHidden(true)
    }

    /// Truncated in the middle, unlike every cell under it: a heading here is
    /// `connection.schema`, and the half that says which schema is the half a
    /// tail truncation would eat.
    private func heading(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.micro)
            .foregroundStyle(Theme.Text.tertiary.color)
            .lineLimit(1)
            .truncationMode(.middle)
    }

    private func row(_ difference: SchemaDifference, in comparison: SchemaComparison) -> some View {
        // Top-aligned, because the two side cells are the only ones that can run
        // to a second line and the four to their left would otherwise drift down
        // half a line beside them.
        HStack(alignment: .top, spacing: Theme.Space.sm) {
            Text(difference.marker)
                .font(Theme.Typography.micro)
                .foregroundStyle(Theme.Text.tertiary.color)
                .frame(width: Self.markerWidth, alignment: .leading)
            cell(difference.table, width: Self.tableWidth, tone: Theme.Text.secondary)
            cell(difference.object, width: Self.objectWidth, tone: Theme.Text.primary)
            cell(difference.word, width: Self.kindWidth, tone: Theme.Text.tertiary)
            // The two sides in the font a type or an expression is written in:
            // these are a database's own words, and `varchar(64) not null` lines
            // up against `varchar(32) not null` only in a monospaced one.
            side(difference.leftCell)
            side(difference.rightCell)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(difference.spoken(left: comparison.left, right: comparison.right))
    }

    private func cell(_ text: String, width: CGFloat, tone: Theme.Tone) -> some View {
        Text(text)
            .font(Theme.Typography.caption)
            .foregroundStyle(tone.color)
            .lineLimit(1)
            .truncationMode(.tail)
            .frame(width: width, alignment: .leading)
    }

    /// One side's own words about the object.
    ///
    /// Two lines rather than one, and this is the column where that matters:
    /// what differs between two sides is most often the tail — `not null`, a
    /// default, `unique` — and a single truncated line puts the two descriptions
    /// on screen looking identical, which is the one thing this table exists not
    /// to do. The tooltip is still there for the third line nobody expected.
    private func side(_ text: String) -> some View {
        Text(text)
            .font(Theme.Typography.monoSmall)
            .foregroundStyle(
                text == SchemaDifference.absent
                    ? Theme.Text.tertiary.color : Theme.Text.secondary.color
            )
            .lineLimit(2)
            .truncationMode(.tail)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .help(text)
    }

    /// The sentence, and the way out.
    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            if let comparison = model.schemaComparison {
                Text(comparison.report.summary(left: comparison.left, right: comparison.right))
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.tertiary.color)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.Space.sm)
            Button("Done") { model.closeSchemaDiff() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
    }

    private static let markerWidth: CGFloat = 14
    private static let tableWidth: CGFloat = 120
    private static let objectWidth: CGFloat = 130
    private static let kindWidth: CGFloat = 84
}
