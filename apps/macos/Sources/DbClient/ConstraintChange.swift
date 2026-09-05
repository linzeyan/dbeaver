import Foundation

/// Which of the three sorts a constraint statement is about.
///
/// Its own type rather than the `ConstraintKind` the structure pane already
/// shows, and the two are not the same question. That one describes what the
/// catalog reported and carries `exclude` and `other`, neither of which this
/// form can compose; this one is the closed set of things that can be *written*,
/// and it carries the foreign key, which the catalog reports in a list of its
/// own.
///
/// No primary key. DBeaver drops one on MySQL as `ALTER TABLE t DROP PRIMARY
/// KEY`, with no name in the statement at all, and refuses outright when the key
/// is the one an `AUTO_INCREMENT` column needs — and there is nowhere here to
/// put the item anyway: a primary key reaches this window as its index, on a row
/// whose Drop Index is already drawn shut.
///
/// The raw values are the words the C boundary reads.
enum ConstraintSort: String, Codable, Hashable, CaseIterable {
    case unique
    case check
    case foreignKey = "foreign_key"

    /// What the sort picker's row says.
    var label: String {
        switch self {
        case .unique: return "Unique"
        case .check: return "Check"
        case .foreignKey: return "Foreign key"
        }
    }

    /// The noun the menu items use, which is not the label: a foreign key has a
    /// section of its own in this window, and calling its item "New Constraint"
    /// would name the row it was opened from as something else.
    var noun: String {
        self == .foreignKey ? "Foreign Key" : "Constraint"
    }

    /// The sentence under the fields, which is the one thing on the sheet that
    /// is not a restatement of the statement below it.
    var consequence: String {
        switch self {
        case .unique:
            return "The server reads the whole table to check it, and refuses the constraint if "
                + "the values are not already unique. The index it builds underneath stays with "
                + "the constraint and cannot be dropped on its own."
        case .check:
            return "Every row is tested as the constraint is added, and the server refuses it if "
                + "any row fails. Rows written afterwards are tested one at a time."
        case .foreignKey:
            return "Every row is checked against the other table as the key is added. Afterwards "
                + "a row cannot name something that is not there, and the referenced rows cannot "
                + "leave without the rule below deciding what happens to these."
        }
    }
}

/// What a foreign key does to this table's rows when the row it points at moves.
///
/// A closed set for the reason `ColumnKind` is one: the alternative is a text
/// field, and a rule typed by hand is SQL spelled for a server this side does
/// not know. The raw values are the words the core reads.
///
/// `noAction` is every one of these servers' default and is written as nothing
/// at all, which is not the same as being absent from this list: somebody
/// choosing it is choosing to leave the rule alone, and a picker with no row for
/// the default could not be set back.
enum ReferentialAction: String, Codable, Hashable, CaseIterable {
    case noAction = "no_action"
    case restrict
    case cascade
    case setNull = "set_null"
    case setDefault = "set_default"

    /// What the picker's row says. Sentence case rather than the SQL, because
    /// the SQL is on screen underneath in the statement itself.
    var label: String {
        switch self {
        case .noAction: return "No action"
        case .restrict: return "Restrict"
        case .cascade: return "Cascade"
        case .setNull: return "Set null"
        case .setDefault: return "Set default"
        }
    }
}

/// A constraint that does not exist yet.
///
/// One struct carrying every sort's answers rather than an enum carrying one
/// sort's, which is the opposite of the core's shape and is deliberate: this is
/// what a *form* holds. Somebody who types a name, fills in three columns and
/// then changes the sort picker to look at what a check would need should find
/// their columns still there when they change it back. An enum with associated
/// values would throw away the other sorts' answers on every switch of the
/// picker, and the core is where the shape that cannot hold a contradiction
/// belongs.
///
/// What crosses is only the fields the chosen sort needs — see `encode`.
struct NewConstraint: Hashable, Encodable {
    var sort: ConstraintSort = .unique
    var name: String = ""
    /// The columns on this table. For a foreign key each row also carries the
    /// column it points at, which is what makes the two lists impossible to get
    /// out of step: a key over `(a, b)` referencing `(x)` is a statement the
    /// server refuses, and a row that holds both ends cannot express it.
    var columns: [ConstraintColumn] = [ConstraintColumn()]
    /// Written after `CHECK` exactly as given, and neither quoted nor checked —
    /// the rule `NewTableColumn`'s default follows. A check is an expression in
    /// the server's own grammar, and telling a legal one from a mistake means
    /// parsing that grammar. What was typed is what is sent, and the statement
    /// is on screen before it goes.
    var expression: String = ""
    /// Empty means the container this table is in, which is what the sheet fills
    /// it with: a key that points somewhere else is the unusual one and the
    /// field is there to be changed.
    var otherSchema: String = ""
    var otherTable: String = ""
    var onDelete: ReferentialAction = .noAction
    var onUpdate: ReferentialAction = .noAction

