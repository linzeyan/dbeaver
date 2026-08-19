import Foundation

/// One thing the go-to palette can take you to.
///
/// Relations only, though the sidebar also holds schemas. A schema is not a
/// destination — this window opens tables, and the sidebar already expands
/// schemas — so a schema in this list would be a row that answers Return by
/// doing nothing. Typing one still narrows the list: a needle with a dot in it
/// is read as schema-then-name, the way the SQL completion reads the same
/// characters.
struct GoToTarget: Equatable {
    let schema: String
    let name: String

    /// What the list shows, and what names the row uniquely. Two schemas may
    /// hold a table of the same name, and a palette that showed both as
    /// `orders` would be asking somebody to guess.
    var qualified: String { schema.isEmpty ? name : "\(schema).\(name)" }
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
        guard !text.isEmpty else { return targets.sorted { $0.qualified < $1.qualified } }

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
                return a.qualified < b.qualified
            }
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
