import Foundation

/// One thing the go-to palette can take you to.
///
/// Relations, the connections this window holds, and the saved statements —
/// though the sidebar also holds schemas. A schema is not a destination — this
/// window opens tables, and the sidebar already expands schemas — so a schema in
/// this list would be a row that answers Return by doing nothing. Typing one
/// still narrows the list: a needle with a dot in it is read as
/// schema-then-name, the way the SQL completion reads the same characters.
struct GoToTarget: Equatable {
    /// What the palette does with this row: open a relation on the Content tab,
    /// put another of this window's connections in front, or append a saved
    /// statement to the editor.
    ///
    /// A case rather than a flag, because the ordering below reads it and
    /// because the strip's later kinds would each have turned a second boolean
    /// into a third.
    enum Kind: Equatable {
        case relation
        case connection
        case favorite

        /// Where this kind sits when two rows matched equally well.
        ///
        /// `catalog::rank`'s "then kind", with this palette's three in it. A
        /// table named `orders` is what somebody typing `orders` into a palette
        /// over a database meant; a connection called that is the second guess,
        /// and the statement they filed under the name is the third.
        var order: Int {
            switch self {
            case .relation: 0
            case .connection: 1
            case .favorite: 2
            }
        }

        /// The row's trailing label, where it needs one. Relations carry none:
        /// this is a palette over a database and tables are what it has always
        /// listed, so a badge on every row would spend the width on the word
        /// nobody was in doubt about. Connections carry none either — the driver
        /// mark at the end of the row says more than the word would.
        var label: String? { self == .favorite ? "Favorite" : nil }
    }

    let schema: String
    let name: String
    var kind = Kind.relation
    /// The statement a favorite would insert. Empty for a relation, which is
    /// opened by its name rather than by anything it carries.
    var sql = ""

    /// Which of this window's tabs the row is in.
    ///
    /// An index rather than the session itself, for the reason `AppModel.goTo`
    /// gives about `RelationInfo`: the ordering below is checked without a
    /// database behind it, and this list is built and read inside one sheet, so
    /// there is no moment in which the tabs can move under it.
    var tab = 0

    /// What the connection this row is in is called, and empty where naming it
    /// would tell somebody nothing: the tab in front, and the saved statements,
    /// which belong to the person rather than to a server.
    ///
    /// The ordering below reads it, which is what puts the database somebody is
    /// looking at above the others — see `precedes`.
    var connection = ""

    /// The driver behind the row, for the mark at the end of it. Empty for a
    /// favorite, and for a tab still holding a form nobody has dialled.
    var scheme = ""

    /// What the list shows, and what names the row uniquely. Two schemas may
    /// hold a table of the same name, and a palette that showed both as
    /// `orders` would be asking somebody to guess.
    var qualified: String { schema.isEmpty ? name : "\(schema).\(name)" }

    /// The quiet second line: which schema holds the relation, or what the
    /// favorite would type. Two favorites can be named alike as easily as two
    /// tables can, and this is what tells them apart.
    ///
    /// Nothing for a connection. What tells two of those apart is the name
    /// somebody gave them, which is the row itself.
    var detail: String {
        switch kind {
        case .relation: schema
        case .connection: ""
        case .favorite: sql
        }
    }
}

/// The palette's matching: `catalog::rank`'s rule, written where the candidates
/// already are.
///
/// Not asked of the core, deliberately, though the rule is the core's. The names
/// are already in this window's inventory, so a call per keystroke would ship a
/// few thousand of them across the FFI to have four lines of ordering applied to
/// them. What must not differ is the behaviour, which is why the rule is copied
/// exactly and pinned by `GoToChecks`: two go-to surfaces that rank differently
/// is the failure "do not invent a second fuzzy matcher" was written to prevent.
enum GoTo {
    /// The targets `needle` names, best first.
    ///
    /// Keeps what contains the text and puts what begins with it first, which is
    /// `catalog::rank`'s rule and its reason: a prefix match is almost always
    /// the one meant, and a containing match is still worth offering because
    /// somebody looking for `customer_orders` may well type `orders`.
    ///
    /// An empty needle is every table in name order rather than nothing. The
    /// palette opens before anything is typed, and a list that starts empty
    /// teaches nobody what is in the database.
    static func ranked(_ targets: [GoToTarget], matching needle: String) -> [GoToTarget] {
        let text = needle.trimmingCharacters(in: .whitespaces).lowercased()
        guard !text.isEmpty else { return targets.sorted(by: precedes) }

        let (schema, name) = split(text)
        return
            targets
            .filter { target in
                // Both halves are spelt "empty means no constraint" rather than
                // leaning on `contains`, which answers false for the empty
                // string — so a half-typed `sales.` would match nothing at all.
                (name.isEmpty || target.name.lowercased().contains(name))
                    && (schema.isEmpty || target.schema.lowercased().contains(schema))
            }
            .sorted { a, b in
                let first = a.name.lowercased().hasPrefix(name)
                let second = b.name.lowercased().hasPrefix(name)
                if first != second { return first }
                return precedes(a, b)
            }
    }

    /// The tie-break both orderings share: kind, then connection, then the
    /// qualified name.
    ///
    /// This is `catalog::rank`'s "then kind, then name" with `Kind.order`'s
    /// three put in it, and one key added between them for the window that holds
    /// several databases. A window with prod and staging open has a table of the
    /// same name in each, and the one somebody typed the name of is the one they
    /// are looking at — which falls out of sorting by the connection's name,
    /// because the tab in front is the row that carries none.
    private static func precedes(_ a: GoToTarget, _ b: GoToTarget) -> Bool {
        if a.kind != b.kind { return a.kind.order < b.kind.order }
        if a.connection != b.connection { return a.connection < b.connection }
        return a.qualified < b.qualified
    }

    /// A needle split at its last dot, as `(schema, name)`.
    ///
    /// Either half may be empty. `sales.` is a schema and nothing in it yet,
    /// which is the state somebody is in halfway through typing — and it lists
    /// that schema rather than nothing, because that is the useful answer.
    private static func split(_ needle: String) -> (String, String) {
        guard let dot = needle.lastIndex(of: ".") else { return ("", needle) }
        return (String(needle[..<dot]), String(needle[needle.index(after: dot)...]))
    }
}
