import CoreGraphics
import Foundation

/// A schema's foreign keys, laid out as boxes and lines.
///
/// The whole of the picture is composed here, away from the view: which tables
/// get a box, which keys get a line, where each box goes, and what is left out.
/// That is what makes a diagram checkable — `--verify-schema-diagram` asserts
/// rules about this value, and nothing about pixels.
///
/// **Keys only, and deliberately.** A box lists the columns a key touches and
/// nothing else. That is a real ERD notation — upstream's plugin calls it
/// "Attributes: keys" and dbx's inspector defaults near it — and here it is also
/// what keeps the cost honest: the diagram is built from `foreign_keys` alone,
/// so a schema of three hundred tables costs three hundred key lists and not a
/// single column list. A version that showed every column would fetch the whole
/// shape of a database to draw table names in it.
///
/// **What is held.** The snapshot is proportional to the keys, not to the
/// schema: at most `tableCap` boxes, each carrying at most `columnCap` column
/// names, plus one `Edge` per key drawn. The raw answers are dropped as soon as
/// this is built, and the whole value is dropped when the sheet closes — see
/// `AppModel.closeSchemaDiagram`. Nothing about a diagram outlives the sheet
/// that asked for it, because a picture is a snapshot of metadata that is stale
/// the moment somebody runs a migration.
struct SchemaDiagram: Sendable, Equatable {
    /// What one relation answered when asked for its foreign keys.
    ///
    /// The relation's own name travels with the answer because a
    /// `RelationshipInfo` names only the other side: read from the referencing
    /// table, "local" is this table, and which table that was is the caller's
    /// knowledge.
    struct TableKeys: Sendable, Equatable {
        let table: String
        let keys: [RelationshipInfo]
    }

    /// One table, and where it sits.
    struct Table: Identifiable, Sendable, Equatable {
        let name: String
        /// The columns this table's keys touch — its own foreign key columns and
        /// the columns other tables point at — in the order the keys were read.
        /// Not every column of the table; see the type note.
        let columns: [String]
        /// How many more the keys touch than the box has room for. Zero for
        /// nearly every table: a box that lists eight names is already an
        /// unusual table, and a box that lists forty is a wall.
        let hiddenColumns: Int
        let x: CGFloat
        let y: CGFloat

        var id: String { name }

        var height: CGFloat {
            let rows = columns.count + (hiddenColumns > 0 ? 1 : 0)
            return SchemaDiagram.headerHeight + SchemaDiagram.boxPadding * 2
                + CGFloat(rows) * SchemaDiagram.rowHeight
        }

        var frame: CGRect {
            CGRect(x: x, y: y, width: SchemaDiagram.boxWidth, height: height)
        }
    }

    /// One foreign key, pointing from the table that declares it at the table it
    /// names.
    ///
    /// Read from the referencing side only. `referenced_by` is deliberately not
    /// asked: the same constraint answers from both ends, and asking both would
    /// draw every relationship twice and double the round trips to do it.
    struct Edge: Identifiable, Sendable, Equatable {
        let name: String
        let from: String
        let fromColumns: [String]
        let to: String
        let toColumns: [String]

        /// A key a table declares against itself — a parent id, a manager. One
        /// box and a line that starts and ends on it, which the view draws as a
        /// loop rather than as a zero-length segment.
        var isSelfReference: Bool { from == to }

        var id: String { "\(from).\(name).\(to)" }

        /// `customer_id → customer(id)`, which is the line read aloud.
        var label: String {
            "\(fromColumns.joined(separator: ", ")) → \(to)"
                + "(\(toColumns.joined(separator: ", ")))"
        }
    }

    let schema: String
    /// The boxes, in layout order.
    let tables: [Table]
    /// The lines. Every one of them has both of its boxes on the diagram.
    let edges: [Edge]
    /// How many relations were asked for their keys. The denominator of "two of
    /// forty tables are related", which is the sentence that stops an almost
    /// empty diagram reading as a failed read.
    let asked: Int
    /// How many tables take part in a key at all, before the cap.
    let related: Int
    /// Keys whose other side is not on this diagram: another schema, or a name
    /// this schema did not list. Counted rather than drawn — a box for a table
    /// from somewhere else would be a diagram claiming to be one schema and
    /// showing another.
    let outside: Int
    /// Keys dropped because one of their tables fell past `tableCap`.
    let undrawn: Int

