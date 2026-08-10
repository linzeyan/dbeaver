import Foundation

/// Breaking an editor buffer into the statements a user runs one at a time.
///
/// Functions over strings and nothing else — no window, no connection — so the
/// rules can be checked directly. `SQLScriptChecks` does exactly that, behind
/// `--verify-splitter`.
///
/// Splitting on `;` is wrong in ways that bite on the first real script. A
/// semicolon inside a string literal, a quoted identifier, a line or block
/// comment, or a dollar-quoted body is not a boundary, and every PL/pgSQL
/// function body ever written contains one — `tools/seed-bench-db.sh` has one
/// four lines long. So this walks the buffer with the construct rules the
/// server's own lexer uses, and only the semicolons it reaches at the top level
/// separate anything.
///
/// Everything here counts in Unicode scalars rather than `Character`s. That is
/// the unit PostgreSQL reports an error position in, and it is the unit a
/// `String.Index` can be recovered from. A `Character` is a grapheme cluster —
/// a flag is two scalars and one Character, a letter with a combining accent
/// likewise — so counting them instead puts every offset after such a character
/// one place to the left, and the caret on the wrong letter.
///
/// The same walk also feeds the editor's colours, through `tokens(in:)`. It is
/// one scanner on purpose: a highlighter with a lexer of its own would be a
/// second opinion about where a string ends, and the two would disagree the
/// first time one of them was fixed. Sharing it means the checks behind
/// `--verify-splitter` are checking the colours too — a scanner that loses its
/// place mid-literal splits wrongly and paints wrongly in the same breath.
enum SQLScript {
    /// What ⌘R will send, and where it came from.
    struct Target: Equatable {
        /// Scalar offsets of the exact text to send. What goes to the server
        /// has to be exactly this slice — a server error position is counted
        /// from the start of the string it was handed, so trimming anything
        /// after the fact moves every position with it.
        let range: Range<Int>
        let origin: Origin

        /// Which part of the buffer ran. The status bar says "query" for a
        /// result and a buffer of five statements makes that a riddle; this is
        /// what it needs to stop being one.
        enum Origin: Equatable {
            /// The only statement in the buffer.
            case whole
            /// One statement of several, both counted from 1.
            case statement(Int, of: Int)
            /// Text the user highlighted.
            case selection
        }

        /// What the status bar calls the result. A buffer holding one statement
        /// is still "query", so a one-liner reads the way it always has.
        var label: String {
            switch origin {
            case .whole: return "query"
            case .statement(let n, let count): return "statement \(n) of \(count)"
            case .selection: return "selection"
            }
        }

        /// What the editor's corner says ⌘R is about to do. The same fact as
        /// `label`, in the tense that matters before the key is pressed.
        var hint: String {
            switch origin {
            case .whole: return "⌘R to run"
            case .statement: return "⌘R runs \(label)"
            case .selection: return "⌘R runs the selection"
            }
        }
    }

    /// The statement ⌘R means, given where the caret or selection is.
    ///
    /// A selection is taken as written: someone who highlighted three lines
    /// meant those three lines, and second-guessing that is how a client runs
    /// something the user did not ask for. Everything else is the statement the
    /// caret sits in.
    static func target(in script: String, selection: Range<Int>) -> Target? {
        if !selection.isEmpty {
            let scalars = Array(script.unicodeScalars)
            let trimmed = trimming(scalars, selection.clamped(to: 0..<scalars.count))
            return trimmed.isEmpty ? nil : Target(range: trimmed, origin: .selection)
        }

        let all = statements(in: script)
        guard !all.isEmpty else { return nil }
        // The last statement that starts at or before the caret. Inside a
        // statement that is the statement; in the blank space or the trailing
        // comment after one, it is the one just above, which is where the caret
        // still visually is. Only a caret in the buffer's leading whitespace
        // matches nothing, and there the first statement is what was meant.
        let index = all.lastIndex { $0.lowerBound <= selection.lowerBound } ?? 0
        return Target(
            range: all[index],
            origin: all.count == 1 ? .whole : .statement(index + 1, of: all.count))
    }

