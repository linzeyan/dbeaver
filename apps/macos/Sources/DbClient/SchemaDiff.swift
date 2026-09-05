import Foundation

/// One thing two schemas do not agree about, as the core reported it.
///
/// A mirror of `dbdiff::Difference` and nothing else. The words in `left` and
/// `right` are each side's own description of the object, composed in the core;
/// this side neither parses them nor adds to them. That is what keeps the list
/// of fields that are compared and the list of fields that are shown the same
/// list — a difference is *found* by two descriptions differing and *shown* as
/// those descriptions, so neither can drift from the other.
struct SchemaDifference: Decodable, Identifiable, Hashable {
    enum Kind: String, Decodable {
        case relation
        case column
        case index
        case constraint
        case foreignKey = "foreign_key"
    }

    /// Which side has it.
    ///
    /// Not "added" and "removed". The comparison is symmetric — neither schema
    /// is the one the other was changed from, and the picker will happily put
    /// last week's staging on the left — so a direction here would be the half
    /// of the story this was never told.
    enum Verdict: String, Decodable {
        case onlyLeft = "only_left"
        case onlyRight = "only_right"
        case changed
    }

    let table: String
    let object: String
    let kind: Kind
    let verdict: Verdict
    let left: String
    let right: String

    /// The core reports one line per named object per kind within a relation,
    /// which is exactly these three fields. Joined on the unit separator rather
    /// than a punctuation mark, because every one of them is a name a database
    /// will let somebody put a dot or a slash in.
    var id: String { "\(table)\u{1F}\(kind.rawValue)\u{1F}\(object)" }

    /// What a side that does not have this object shows. An em dash rather than
    /// a blank: a blank cell in a table reads as something that failed to load.
    static let absent = "—"

    var leftCell: String { verdict == .onlyRight ? Self.absent : left }
    var rightCell: String { verdict == .onlyLeft ? Self.absent : right }

    /// The glyph in the leading column, pointing at the side that has it.
    ///
    /// Pointing rather than coloured red and green. Those two colours mean
    /// removed and added everywhere they are used, which is a direction this
    /// report does not claim — and they would say it louder than a heading
    /// could take back.
    var marker: String {
        switch verdict {
        case .onlyLeft: "◀"
        case .onlyRight: "▶"
        case .changed: "≠"
        }
    }

    /// What the Kind column says.
    ///
    /// A relation-level difference names itself: the descriptions are already
    /// the words "table" and "view", so a fixed word here would be a third one
    /// that could contradict them.
    var word: String {
        switch kind {
        case .relation: verdict == .onlyRight ? right : left
        case .column: "column"
        case .index: "index"
        case .constraint: "constraint"
        case .foreignKey: "foreign key"
        }
    }

    /// One row read aloud, which has to carry what the glyph and the two columns
    /// carry between them — the glyph points at a column heading a screen reader
    /// is not on, and the empty side is silence.
    func spoken(left leftName: String, right rightName: String) -> String {
        switch verdict {
        case .onlyLeft: "\(word) \(object) in \(table), only on \(leftName): \(left)"
        case .onlyRight: "\(word) \(object) in \(table), only on \(rightName): \(right)"
        case .changed:
            "\(word) \(object) in \(table) differs: \(leftName) \(left), \(rightName) \(right)"
        }
    }
}

/// Two schemas compared, as the core reported it.
struct SchemaDiffReport: Decodable {
    /// How many relations were read on each side.
    ///
    /// Here because "the two agree" and "there was nothing to read" are the same
    /// empty list and are not the same news. A login that can see nothing in the
    /// schema it named deserves the second sentence.
    let leftRelations: Int
    let rightRelations: Int
    let differences: [SchemaDifference]

    enum CodingKeys: String, CodingKey {
        case leftRelations = "left_relations"
        case rightRelations = "right_relations"
        case differences
    }

    /// The sentence under the table.
    ///
    /// Main-actor isolated where the rest of the type is not, because the number
    /// formatter it goes through is: the report itself is decoded on the core
    /// queue and has to cross back, and only the sentence is ever read on screen.
    @MainActor
    func summary(left: String, right: String) -> String {
        guard leftRelations > 0 || rightRelations > 0 else {
            return "Neither schema has anything in it to compare."
        }
        guard !differences.isEmpty else {
            // The counts cannot differ here: a relation one side does not have
            // is itself a difference, so an empty list means one number.
            return "No differences · \(AppModel.pluralized(leftRelations, "relation")) "
                + "on each side"
        }
        // The second half names what it is counting. "8 differences · 4 on prod,
        // 4 on staging" reads as four of the differences being on each side,
        // which is not what those numbers are.
        return "\(AppModel.pluralized(differences.count, "difference")) · "
            + "\(AppModel.pluralized(leftRelations, "relation")) on \(left), "
            + "\(rightRelations) on \(right)"
    }
}

/// A report and the pair it is a report of.
///
/// One value rather than three loose ones, so the column headings cannot come
/// from a different pair than the rows beneath them: changing the target picker
/// after a comparison would otherwise relabel a report that is not its own.
struct SchemaComparison {
    let left: String
    let right: String
    let report: SchemaDiffReport
}