    // MARK: - Limits

    /// How many boxes a diagram draws.
    ///
    /// A legibility limit, not a memory one — the keys of a whole schema are a
    /// few hundred short strings, while sixty boxes is already more than fits on
    /// a screen without scrolling in both directions. Past this the picture stops
    /// answering the question it was opened for, so it stops and says how many
    /// tables it left out. Narrowing it is what the schema picker is for.
    static let tableCap = 60

    /// How many column names one box lists.
    static let columnCap = 8

    // MARK: - Metrics
    //
    // In the model rather than in the view because the layout rule is what the
    // checks assert: boxes that do not overlap, and a component whose tables end
    // up beside each other. A rule expressed in view code is a rule nothing can
    // be wrong about until somebody looks at a screenshot.

    static let boxWidth: CGFloat = 200
    static let headerHeight: CGFloat = 24
    static let rowHeight: CGFloat = 15
    static let boxPadding: CGFloat = 6
    static let gapX: CGFloat = 48
    static let gapY: CGFloat = 32
    static let margin: CGFloat = 24

    /// The relations worth a round trip.
    ///
    /// Views and materialized views are skipped, and this is where the cost of a
    /// big schema is actually decided: a warehouse schema is mostly views, and a
    /// view can be on neither end of a foreign key — it declares none, and
    /// nothing may point at one, because the referenced side of a key has to
    /// carry a unique constraint. Skipping them cannot lose a line.
    static func asks(_ relations: [RelationInfo]) -> [RelationInfo] {
        relations.filter { $0.kind != .view && $0.kind != .materializedView }
    }

    /// Builds the picture from what each relation said about its keys.
    ///
    /// `read` is in the order the navigator lists the schema, which is what makes
    /// everything below deterministic: the same schema draws the same diagram
    /// twice.
    static func of(schema: String, read: [TableKeys], cap: Int = tableCap) -> SchemaDiagram {
        let listed = read.map(\.table)
        let known = Set(listed)

        var edges: [Edge] = []
        var outside = 0
        for answer in read {
            for key in answer.keys {
                // An empty `other_schema` is not a foreign schema: the engines
                // with one level of container report the level and leave the
                // qualifier blank, and treating that as "elsewhere" would draw
                // an empty diagram for every SQLite file.
                let here = key.otherSchema.isEmpty || key.otherSchema == schema
                guard here, known.contains(key.otherTable) else {
                    outside += 1
                    continue
                }
                edges.append(
                    Edge(
                        name: key.name, from: answer.table, fromColumns: key.localColumns,
                        to: key.otherTable, toColumns: key.otherColumns))
            }
        }

        // Only tables a key reaches. On a schema where twelve tables of three
        // hundred are joined, the other 288 boxes are the whole reason nobody
        // can read the picture — and each of them says nothing that the sidebar
        // does not already say better.
        var columns: [String: [String]] = [:]
        for edge in edges {
            for column in edge.fromColumns { note(column, of: edge.from, into: &columns) }
            for column in edge.toColumns { note(column, of: edge.to, into: &columns) }
        }
        let participating = listed.filter { columns[$0] != nil }

        // Whole groups while they fit. Half a group is half a picture — the
        // tables left out are the ones the drawn half is joined to — so the cap
        // stops at a boundary and says how many tables it did not draw. The one
        // exception is a group bigger than the cap on its own: drawing nothing at
        // all for it would answer a schema of four hundred joined tables with an
        // empty canvas, so it is drawn as far as it goes.
        var drawn: [String] = []
        for group in grouped(participating, edges: edges) {
            if drawn.count + group.count <= cap {
                drawn += group
                continue
            }
            if drawn.isEmpty { drawn = Array(group.prefix(cap)) }
            break
        }
        let shown = Set(drawn)
        let kept = edges.filter { shown.contains($0.from) && shown.contains($0.to) }

        return SchemaDiagram(
            schema: schema,
            tables: laidOut(drawn, columns: columns),
            edges: kept,
            asked: read.count,
            related: participating.count,
            outside: outside,
            undrawn: edges.count - kept.count)
    }