    /// Every statement in `script`, in order, with its terminating semicolon and
    /// the blank space around it removed.
    ///
    /// A chunk holding only comments and whitespace is not a statement — there
    /// is nothing there to run — so a trailing `-- done` after the last `;`
    /// produces no entry. Leading comments, on the other hand, stay inside the
    /// statement below them. That is how scripts are written, the server treats
    /// them as whitespace, and it is what puts a caret parked on `-- fetch the
    /// wide rows` in the statement that comment describes. The cost is that a
    /// comment trailing a semicolon on the same line attaches to the statement
    /// after it instead of the one before; no rule gets both, and leading
    /// comments are the ones that occur.
    static func statements(in script: String) -> [Range<Int>] {
        let s = Array(script.unicodeScalars)
        var found: [Range<Int>] = []
        var start = 0
        var hasCode = false

        scan(s) { construct, range in
            switch construct {
            case .terminator:
                if hasCode { found.append(trimming(s, start..<range.lowerBound)) }
                hasCode = false
                start = range.upperBound
            case .comment, .whitespace:
                break
            case .string, .quotedIdentifier, .dollarQuoted, .number, .word, .other:
                hasCode = true
            }
        }
        if hasCode { found.append(trimming(s, start..<s.count)) }
        return found
    }

    // MARK: - Tokens

    /// A run of the buffer worth giving a colour of its own.
    struct Token: Equatable {
        let kind: Kind
        /// Scalar offsets, like everything else here.
        let range: Range<Int>

        enum Kind {
            case keyword
            case string
            case quotedIdentifier
            case number
            case comment
            case dollarQuoted
        }
    }

    /// Every token the editor colours, in order and non-overlapping.
    ///
    /// Ordinary identifiers, operators and whitespace produce nothing. They are
    /// the editor's default colour, so a token apiece would be most of the array
    /// carrying no information — and the names of tables and columns are what a
    /// reader is scanning for, which argues for leaving them at full strength
    /// rather than tinting them too.
    static func tokens(in script: String) -> [Token] {
        let s = Array(script.unicodeScalars)
        var found: [Token] = []
        // Roughly one token per five scalars in real SQL; a starting guess, not
        // a bound.
        found.reserveCapacity(s.count / 5)
        scan(s) { construct, range in
            switch construct {
            case .string: found.append(Token(kind: .string, range: range))
            case .quotedIdentifier: found.append(Token(kind: .quotedIdentifier, range: range))
            case .dollarQuoted: found.append(Token(kind: .dollarQuoted, range: range))
            case .comment: found.append(Token(kind: .comment, range: range))
            case .number: found.append(Token(kind: .number, range: range))
            case .word: if isKeyword(s, range) { found.append(Token(kind: .keyword, range: range)) }
            case .terminator, .whitespace, .other: break
            }
        }
        return found
    }

    // MARK: - Positions

    /// Where a server error position lands in the buffer.
    ///
    /// PostgreSQL counts from 1, in characters, and from the start of the string
    /// it was handed — which is the statement, not the buffer. Applying it to
    /// the buffer directly points confidently at a character in the wrong
    /// statement, and looks right every time the one that failed happened to be
    /// the first. Nil when the number could not have come from `sent`.
    static func errorOffset(ofPosition position: Int, in sent: Range<Int>) -> Int? {
        guard position >= 1 else { return nil }
        let offset = sent.lowerBound + position - 1
        // One past the last character is a real answer — it is what an
        // unexpected end of input points at — but anything beyond it is not.
        return offset <= sent.upperBound ? offset : nil
    }

    /// Line and column of a scalar offset, both counted from 1 the way every
    /// error message a user has ever read counts them.
    static func lineColumn(of offset: Int, in script: String) -> (line: Int, column: Int) {
        var line = 1
        var column = 1
        for (i, scalar) in script.unicodeScalars.enumerated() {
            if i >= offset { break }
            if scalar == "\n" {
                line += 1
                column = 1
            } else {
                column += 1
            }
        }
        return (line, column)
    }

    /// What to select to make an error at `offset` visible: the word the server
    /// is pointing at, or the single character there when it is punctuation.
    ///
    /// A bare insertion point would be the literal answer and a useless one — a
    /// caret among a thousand characters of SQL is not something anyone spots.
    static func tokenRange(at offset: Int, in script: String) -> Range<Int> {
        let s = Array(script.unicodeScalars)
        guard offset >= 0, offset < s.count else { return offset..<offset }
        var end = offset
        while end < s.count, isIdentifierScalar(s[end]) { end += 1 }
        return offset..<max(end, offset + 1)
    }

    // MARK: - Bridging to String

    /// The text a scalar range names. Empty for a range the buffer cannot hold.
    static func text(_ range: Range<Int>, in script: String) -> String {
        guard let indices = self.range(range, in: script) else { return "" }
        return String(script[indices])
    }

