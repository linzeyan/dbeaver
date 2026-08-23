import Foundation

/// Decides whether a statement is a plain read, for the MCP `query` tool.
///
/// This is a statement filter, not a sandbox, and the difference is stated
/// where users can see it: a stored function can hide any write from a filter,
/// and the only hard boundary is connecting as a read-only database user. What
/// the filter is for is the everyday case — an agent that was asked to look
/// should not be able to change anything by accident or by prompt injection.
///
/// It fails closed, and every ambiguity below is resolved toward rejection.
/// The app speaks fifteen dialects through one guard, and the dialects
/// disagree about comments and escapes; each disagreement is settled by asking
/// which reading could let a write through on *some* server, and refusing
/// that reading:
///
/// - `#` is not a comment here. It comments a line on MySQL but is an
///   operator on PostgreSQL, so treating it as a comment would hide
///   `# 2; DROP …` from the guard while PostgreSQL executes it.
/// - `--` comments only when followed by whitespace or end of line, which is
///   MySQL's rule and the strictest one. `--x` stays visible, and a statement
///   PostgreSQL would have treated as commented is merely rejected.
/// - Block comments do not nest, which is MySQL's rule. Nesting would end a
///   PostgreSQL comment early at worst — a rejection — where the reverse
///   reading hides real statements from the guard on MySQL.
/// - Backslash does not escape a quote, which is the SQL standard. MySQL
///   strings containing `\'` therefore read as ending early and are rejected;
///   the other reading would let PostgreSQL text smuggle a statement inside
///   what the guard took for a string.
///
/// The rejections those rules cost — and `WITH`, and a bare column named
/// `analyze` or `share` — are accepted false positives, pinned as such in the
/// checks. A classifier that parses instead of scans belongs in the core and
/// arrives with write tiers, not before.
enum MCPReadOnlyGuard {

    /// Why this SQL may not run over MCP, or nil for a plain read.
    static func obstacle(in sql: String) -> String? {
        let stripped: String
        switch strip(sql) {
        case .unterminated:
            return "Unterminated quote or comment. Refused rather than guessed at."
        case .executableComment:
            return
                "Executable comments (/*! … */) run on the server. "
                + "The guard does not relay what it cannot read."
        case .clean(let text):
            stripped = text
        }
        var statement = stripped.trimmingCharacters(in: .whitespacesAndNewlines)
        if statement.hasSuffix(";") {
            statement = String(statement.dropLast()).trimmingCharacters(
                in: .whitespacesAndNewlines)
        }
        if statement.contains(";") {
            return "One statement at a time over MCP."
        }

        let tokens = words(of: statement)
        guard let verb = tokens.first else {
            return "Nothing to run."
        }
        switch verb {
        case "SHOW":
            // SHOW takes no subquery on any dialect that has it, so the verb
            // is the whole verdict — and skipping the token scan is what lets
            // SHOW CREATE TABLE through, which is a read despite its middle.
            return nil
        case "SELECT", "DESCRIBE", "DESC", "EXPLAIN":
            if let caught = tokens.first(where: { forbidden.contains($0) }) {
                return "\(caught) does not belong in a read."
            }
            return nil
        case "WITH":
            return
                "WITH can carry writes in its body. "
                + "Refused until the guard can parse rather than scan."
        default:
            return "Only reads run over MCP: SELECT, SHOW, EXPLAIN, DESCRIBE."
        }
    }

    // MARK: - Scanning

    /// Every token that has no business inside a read, uppercased.
    ///
    /// Broader than the write verbs: session state (SET, USE), transactions,
    /// files (OUTFILE, COPY), locking reads (UPDATE and SHARE catch FOR
    /// UPDATE / FOR SHARE / LOCK IN SHARE MODE), and ANALYZE, which is how
    /// EXPLAIN ANALYZE — an EXPLAIN that executes its target — is refused.
    /// INTO is here because SELECT INTO creates a table and no whitelisted
    /// verb uses the word legitimately.
    private static let forbidden: Set<String> = [
        "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "UPSERT",
        "DROP", "ALTER", "CREATE", "TRUNCATE", "RENAME",
        "GRANT", "REVOKE", "SET", "USE", "CALL", "EXEC", "EXECUTE",
        "COPY", "IMPORT", "LOAD", "LOAD_FILE", "OUTFILE", "DUMPFILE", "INTO",
        "VACUUM", "ANALYZE", "ANALYSE", "REINDEX", "CLUSTER", "CHECKPOINT",
        "LOCK", "SHARE", "BEGIN", "COMMIT", "ROLLBACK", "START", "SAVEPOINT",
        "ATTACH", "DETACH", "PRAGMA", "KILL", "DO", "INSTALL", "RESET",
        "DECLARE", "PREPARE", "DEALLOCATE", "REFRESH",
        "LISTEN", "NOTIFY", "UNLISTEN"
    ]