    private static func note(
        _ column: String, of table: String, into columns: inout [String: [String]]
    ) {
        var have = columns[table] ?? []
        if !have.contains(column) { have.append(column) }
        columns[table] = have
    }

    /// The tables split into the groups the keys join them into, biggest first,
    /// each group ordered so that neighbours are next to each other.
    ///
    /// This is the whole of the layout intelligence, and deliberately all of it.
    /// A real layout engine minimises crossings; this only refuses to interleave
    /// two unrelated groups, which is where an alphabetical grid gets its worst
    /// result — `orders` in the top left and `order_line` six boxes down with
    /// eleven strangers between them and a line crossing all of it. Groups are
    /// ordered by size so that the cluster somebody opened the diagram for is the
    /// one at the top, and so that the cap takes the strays rather than the
    /// subject.
    ///
    /// Inside a group the order is breadth-first from its earliest table, which
    /// buys a second thing beyond adjacency: every table after the first is
    /// joined to one before it, so *any* prefix of a group is still a connected
    /// picture. That is what lets the cap cut an oversized group at an arbitrary
    /// point without leaving a box on the canvas with no line attached to it.
    private static func grouped(_ tables: [String], edges: [Edge]) -> [[String]] {
        let index = Dictionary(
            uniqueKeysWithValues: tables.enumerated().map { ($0.element, $0.offset) })

        // Union by the smaller index, so a group is named by its earliest table
        // and the order below is stable.
        var owner = Array(tables.indices)
        func find(_ start: Int) -> Int {
            var at = start
            while owner[at] != at {
                owner[at] = owner[owner[at]]
                at = owner[at]
            }
            return at
        }
        func union(_ one: Int, _ other: Int) {
            let (first, second) = (find(one), find(other))
            guard first != second else { return }
            if first < second { owner[second] = first } else { owner[first] = second }
        }

        var neighbours: [Int: [Int]] = [:]
        for edge in edges {
            guard let from = index[edge.from], let to = index[edge.to], from != to else { continue }
            neighbours[from, default: []].append(to)
            neighbours[to, default: []].append(from)
            union(from, to)
        }

        var members: [Int: [Int]] = [:]
        for position in tables.indices { members[find(position), default: []].append(position) }

        return
            members
            .sorted { left, right in
                left.value.count == right.value.count
                    ? left.key < right.key : left.value.count > right.value.count
            }
            .map { breadthFirst($0.value, neighbours: neighbours, tables: tables) }
    }

    private static func breadthFirst(
        _ group: [Int], neighbours: [Int: [Int]], tables: [String]
    ) -> [String] {
        var seen: Set<Int> = []
        var order: [Int] = []
        for start in group where !seen.contains(start) {
            seen.insert(start)
            var queue = [start]
            while !queue.isEmpty {
                let next = queue.removeFirst()
                order.append(next)
                // The schema's own order among a table's neighbours, so that a
                // diagram is the same picture twice.
                for near in (neighbours[next] ?? []).sorted() where !seen.contains(near) {
                    seen.insert(near)
                    queue.append(near)
                }
            }
        }
        return order.map { tables[$0] }
    }

    /// Boxes into a grid, in the order they arrive.
    ///
    /// Square-ish rather than one long row, which is dbx's rule and the right one
    /// for a canvas somebody scrolls in both directions: a row of forty boxes is
    /// a diagram that can only be read by scrolling past the lines that explain
    /// it. Each row is as tall as its tallest box, so two boxes never overlap
    /// however many key columns one of them turns out to have.
    private static func laidOut(_ tables: [String], columns: [String: [String]]) -> [Table] {
        guard !tables.isEmpty else { return [] }
        let perRow = max(1, Int(ceil(Double(tables.count).squareRoot())))
        var laid: [Table] = []
        var y = margin
        var start = 0
        while start < tables.count {
            let row = Array(tables[start..<min(start + perRow, tables.count)])
            var tallest: CGFloat = 0
            for (column, name) in row.enumerated() {
                let all = columns[name] ?? []
                let table = Table(
                    name: name,
                    columns: Array(all.prefix(columnCap)),
                    hiddenColumns: max(0, all.count - columnCap),
                    x: margin + CGFloat(column) * (boxWidth + gapX),
                    y: y)
                tallest = max(tallest, table.height)
                laid.append(table)
            }
            y += tallest + gapY
            start += perRow
        }
        return laid
    }