    /// A scalar range as the `String.Index` range an editor selection needs.
    static func range(_ range: Range<Int>, in script: String) -> Range<String.Index>? {
        let scalars = script.unicodeScalars
        guard range.lowerBound >= 0,
            let lower = scalars.index(
                scalars.startIndex, offsetBy: range.lowerBound, limitedBy: scalars.endIndex),
            let upper = scalars.index(
                scalars.startIndex, offsetBy: range.upperBound, limitedBy: scalars.endIndex)
        else { return nil }
        return lower..<upper
    }

    // MARK: - Lexing

    /// What one construct in the buffer turned out to be.
    ///
    /// The categories are the splitter's, not the highlighter's: everything the
    /// splitter has to tell apart is here, and the highlighter maps what it
    /// wants onto them. `word` is any run of identifier characters, keyword or
    /// not — deciding which needs a word list, and the splitter does not care.
    private enum Construct {
        case terminator
        case string
        case quotedIdentifier
        case dollarQuoted
        case comment
        case number
        case word
        case whitespace
        case other
    }

    /// One pass over the buffer, naming every construct in order and covering
    /// every scalar exactly once.
    ///
    /// The order of the tests is the grammar's and cannot be shuffled. The
    /// dollar test comes before the word test because `$` continues an
    /// identifier as well as opening a body, and `endOfDollarQuoted` is the only
    /// thing that knows which; the number test comes before the word test
    /// because both start on a digit and only one of them is reached with a
    /// digit first.
    private static func scan(_ s: [Unicode.Scalar], _ emit: (Construct, Range<Int>) -> Void) {
        var i = 0
        while i < s.count {
            let c = s[i]
            let start = i
            if c == ";" {
                i += 1
                emit(.terminator, start..<i)
            } else if c == "'" {
                let escapes = isEscapeStringPrefix(s, at: i)
                i = endOfQuoted(s, from: i, quote: "'", escapes: escapes)
                // The `E` of `E'…'` belongs to the literal, so the span reaches
                // back over it. It was emitted a moment ago as a one-character
                // word, which costs nothing: no keyword is spelled "e", so the
                // highlighter drops it and the two spans never both survive.
                emit(.string, (escapes ? start - 1 : start)..<i)
            } else if c == "\"" {
                i = endOfQuoted(s, from: i, quote: "\"", escapes: false)
                emit(.quotedIdentifier, start..<i)
            } else if c == "-", i + 1 < s.count, s[i + 1] == "-" {
                i = endOfLineComment(s, from: i)
                emit(.comment, start..<i)
            } else if c == "/", i + 1 < s.count, s[i + 1] == "*" {
                i = endOfBlockComment(s, from: i)
                emit(.comment, start..<i)
            } else if c == "$", let close = endOfDollarQuoted(s, from: i) {
                i = close
                emit(.dollarQuoted, start..<i)
            } else if let close = endOfNumber(s, from: i) {
                i = close
                emit(.number, start..<i)
            } else if isIdentifierScalar(c) {
                while i < s.count, isIdentifierScalar(s[i]) { i += 1 }
                emit(.word, start..<i)
            } else {
                i += 1
                emit(c.properties.isWhitespace ? .whitespace : .other, start..<i)
            }
        }
    }

    /// One past the numeric literal starting at `i`, or nil when what is there
    /// is not one.
    ///
    /// Reached only at the start of a token, which is what keeps the `1` of
    /// `col1` out and the `$1` of a parameter placeholder out with it: both are
    /// consumed by the word that began before the digit.
    private static func endOfNumber(_ s: [Unicode.Scalar], from i: Int) -> Int? {
        if isDigit(s[i]) {
            // Non-decimal literals and `_` as a group separator arrived in
            // PostgreSQL 16. `0x` with no digits after it is not a number to the
            // server either, but it is not anything else either, so painting the
            // half-typed form is better than leaving it to flicker.
            if s[i] == "0", i + 1 < s.count, isRadixMark(s[i + 1]) {
                var j = i + 2
                while j < s.count, isHexDigit(s[j]) || s[j] == "_" { j += 1 }
                return j
            }
        } else if s[i] != "." || i + 1 >= s.count || !isDigit(s[i + 1]) {
            // `.5` is a number; `t.x` is not, and neither is a bare `.`.
            return nil
        }

        var j = i
        var seenPoint = false
        while j < s.count {
            let c = s[j]
            if isDigit(c) || c == "_" {
                j += 1
            } else if c == ".", !seenPoint {
                seenPoint = true
                j += 1
            } else if c == "e" || c == "E", let after = exponentDigits(s, from: j) {
                return after
            } else {
                break
            }
        }
        return j
    }

