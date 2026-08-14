import Foundation

/// What the core offers at the caret, and the rules for asking.
///
/// The question and the answer are both the core's: `crates/sql` decides what a
/// name at the caret would have to be and `crates/catalog` decides which names
/// there are. Nothing here reads SQL, for the reason `SQLScript` gives — a
/// second opinion about where a name begins is a second opinion that will
/// disagree the first time one of them is corrected.
///
/// What is left is this side's own. When to ask at all, since a popup that
/// appears on every keystroke is one the user learns to dismiss; which
/// characters accepting an offer replaces, converted from the scalars the core
/// counts in to the UTF-16 units AppKit does; and what a kind is called on
/// screen.
enum SQLCompletion {
    /// One thing that could be typed.
    struct Offer: Decodable, Equatable {
        /// The name as the catalog holds it, which is what the list shows.
        let label: String
        /// That name written so this database reads it as itself — quoted when
        /// it has to be. What goes into the buffer, and not always what is
        /// shown: inserting the label instead produces SQL that finds nothing.
        let insert: String
        let kind: Kind
        /// The second line: a column's type, a relation's schema and kind.
        let detail: String
    }

    /// What sort of thing an offer is, for the glyph beside it.
    enum Kind: String, Decodable {
        case keyword, schema, relation, column
        /// A name this statement invented — a CTE or a derived table — which the
        /// catalog has never heard of.
        case local
        /// A kind this build does not know. Shown rather than dropped: a core
        /// that grew a kind should not empty the list of an older front end.
        case unknown

        init(from decoder: Decoder) throws {
            let raw = try decoder.singleValueContainer().decode(String.self)
            self = Kind(rawValue: raw) ?? .unknown
        }

        /// The navigator's vocabulary where there is one, so a table looks like
        /// a table in both places.
        var symbol: String {
            switch self {
            case .keyword: return "textformat"
            case .schema: return "square.stack.3d.up"
            case .relation: return "tablecells"
            case .column: return "list.bullet"
            case .local: return "curlybraces"
            case .unknown: return "questionmark.square"
            }
        }
    }

    /// What the core answered about one caret.
    struct Answer: Decodable, Equatable {
        /// Scalar offsets of the characters already typed of the name, which are
        /// the ones accepting an offer replaces. Empty where nothing has been
        /// typed yet.
        let start: Int
        let end: Int
        let offers: [Offer]

        /// Guarded rather than trusted: this arrives as two independent numbers,
        /// and a reversed pair would trap `Range` on construction — in the
        /// middle of a keystroke, taking the window with it.
        var replacing: Range<Int> { start..<Swift.max(start, end) }

        static let none = Answer(start: 0, end: 0, offers: [])

        private init(start: Int, end: Int, offers: [Offer]) {
            self.start = start
            self.end = end
            self.offers = offers
        }
    }

    // MARK: - When to ask

    /// Whether what was just typed is worth asking about.
    ///
    /// Asking on every keystroke would be a popup over the whole buffer,
    /// including in the middle of `WHERE x = 3` where there is nothing anybody
    /// wants inserted. So the trigger is the shape of a name being written: a
    /// letter, a digit or an underscore, the `.` that starts a qualified name,
    /// or the quote that opens one that needs quoting.
    ///
    /// Not a space. `FROM ` has an obvious thing to offer and offering it
    /// unasked puts a list over the buffer every time somebody presses the space
    /// bar; ⌃Space is how a user asks for it there, and that path does not come
    /// through here.
    static func wantsOffers(before caret: Int, in text: String) -> Bool {
        guard caret > 0 else { return false }
        let scalars = Array(text.unicodeScalars)
        guard caret <= scalars.count else { return false }
        let last = scalars[caret - 1]
        if last == "." || last == "_" || last == "\"" || last == "`" || last == "[" {
            return true
        }
        return CharacterSet.alphanumerics.contains(last)
    }

    // MARK: - Bridging to AppKit

    /// A scalar range as the UTF-16 range `NSTextView` edits with.
    ///
    /// The core counts Unicode scalars because that is what a server reports an
    /// error position in; `NSRange` has always meant UTF-16 units. The two agree
    /// on every character in the Basic Multilingual Plane and disagree on every
    /// emoji, so a buffer holding one would otherwise have its completions
    /// replace the wrong characters — and a wrong replacement range is a
    /// keystroke that eats a letter the user can see.
    static func utf16Range(of scalars: Range<Int>, in text: String) -> NSRange? {
        guard let indices = SQLScript.range(scalars, in: text) else { return nil }
        return NSRange(indices, in: text)
    }
}