    // MARK: - Reading it

    /// How wide and tall the canvas has to be for all of it to be reachable.
    var canvas: CGSize {
        guard let last = tables.max(by: { $0.frame.maxY < $1.frame.maxY }) else {
            return CGSize(width: SchemaDiagram.boxWidth, height: SchemaDiagram.boxWidth)
        }
        let right = tables.map(\.frame.maxX).max() ?? SchemaDiagram.boxWidth
        return CGSize(
            width: right + SchemaDiagram.margin, height: last.frame.maxY + SchemaDiagram.margin)
    }

    func table(named name: String) -> Table? { tables.first { $0.name == name } }

    /// Whether there is a picture, as opposed to an answer with nothing in it.
    var isEmpty: Bool { tables.isEmpty }

    /// What the footer says, and what the status bar keeps after the sheet is
    /// closed.
    ///
    /// Every clause is a number somebody would otherwise have to guess at. The
    /// first two say how much of the schema is in the picture — "3 tables" over a
    /// schema of 200 reads as a broken read until it says the other 197 declare
    /// no keys — and the last two name what was left out rather than leaving it
    /// silently missing.
    ///
    /// Main-actor isolated where the rest of the type is not, for the reason
    /// `SchemaDiffReport.summary` is: the number formatter it goes through is,
    /// while the diagram itself is built on the core queue and has to cross back.
    @MainActor
    var summary: String {
        guard asked > 0 else { return "Nothing in \(schema) to read." }
        guard !edges.isEmpty else {
            return "No foreign keys · \(AppModel.pluralized(asked, "table")) read in \(schema)"
        }
        var parts = [
            AppModel.pluralized(edges.count, "key"),
            tables.count == related
                ? "\(AppModel.pluralized(related, "related table")) of \(asked)"
                : "\(tables.count) of \(related) related tables"
        ]
        if undrawn > 0 { parts.append("\(undrawn) not drawn") }
        if outside > 0 {
            parts.append("\(AppModel.pluralized(outside, "key")) point outside \(schema)")
        }
        return parts.joined(separator: " · ")
    }

    /// One box, read aloud with the lines attached to it.
    ///
    /// The lines are drawn on a canvas, which is a picture and says nothing to a
    /// screen reader. Saying them here is what keeps the relationships — the
    /// whole subject of the diagram — reachable without sight: a box announces
    /// what it points at and what points at it.
    func spoken(for table: Table) -> String {
        let out = edges.filter { $0.from == table.name }
            .map { "\($0.fromColumns.joined(separator: ", ")) points at \($0.to)" }
        let incoming = edges.filter { $0.to == table.name && $0.from != table.name }
            .map(\.from)
        var sentence = table.name
        if !out.isEmpty { sentence += ", " + out.joined(separator: ", ") }
        if !incoming.isEmpty {
            sentence += ", referenced by " + incoming.joined(separator: ", ")
        }
        return sentence
    }

    /// Where a line between two boxes starts and ends.
    ///
    /// Centre to centre, clipped to each box's edge, so a line touches the boxes
    /// it joins instead of disappearing under them. Here rather than in the view
    /// for the reason the metrics are.
    static func link(from: Table, to: Table) -> (CGPoint, CGPoint) {
        let a = CGPoint(x: from.frame.midX, y: from.frame.midY)
        let b = CGPoint(x: to.frame.midX, y: to.frame.midY)
        return (border(of: from, towards: b), border(of: to, towards: a))
    }

    /// The point on `table`'s edge on the way to `target`.
    private static func border(of table: Table, towards target: CGPoint) -> CGPoint {
        let centre = CGPoint(x: table.frame.midX, y: table.frame.midY)
        let dx = target.x - centre.x
        let dy = target.y - centre.y
        guard dx != 0 || dy != 0 else { return centre }
        let halfWidth = table.frame.width / 2
        let halfHeight = table.frame.height / 2
        // The smaller of the two scalings is the edge the ray leaves through.
        let scale = min(
            dx == 0 ? .greatestFiniteMagnitude : halfWidth / abs(dx),
            dy == 0 ? .greatestFiniteMagnitude : halfHeight / abs(dy))
        return CGPoint(x: centre.x + dx * scale, y: centre.y + dy * scale)
    }
}
