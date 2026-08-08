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
        var i = 0

        func end(at boundary: Int) {
            if hasCode { found.append(trimming(s, start..<boundary)) }
            hasCode = false
        }

        while i < s.count {
            let c = s[i]
            if c == ";" {
                end(at: i)
                i += 1
                start = i
            } else if c == "'" {
                hasCode = true
                i = endOfQuoted(s, from: i, quote: "'", escapes: isEscapeStringPrefix(s, at: i))
            } else if c == "\"" {
                hasCode = true
                i = endOfQuoted(s, from: i, quote: "\"", escapes: false)
            } else if c == "-", i + 1 < s.count, s[i + 1] == "-" {
                i = endOfLineComment(s, from: i)
            } else if c == "/", i + 1 < s.count, s[i + 1] == "*" {
                i = endOfBlockComment(s, from: i)
            } else if c == "$", let close = endOfDollarQuoted(s, from: i) {
                hasCode = true
                i = close
            } else {
                if !c.properties.isWhitespace { hasCode = true }
                i += 1
            }
        }
        end(at: s.count)
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
}