    /// One past the exponent starting at the `e` at `j`, or nil when the `e`
    /// begins an identifier instead — `1e` is the number 1 followed by a column
    /// called `e`, and `1e+` is the same plus an operator.
    private static func exponentDigits(_ s: [Unicode.Scalar], from j: Int) -> Int? {
        var k = j + 1
        if k < s.count, s[k] == "+" || s[k] == "-" { k += 1 }
        guard k < s.count, isDigit(s[k]) else { return nil }
        while k < s.count, isDigit(s[k]) { k += 1 }
        return k
    }

    private static func isDigit(_ c: Unicode.Scalar) -> Bool { c >= "0" && c <= "9" }

    private static func isRadixMark(_ c: Unicode.Scalar) -> Bool {
        c == "x" || c == "X" || c == "o" || c == "O" || c == "b" || c == "B"
    }

    private static func isHexDigit(_ c: Unicode.Scalar) -> Bool {
        isDigit(c) || (c >= "a" && c <= "f") || (c >= "A" && c <= "F")
    }

    /// One past the closing quote of the run opening at `open`, or the end of
    /// the buffer for one that is never closed.
    ///
    /// An unterminated quote swallowing the rest of the buffer is the right
    /// answer, not a failure to recover: the user is mid-typing, and treating a
    /// semicolon inside the half-written literal as a boundary would run half a
    /// string as if it were a statement.
    private static func endOfQuoted(
        _ s: [Unicode.Scalar], from open: Int, quote: Unicode.Scalar, escapes: Bool
    ) -> Int {
        var i = open + 1
        while i < s.count {
            if escapes, s[i] == "\\" {
                i += 2
                continue
            }
            if s[i] == quote {
                // Doubled is one embedded quote, which is how both a literal and
                // an identifier carry their own delimiter.
                if i + 1 < s.count, s[i + 1] == quote {
                    i += 2
                    continue
                }
                return i + 1
            }
            i += 1
        }
        return s.count
    }

    /// Whether the quote at `i` opens an `E'…'` literal, where a backslash
    /// escapes the character after it.
    ///
    /// Only that form takes backslash escapes: `standard_conforming_strings` has
    /// been on by default since 9.1, so in a plain literal `'a\'` is a complete
    /// string ending in a backslash. Reading it the other way leaves the scanner
    /// one quote out of step for the rest of the buffer.
    private static func isEscapeStringPrefix(_ s: [Unicode.Scalar], at i: Int) -> Bool {
        guard i > 0, s[i - 1] == "E" || s[i - 1] == "e" else { return false }
        // `someE'x'` is not an escape string; the E belongs to the identifier.
        return i < 2 || !isIdentifierScalar(s[i - 2])
    }

    private static func endOfLineComment(_ s: [Unicode.Scalar], from open: Int) -> Int {
        var i = open + 2
        while i < s.count, s[i] != "\n" { i += 1 }
        return i
    }

    /// One past the `*/` that closes the comment opening at `open`.
    ///
    /// Block comments nest in PostgreSQL, unlike C's, so this counts depth. A
    /// scanner that stopped at the first `*/` would leave the tail of a
    /// commented-out block being read as SQL.
    private static func endOfBlockComment(_ s: [Unicode.Scalar], from open: Int) -> Int {
        var depth = 0
        var i = open
        while i < s.count {
            if s[i] == "/", i + 1 < s.count, s[i + 1] == "*" {
                depth += 1
                i += 2
            } else if s[i] == "*", i + 1 < s.count, s[i + 1] == "/" {
                depth -= 1
                i += 2
                if depth == 0 { return i }
            } else {
                i += 1
            }
        }
        return s.count
    }