    /// What became of the statement once its opaque regions were blanked.
    private enum Stripped {
        case clean(String)
        case unterminated
        case executableComment
    }

    /// Replaces string bodies, quoted identifiers and comments with spaces so
    /// the token scan cannot be fooled by what is inside them — and, just as
    /// deliberately, cannot be *hidden from* by them.
    private static func strip(_ sql: String) -> Stripped {
        var out = ""
        let chars = Array(sql)
        var i = 0
        while i < chars.count {
            let c = chars[i]
            switch c {
            case "'", "\"", "`":
                // Doubling is the one escape every dialect agrees on.
                out.append(" ")
                i += 1
                var closed = false
                while i < chars.count {
                    if chars[i] == c {
                        if i + 1 < chars.count && chars[i + 1] == c {
                            i += 2
                            continue
                        }
                        i += 1
                        closed = true
                        break
                    }
                    i += 1
                }
                if !closed { return .unterminated }
            case "$":
                // A PostgreSQL dollar quote: $tag$ … $tag$, tag empty or
                // starting with a letter. $1 stays a parameter.
                if let (tag, afterOpen) = dollarTag(chars, at: i) {
                    guard let end = find(tag, in: chars, from: afterOpen) else {
                        return .unterminated
                    }
                    out.append(" ")
                    i = end
                } else {
                    out.append(c)
                    i += 1
                }
            case "-":
                if i + 1 < chars.count && chars[i + 1] == "-"
                    && (i + 2 >= chars.count || chars[i + 2] == " " || chars[i + 2] == "\t"
                        || chars[i + 2] == "\n" || chars[i + 2] == "\r")
                {
                    while i < chars.count && chars[i] != "\n" { i += 1 }
                    out.append(" ")
                } else {
                    out.append(c)
                    i += 1
                }
            case "/":
                if i + 1 < chars.count && chars[i + 1] == "*" {
                    let after = i + 2
                    if after < chars.count
                        && (chars[after] == "!"
                            || (chars[after] == "M" && after + 1 < chars.count
                                && chars[after + 1] == "!"))
                    {
                        return .executableComment
                    }
                    var j = after
                    var closed = false
                    while j + 1 < chars.count {
                        if chars[j] == "*" && chars[j + 1] == "/" {
                            closed = true
                            break
                        }
                        j += 1
                    }
                    if !closed { return .unterminated }
                    out.append(" ")
                    i = j + 2
                } else {
                    out.append(c)
                    i += 1
                }
            default:
                out.append(c)
                i += 1
            }
        }
        return .clean(out)
    }

    /// The tag of a dollar quote opening at `i`, and the index just past it —
    /// or nil where `$` is only a parameter marker.
    private static func dollarTag(_ chars: [Character], at i: Int) -> ([Character], Int)? {
        var j = i + 1
        var tag: [Character] = []
        while j < chars.count, chars[j].isLetter || chars[j].isNumber || chars[j] == "_" {
            tag.append(chars[j])
            j += 1
        }
        guard j < chars.count, chars[j] == "$" else { return nil }
        if let first = tag.first, first.isNumber { return nil }
        return (tag, j + 1)
    }

    /// The index just past the closing `$tag$`, or nil if it never comes.
    private static func find(_ tag: [Character], in chars: [Character], from start: Int) -> Int? {
        let closer: [Character] = ["$"] + tag + ["$"]
        var i = start
        while i + closer.count <= chars.count {
            if Array(chars[i..<i + closer.count]) == closer { return i + closer.count }
            i += 1
        }
        return nil
    }

    /// The statement as uppercase word tokens: what the verb check and the
    /// forbidden scan both read, and the only view of the SQL either gets.
    private static func words(of statement: String) -> [String] {
        var tokens: [String] = []
        var current = ""
        for c in statement {
            if c.isLetter || c.isNumber || c == "_" {
                current.append(c)
            } else if !current.isEmpty {
                tokens.append(current.uppercased())
                current = ""
            }
        }
        if !current.isEmpty { tokens.append(current.uppercased()) }
        return tokens
    }
}