    private enum CodingKeys: String, CodingKey {
        case sort, name, columns, expression
        case otherSchema = "other_schema"
        case otherTable = "other_table"
        case otherColumns = "other_columns"
        case onDelete = "on_delete"
        case onUpdate = "on_update"
    }

    /// Only what the chosen sort needs.
    ///
    /// The unchosen fields are held on this side and not sent, because the core
    /// reads a tagged shape where a check has no columns and a unique constraint
    /// has no table to point at. Sending the lot would make the boundary take a
    /// constraint that is three things at once.
    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(sort, forKey: .sort)
        // Trimmed, the rule every other name on this side follows: a constraint
        // called " x " is one nobody can name again without the spaces.
        try container.encode(name.trimmingCharacters(in: .whitespaces), forKey: .name)
        switch sort {
        case .unique:
            try container.encode(columns.map(\.name), forKey: .columns)
        case .check:
            try container.encode(
                expression.trimmingCharacters(in: .whitespaces), forKey: .expression)
        case .foreignKey:
            try container.encode(columns.map(\.name), forKey: .columns)
            try container.encode(columns.map(\.other), forKey: .otherColumns)
            try container.encode(
                otherSchema.trimmingCharacters(in: .whitespaces), forKey: .otherSchema)
            try container.encode(
                otherTable.trimmingCharacters(in: .whitespaces), forKey: .otherTable)
            try container.encode(onDelete, forKey: .onDelete)
            try container.encode(onUpdate, forKey: .onUpdate)
        }
    }
}

/// One row of a constraint's column list.
///
/// A struct with an identity rather than a bare string, for the reason
/// `IndexColumn` has one: two rows can hold the same name while somebody is
/// picking, and a list keyed by value would collapse them into one.
struct ConstraintColumn: Identifiable, Hashable {
    let id = UUID()
    /// This table's column.
    var name: String = ""
    /// The referenced table's column, on a foreign key. Typed rather than
    /// picked: the other table's columns are a read this sheet does not make,
    /// and holding another relation's metadata open while a form is being filled
    /// in is the thing this window does not do.
    var other: String = ""
}

/// What is being done to a constraint of a relation.
///
/// Two verbs. No server here alters a constraint in place — DBeaver's own modify
/// path is the delete followed by the create, which is two statements and a
/// window in which the table is unconstrained and rows can arrive that the new
/// rule would have refused — so what is offered is the two that are one
/// statement each.
enum ConstraintChange: Hashable, Encodable {
    case create(NewConstraint)
    /// The sort travels with the name because the statement cannot be written
    /// without it: PostgreSQL drops all three with `DROP CONSTRAINT` and MySQL
    /// writes `DROP KEY`, `DROP CONSTRAINT` and `DROP FOREIGN KEY`. The row the
    /// item was opened from is what knows which.
    case drop(name: String, sort: ConstraintSort)

    /// The word the core reads.
    var verb: String {
        switch self {
        case .create: return "create"
        case .drop: return "drop"
        }
    }

    /// Which sort this acts on, which decides both the noun and the statement.
    var sort: ConstraintSort {
        switch self {
        case .create(let constraint): return constraint.sort
        case .drop(_, let sort): return sort
        }
    }

    /// What the menu item says.
    ///
    /// The noun follows the sort rather than being fixed, because the two
    /// sections this is opened from call the object different things: a row in
    /// Foreign keys offering "Drop Constraint…" would be naming the row as
    /// something the section does not.
    var menuTitle: String {
        switch self {
        case .create: return "New \(sort.noun)…"
        case .drop: return "Drop \(sort.noun)…"
        }
    }

    /// What the button that runs it says. No ellipsis: this one is the doing.
    var actionTitle: String {
        switch self {
        case .create: return "Create"
        case .drop: return "Drop"
        }
    }

    var progressive: String {
        switch self {
        case .create: return "Creating"
        case .drop: return "Dropping"
        }
    }

    var pastTense: String {
        switch self {
        case .create: return "Created"
        case .drop: return "Dropped"
        }
    }

    /// Whether pressing the button loses something that cannot be got back.
    ///
    /// The drop. No rows go with it, but the rule does, and the rows that arrive
    /// while it is gone are exactly the ones that stop it being added again —
    /// which is enough to take Return off the button.
    var isDestructive: Bool {
        if case .drop = self { return true }
        return false
    }

    /// The constraint this acts on, which the sheet puts at the top.
    var constraintName: String {
        switch self {
        case .create(let constraint): return constraint.name
        case .drop(let name, _): return name
        }
    }

    private enum CodingKeys: String, CodingKey {
        case change, constraint, name, sort
    }

    func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(verb, forKey: .change)
        switch self {
        case .create(let constraint): try container.encode(constraint, forKey: .constraint)
        case .drop(let name, let sort):
            try container.encode(name, forKey: .name)
            try container.encode(sort, forKey: .sort)
        }
    }
}