    /// One past the closing `$tag$` of the dollar-quoted string opening at
    /// `open`, or nil when the `$` there opens nothing.
    ///
    /// Two things make this more than a search for the next `$…$`. A tag may not
    /// start with a digit, which is what keeps `$1` a parameter placeholder
    /// rather than the start of a quoted body running to the end of the script.
    /// And `$` is a legal identifier continuation, so `a$b$c` is one identifier
    /// to the server's lexer and must be one here too — hence the look back at
    /// the character before the dollar.
    private static func endOfDollarQuoted(_ s: [Unicode.Scalar], from open: Int) -> Int? {
        if open > 0, isIdentifierScalar(s[open - 1]) { return nil }
        var i = open + 1
        while i < s.count, isTagScalar(s[i], isFirst: i == open + 1) { i += 1 }
        guard i < s.count, s[i] == "$" else { return nil }

        let tagLength = i - open + 1
        var j = i + 1
        while j + tagLength <= s.count {
            if (0..<tagLength).allSatisfy({ s[j + $0] == s[open + $0] }) { return j + tagLength }
            j += 1
        }
        // An unclosed body runs to the end, for the same reason an unclosed
        // quote does: what follows is inside a string until proven otherwise.
        return s.count
    }

    private static func isTagScalar(_ c: Unicode.Scalar, isFirst: Bool) -> Bool {
        if c == "_" || c.value >= 0x80 { return true }
        if c >= "a" && c <= "z" || c >= "A" && c <= "Z" { return true }
        return !isFirst && c >= "0" && c <= "9"
    }

    /// Whether `c` can continue an unquoted identifier. `$` is in the set: that
    /// is what makes `a$b$c` a single name.
    private static func isIdentifierScalar(_ c: Unicode.Scalar) -> Bool {
        if c == "_" || c == "$" || c.value >= 0x80 { return true }
        return c >= "a" && c <= "z" || c >= "A" && c <= "Z" || c >= "0" && c <= "9"
    }

    private static func trimming(_ s: [Unicode.Scalar], _ range: Range<Int>) -> Range<Int> {
        var lower = range.lowerBound
        var upper = range.upperBound
        while lower < upper, s[lower].properties.isWhitespace { lower += 1 }
        while upper > lower, s[upper - 1].properties.isWhitespace { upper -= 1 }
        return lower..<upper
    }

    // MARK: - Keywords

    /// Whether the word at `range` is one the editor paints.
    ///
    /// Case-folds and compares without touching the buffer's own storage,
    /// because this runs once per identifier in the script and the script is
    /// re-lexed on every edit. The two early exits carry most of that: no
    /// keyword is longer than `current_timestamp` and none contains a non-ASCII
    /// character, so a long name or an accented one is rejected before a string
    /// is built at all.
    private static func isKeyword(_ s: [Unicode.Scalar], _ range: Range<Int>) -> Bool {
        guard range.count <= longestKeyword else { return false }
        var word = ""
        word.reserveCapacity(range.count)
        for i in range {
            let c = s[i]
            guard c.value < 0x80 else { return false }
            let folded = c >= "A" && c <= "Z" ? Unicode.Scalar(c.value + 32)! : c
            word.unicodeScalars.append(folded)
        }
        return keywords.contains(word)
    }

    private static let longestKeyword = keywords.map(\.count).max() ?? 0

    /// The words the editor paints as keywords.
    ///
    /// Read out of the server rather than remembered:
    /// `SELECT word, catcode FROM pg_get_keywords()` on PostgreSQL 17.10, which
    /// is the same table Appendix C of the manual is generated from. Every
    /// reserved word is here — the three categories the server will not accept
    /// as a bare column name in at least one position — so painting one can
    /// never surprise anybody.
    ///
    /// The unreserved bucket is where judgement comes in, and it cannot simply
    /// be taken whole: it holds 327 words including `name`, `value`, `level` and
    /// `year`, which are unreserved precisely because they are ordinary column
    /// names, and a highlighter that paints `SELECT name, value FROM config` as
    /// three keywords is worse than one that paints nothing. Nor can it be left
    /// out: `insert`, `update`, `delete`, `set` and `by` are all unreserved, and
    /// an editor that does not colour `INSERT` is not colouring SQL. So the
    /// unreserved words taken are the ones that name a statement, name an object
    /// a statement acts on, or introduce a clause inside one.
    ///
    /// The cost of that line is real and accepted: a column genuinely called
    /// `key` or `type` gets painted. That is a cosmetic surprise on a rare name,
    /// where the alternative is `PRIMARY KEY` and `CREATE TYPE` going unpainted
    /// every time they occur.
    private static let keywords: Set<String> = reserved.union(unreserved).union(typeNames)

