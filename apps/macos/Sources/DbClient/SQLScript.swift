import CDbFfi
import Foundation

/// What the core's scanner says about the buffer in the editor.
///
/// The rules live in `crates/sql` and are asked for across the FFI: where a
/// literal ends, which words this database calls keywords, what separates two
/// statements. Nothing here reads a character of SQL. A Swift lexer beside the
/// Rust one would be a second opinion about where a string ends, and the two
/// would disagree the first time one of them was fixed — a wrongly split
/// statement and a wrongly coloured buffer in the same breath.
///
/// What is left is this side's own work: converting between the offsets the core
/// counts in and the `String.Index` values an editor selection needs, and
/// deciding what the answers are called on screen.
///
/// Those offsets are Unicode scalars rather than `Character`s. That is the unit
/// a Rust `char` offset is and the unit a server reports an error position in. A
/// `Character` is a grapheme cluster — a flag is two scalars and one Character,
/// a letter with a combining accent likewise — so counting them instead puts
/// every offset after such a character one place to the left, and the caret on
/// the wrong letter.
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

    // MARK: - Scanning

    /// Everything the core has to say about one buffer.
    ///
    /// One value rather than three answers fetched separately, because one scan
    /// of the text produces all of them and the window asks them together: the
    /// editor repaints, the Run button asks whether there is anything to run,
    /// and the corner asks which statement ⌘R would send — all off the same
    /// keystroke.
    struct Scan {
        /// The statements, in order, each trimmed of the blank space around it
        /// and of its terminating semicolon.
        let statements: [Range<Int>]

        /// What ⌘R would send, or nil when there is nothing to run.
        let target: Target?

        /// The runs the editor gives a colour of their own.
        ///
        /// Ordinary identifiers, operators and whitespace are not among them.
        /// They are the editor's default colour, so a token apiece would be most
        /// of the array carrying no information — and the names of tables and
        /// columns are what a reader is scanning for, which argues for leaving
        /// them at full strength rather than tinting them too.
        let tokens: [Token]

        /// Every token's span, painted or not, in order and covering the buffer
        /// exactly once. `tokenRange` walks it, and `--verify-splitter` checks
        /// that the covering survived the crossing.
        let spans: [Range<Int>]

        /// What to select to make an error at `offset` visible: the run of
        /// characters the server is pointing into.
        ///
        /// A bare insertion point would be the literal answer and a useless one
        /// — a caret among a thousand characters of SQL is not something anyone
        /// spots. The spans cover the buffer, so there is always one to find
        /// unless the offset is outside it altogether.
        func tokenRange(at offset: Int) -> Range<Int> {
            guard offset >= 0, offset < spans.last?.upperBound ?? 0 else {
                return offset..<offset
            }
            var low = 0
            var high = spans.count
            while low < high {
                let mid = (low + high) / 2
                if spans[mid].upperBound <= offset { low = mid + 1 } else { high = mid }
            }
            return spans[low]
        }
    }

    /// The core's reading of `script`, in the dialect `scheme` names.
    ///
    /// Memoized on its arguments, which is what holds a keystroke to one call
    /// into the core. Every part of the window asks something of this while
    /// SwiftUI renders — the Run button, the editor's corner, the Query menu —
    /// and scanning the buffer once per asker would be most of what a redraw
    /// costs.
    static func scan(_ script: String, scheme: String, selection: Range<Int>) -> Scan {
        if let last, last.script == script, last.scheme == scheme, last.selection == selection {
            return last.scan
        }
        let scan = read(script, scheme: scheme, selection: selection)
        last = (script, scheme, selection, scan)
        return scan
    }

    /// The scan last asked for, and what it was asked about. One entry: the
    /// window has one editor, and a buffer that changed is a buffer nothing will
    /// ask about again.
    private static var last: (script: String, scheme: String, selection: Range<Int>, scan: Scan)?

    private static func read(_ script: String, scheme: String, selection: Range<Int>) -> Scan {
        var err: UnsafeMutablePointer<CChar>?
        guard
            let raw = db_sql_scan_json(
                script, scheme, UInt32(clamping: selection.lowerBound),
                UInt32(clamping: selection.upperBound), &err)
        else {
            if let e = err { db_string_free(e) }
            return .empty
        }
        defer { db_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        // An empty scan rather than a trap, for the reason `DriverCatalog` gives
        // for an empty catalogue: the editor then paints nothing and offers
        // nothing to run, which is a poor state but a legible one, where a crash
        // here would take the window down over a keystroke.
        guard let payload = try? JSONDecoder().decode(Payload.self, from: data) else {
            return .empty
        }
        return payload.scan
    }

    /// The wire shape of a scan, mirroring `db_sql_scan_json` in `dbffi.h`.
    ///
    /// Flat arrays of offsets rather than arrays of objects, because this
    /// crosses on every keystroke and an object per token would spend most of
    /// the payload repeating three field names.
    private struct Payload: Decodable {
        /// Kind, start and end for every token, in that order.
        let tokens: [Int]
        /// Start and end for every statement.
        let statements: [Int]
        let target: TargetPayload?

        var scan: Scan {
            var painted: [Token] = []
            var spans: [Range<Int>] = []
            spans.reserveCapacity(tokens.count / 3)
            for i in stride(from: 0, to: tokens.count - 2, by: 3) {
                let range = tokens[i + 1]..<tokens[i + 2]
                spans.append(range)
                if let kind = Token.Kind(coreKind: tokens[i]) {
                    painted.append(Token(kind: kind, range: range))
                }
            }
            var found: [Range<Int>] = []
            found.reserveCapacity(statements.count / 2)
            for i in stride(from: 0, to: statements.count - 1, by: 2) {
                found.append(statements[i]..<statements[i + 1])
            }
            return Scan(statements: found, target: target?.value, tokens: painted, spans: spans)
        }
    }

    private struct TargetPayload: Decodable {
        let start: Int
        let end: Int
        let origin: String
        let index: Int
        let of: Int

        /// An origin this build does not recognise is read as the whole buffer,
        /// which is the reading that describes a one-statement script and is
        /// therefore the one that misleads least.
        var value: Target {
            let range = start..<end
            switch origin {
            case "statement": return Target(range: range, origin: .statement(index, of: of))
            case "selection": return Target(range: range, origin: .selection)
            default: return Target(range: range, origin: .whole)
            }
        }
    }

    // MARK: - Positions

    /// Where a server error position lands in the buffer.
    ///
    /// The arithmetic is the core's, because the trap in it is the core's too:
    /// the server counts from 1 and from the start of the statement it was
    /// handed, not of the buffer that statement was cut from. Nil when the
    /// number could not have come from `sent`.
    static func errorOffset(ofPosition position: Int, in sent: Range<Int>) -> Int? {
        let offset = db_sql_error_offset(
            Int32(clamping: position), UInt32(clamping: sent.lowerBound),
            UInt32(clamping: sent.upperBound))
        return offset < 0 ? nil : Int(offset)
    }

    /// Line and column of a scalar offset, both counted from 1 the way every
    /// error message a user has ever read counts them.
    ///
    /// Not something the core is asked for: a line is a fact about how the text
    /// is displayed rather than about what it means, and the scanner has no
    /// opinion on it.
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
}

extension SQLScript.Scan {
    /// What a buffer nothing could be read out of looks like.
    static let empty = SQLScript.Scan(statements: [], target: nil, tokens: [], spans: [])
}

extension SQLScript.Token.Kind {
    /// The core's token kind, as `db_sql_scan_json` numbers it in `dbffi.h`.
    ///
    /// Nil for the kinds the editor leaves at its default colour, which is most
    /// of them. The numbers are the one part of that surface a compiler cannot
    /// check on either side, so `--verify-splitter` paints a script holding all
    /// six and compares — a renumbering would otherwise show up as a buffer in
    /// the wrong colours and nothing else.
    fileprivate init?(coreKind: Int) {
        switch coreKind {
        case 1: self = .keyword
        case 3: self = .quotedIdentifier
        case 4: self = .string
        case 5: self = .dollarQuoted
        case 6: self = .number
        case 7: self = .comment
        default: return nil
        }
    }
}