    /// `catcode` R, T and C: reserved, reserved-but-usable-as-a-function-name,
    /// and unreserved-but-not-usable-as-a-function-or-type-name. Verbatim.
    private static let reserved: Set<String> = [
        "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric",
        "authorization", "between", "bigint", "binary", "bit", "boolean", "both", "case", "cast",
        "char", "character", "check", "coalesce", "collate", "collation", "column", "concurrently",
        "constraint", "create", "cross", "current_catalog", "current_date", "current_role",
        "current_schema", "current_time", "current_timestamp", "current_user", "dec", "decimal",
        "default", "deferrable", "desc", "distinct", "do", "else", "end", "except", "exists",
        "extract", "false", "fetch", "float", "for", "foreign", "freeze", "from", "full", "grant",
        "greatest", "group", "grouping", "having", "ilike", "in", "initially", "inner", "inout",
        "int", "integer", "intersect", "interval", "into", "is", "isnull", "join", "json",
        "json_array", "json_arrayagg", "json_exists", "json_object", "json_objectagg",
        "json_query", "json_scalar", "json_serialize", "json_table", "json_value", "lateral",
        "leading", "least", "left", "like", "limit", "localtime", "localtimestamp", "merge_action",
        "national", "natural", "nchar", "none", "normalize", "not", "notnull", "null", "nullif",
        "numeric", "offset", "on", "only", "or", "order", "out", "outer", "overlaps", "overlay",
        "placing", "position", "precision", "primary", "real", "references", "returning", "right",
        "row", "select", "session_user", "setof", "similar", "smallint", "some", "substring",
        "symmetric", "system_user", "table", "tablesample", "then", "time", "timestamp", "to",
        "trailing", "treat", "trim", "true", "union", "unique", "user", "using", "values",
        "varchar", "variadic", "verbose", "when", "where", "window", "with", "xmlattributes",
        "xmlconcat", "xmlelement", "xmlexists", "xmlforest", "xmlnamespaces", "xmlparse", "xmlpi",
        "xmlroot", "xmlserialize", "xmltable"
    ]

    /// `catcode` U, filtered by the rule above. Grouped by what each group is
    /// doing in a script, which is also how the rule was applied.
    private static let unreserved: Set<String> = [
        // Statements.
        "abort", "alter", "begin", "call", "checkpoint", "close", "cluster", "comment", "commit",
        "copy", "deallocate", "declare", "delete", "discard", "drop", "execute", "explain",
        "import", "insert", "listen", "load", "lock", "move", "notify", "prepare", "reassign",
        "refresh", "reindex", "release", "reset", "revoke", "rollback", "savepoint", "set", "show",
        "start", "truncate", "unlisten", "update", "vacuum",

        // What they act on.
        "aggregate", "database", "domain", "extension", "function", "index", "materialized",
        "operator", "policy", "procedure", "publication", "role", "routine", "rule", "schema",
        "sequence", "server", "statistics", "subscription", "tablespace", "trigger", "type",
        "view", "wrapper",

        // Clauses within them.
        "add", "by", "cascade", "conflict", "cycle", "each", "enum", "escape", "exclude", "filter",
        "generated", "identity", "if", "include", "increment", "inherits", "instead", "key",
        "maxvalue", "minvalue", "no", "nothing", "nulls", "of", "off", "owned", "owner",
        "partition", "recursive", "rename", "replace", "restart", "restrict", "returns", "sets",
        "stored", "temp", "temporary", "unlogged", "valid", "varying", "within", "without",

        // Function and procedure definitions.
        "called", "definer", "immutable", "invoker", "language", "leakproof", "parallel", "return",
        "security", "sql", "stable", "strict", "volatile",

        // Window frames and grouping sets.
        "cube", "following", "groups", "over", "preceding", "range", "rollup", "rows", "ties",
        "unbounded",

        // Transactions.
        "committed", "deferred", "immediate", "isolation", "nowait", "repeatable", "serializable",
        "skip", "transaction", "uncommitted",

        "text"
    ]

    /// Types the grammar does not spell.
    ///
    /// `timestamp` and `varchar` are keywords and `date` and `uuid` are not:
    /// the first two are in the grammar, the rest are ordinary entries in
    /// `pg_type` that the parser resolves like any other name. That distinction
    /// is invisible and uninteresting to someone writing a CREATE TABLE, and an
    /// editor that colours `bigint` and leaves `uuid` grey next to it reads as a
    /// bug rather than as a fact about the grammar. The `serial` family is in
    /// neither table — the parser rewrites it into an integer and a sequence —
    /// and is here for the same reason.
    private static let typeNames: Set<String> = [
        "bigserial", "bytea", "date", "inet", "jsonb", "serial", "smallserial", "uuid"
    ]
}
