import Foundation
import Observation

/// What a recorded statement did.
///
/// `StatementOutcome` with the one case that has no place here dropped:
/// `.notRun` describes a statement the server never saw, and a history holding
/// one would claim a run that did not happen. The failure's message is not kept
/// either — what brings a user back to a failed statement is the statement
/// itself, to fix and send again, and the banner answered the error when it was
/// raised.
enum QueryHistoryOutcome: Codable, Equatable {
    /// Returned a result set holding this many rows. Zero is a real result set
    /// with no rows in it, which is not the same answer as having none.
    case rows(Int)
    /// Returned no result set. The count is what the server said it affected.
    case affected(Int)
    case failed
    /// Stopped on request. Kept apart from `failed` for the reason the run's own
    /// list keeps them apart, and because it is the one entry whose statement is
    /// worth coming back to unedited: nothing was wrong with it.
    case cancelled

    var isFailure: Bool { self == .failed }

    /// What the list's outcome column reads. Short on purpose: it shares a row
    /// with the statement, which is the part being scanned.
    @MainActor
    var label: String {
        switch self {
        case .rows(let n): return AppModel.pluralized(n, "row")
        case .affected(let n):
            return n == 0 ? "no rows" : "\(AppModel.pluralized(n, "row")) affected"
        case .failed: return "failed"
        case .cancelled: return "cancelled"
        }
    }

    /// A run's outcome as the history keeps it, or nil for a statement that
    /// never went out. Living here rather than at the call site is what keeps
    /// the missing `.notRun` case explained next to the type that omits it.
    @MainActor
    init?(_ outcome: StatementOutcome) {
        switch outcome {
        case .rows(let n): self = .rows(n)
        // The rows the server sent, not the ones this window kept. A history
        // answers "what did this application run on my database", and how much
        // of the answer the pane had room for is not a fact about the database.
        case .released(let n): self = .rows(n)
        case .completed(let affected): self = .affected(affected)
        case .failed: self = .failed
        case .cancelled: self = .cancelled
        case .notRun: return nil
        }
    }
}

/// What caused a statement to be sent.
///
/// The Query pane is not the only thing this window sends. A browse is a SELECT
/// somebody caused by clicking a table without typing a word of it, and an edit
/// is an UPDATE the core wrote for changes staged in a grid — and "what did this
/// application just run on my database" is a question about all of them. A
/// history that answered it only for the typed ones would be answering an easier
/// question than the one being asked.
///
/// Three cases and not one per call site: what a reader wants to know is whether
/// they wrote it, whether looking at a table caused it, or whether it changed
/// something. Which button was pressed is a finer distinction than the list has
/// room to draw.
enum QueryHistoryOrigin: String, Codable, Equatable, CaseIterable {
    /// Typed into the Query pane and sent with ⌘R or ⌥⌘R.
    case query
    /// The browse's own SELECT, sent by choosing a table or pressing Apply.
    case browse
    /// An INSERT, UPDATE or DELETE the core wrote for staged grid changes.
    case edit
}

/// One statement this window sent, and what came back.
struct QueryHistoryEntry: Codable, Identifiable, Equatable {
    let id: UUID
    /// The statement as sent, verbatim. This is what goes back in the editor,
    /// so it is kept exactly as the server saw it rather than reformatted.
    let sql: String
    /// When it went out, absolute. The list reads it as "8m ago", which is what
    /// a user recognises a statement by, but a stored duration would be wrong
    /// the moment the window was closed.
    let ranAt: Date
    /// What caused it.
    let origin: QueryHistoryOrigin
    /// What the server took, as the run measured it.
    ///
    /// Zero means nothing measured it — a statement that failed before it was
    /// timed — rather than a statement that took no time. The two have to read
    /// differently wherever this is drawn, because "0 ms" is the fastest thing
    /// on the list and "we never found out" is not a speed at all.
    let milliseconds: Double
    let outcome: QueryHistoryOutcome

    /// The statement on one line, for the list. Whitespace is collapsed rather
    /// than trusted to `lineLimit`, for the reason `ScriptStep.preview` gives:
    /// the first line of a formatted statement is often just `SELECT`.
    var preview: String {
        sql.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }
}

/// The statements this window has sent, across launches.
///
/// The Query pane used to forget a statement the moment the next one ran, and
/// the whole buffer the moment the process ended — so the one thing a user
/// reliably wants back, the statement they typed correctly twenty minutes ago,
/// was the one thing the application did not keep.
///
/// Stored in `UserDefaults.standard` under a key of its own rather than in a
/// preferences model shared with everything else: one JSON blob under one key
/// is all this needs, and a shared container is a place for two features being
/// written at the same time to collide.
@Observable
@MainActor
final class QueryHistory {
    /// Newest first, which is both how the list reads and how the duplicate
    /// test is answered — the only entry it ever has to look at is the front.
    private(set) var entries: [QueryHistoryEntry] = []

    /// How many statements are kept, or 0 for all of them.
    ///
    /// Read from the same store the entries are in rather than handed in. The
    /// Settings window holds a `Preferences` and nothing else, and the histories
    /// are built in `main.swift` before any window exists — a setting that had
    /// to be passed from one to the other would need a path between two things
    /// that never meet. `Preferences` writes the key; this reads it where it is
    /// used, so a check driving a scratch suite sets it the same way.
    var limit: Int { Preferences.historyLimit(in: defaults) }

    /// And how many of those may be statements nobody typed.
    ///
    /// A browse runs every time a table is picked, so a single cap on the total
    /// would leave the list all browses within a minute of opening a database —
    /// and the statement somebody typed twenty minutes ago, which is the one
    /// thing this exists to give back, would have been pushed off the end by the
    /// sidebar. Half the list is reserved for the typed ones by capping the
    /// others at half.
    ///
    /// Derived rather than a second setting: the rule is "half the list", and
    /// two numbers somebody could set independently would let them reserve more
    /// room for browses than the list has.
    var untypedLimit: Int { max(limit / 2, 1) }

    /// What the cap is for somebody who has never chosen one. Far more than
    /// anyone scrolls, and small enough that the whole list decodes at launch
    /// without being noticed.
    static let defaultLimit = 200

    /// Not private, so that `--verify-query-history` can write what an earlier
    /// build would have left here and read back what this one wrote. A check
    /// that spelled the key itself would be a second copy of it, and the day
    /// they disagree is the day the check looks like it passes.
    static let key = "dev.dbclient.queryHistory"

    private let defaults: UserDefaults

    /// The store is injectable so that a capture can be given a scratch one; see
    /// `--history-store`. Everything else takes the default.
    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        entries = Self.load(from: defaults)
        // The cap can have been lowered since the last launch, in Settings or
        // by hand in the plist, and what was loaded is held to it before
        // anything reads it. Written back only when it actually shortened, so
        // an ordinary launch touches nothing.
        let loaded = entries.count
        trim()
        if entries.count != loaded { save() }
    }

    /// Records a statement that was actually sent.
    ///
    /// A failure is recorded like any other outcome, and is the entry most worth
    /// keeping: the statement with the typo in it is precisely the one someone
    /// comes back for.
    ///
    /// The statement is kept exactly as it was sent. Every entry here is
    /// something a person may want to run again — that is what a history is for,
    /// and it is the same list Recall puts back into the editor — so a statement
    /// this store had edited would be one that no longer does what it did. A
    /// password typed into the Query tab is therefore written to the plist as
    /// typed, and `limit` — how many are kept — is the only thing bounding how
    /// long it stays there. limitations.md says what that costs.
    func record(
        _ sql: String, from origin: QueryHistoryOrigin, outcome: QueryHistoryOutcome,
        milliseconds: Double, at ranAt: Date = Date()
    ) {
        let statement = sql.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !statement.isEmpty else { return }
        let entry = QueryHistoryEntry(
            id: UUID(), sql: statement, ranAt: ranAt, origin: origin,
            milliseconds: milliseconds, outcome: outcome)
        // A statement run again replaces its own entry rather than stacking a
        // second one: pressing ⌘R four times while fixing a table would
        // otherwise push everything else off the top of the list with four
        // copies of one statement. Replaced rather than left alone, because the
        // newer run is the true one — its outcome may differ, and an entry still
        // reading "8m ago" for something sent a second ago is simply wrong.
        //
        // The origin has to match as well. The same SELECT can be typed into the
        // Query pane and produced by the browse, and folding those two together
        // would answer "did I run this or did the sidebar" with whichever came
        // second.
        if entries.first?.sql == statement, entries.first?.origin == origin {
            entries[0] = entry
        } else {
            entries.insert(entry, at: 0)
        }
        trim()
        save()
    }

    /// Drops whatever the two caps say cannot be kept, newest first.
    ///
    /// The untyped cap is applied on the way through rather than afterwards, so
    /// a hundred-and-first browse falls out while a typed statement below it
    /// stays. Written as one pass with an explicit order because that order is
    /// the whole rule: `removeAll(where:)` does not promise to visit in
    /// sequence, and a rule about which of two entries survives cannot be built
    /// on a predicate whose call order is unspecified.
    private func trim() {
        let limit = limit
        // Zero is "keep everything", which is the only thing an emptied field
        // could honestly mean for a cap. Nothing is dropped, and the list grows
        // for as long as somebody keeps running statements.
        guard limit > 0 else { return }
        let untypedLimit = untypedLimit
        var untyped = 0
        var kept: [QueryHistoryEntry] = []
        kept.reserveCapacity(min(entries.count, limit))
        for entry in entries {
            if entry.origin != .query {
                untyped += 1
                if untyped > untypedLimit { continue }
            }
            kept.append(entry)
            if kept.count == limit { break }
        }
        entries = kept
    }

    /// How a timestamp is written into an exported file.
    ///
    /// ISO 8601 in the local zone: unambiguous, sortable, and the string a `sort`
    /// or a spreadsheet orders correctly without being told how. The list on
    /// screen says "8m ago", which is how a statement is recognised and is
    /// useless in a file somebody opens next week.
    private static let stamped: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [
            .withFullDate, .withTime, .withDashSeparatorInDate, .withColonSeparatorInTime,
            .withSpaceBetweenDateAndTime
        ]
        formatter.timeZone = .current
        return formatter
    }()

    /// The entries as a file somebody can read and re-run.
    ///
    /// SQL rather than the JSON the saved queries use. That file is a list this
    /// application reads back, so its shape is this application's business; this
    /// one is evidence, and both things anybody does with evidence — read what
    /// happened, run one of them again — want the statements themselves with the
    /// facts above them as comments.
    ///
    /// Newest first, exactly as the panel drew it. The other order would read as
    /// a transcript to be replayed top to bottom, and this is a record of what a
    /// window did, not a migration.
    static func script(_ entries: [QueryHistoryEntry], at stamp: Date = Date()) -> String {
        var lines = [
            "-- dbclient statement log · \(stamped.string(from: stamp))",
            "-- \(AppModel.pluralized(entries.count, "statement"))"
        ]
        for entry in entries {
            // Milliseconds at every size, and nothing at all where nobody
            // measured it. One unit is what lets a file like this be sorted or
            // added up; the panel switches to seconds past a thousand because it
            // is read at a glance, and a file is not.
            let took = entry.milliseconds > 0 ? " · \(Int(entry.milliseconds)) ms" : ""
            lines.append("")
            lines.append(
                "-- \(entry.origin.rawValue) · \(stamped.string(from: entry.ranAt))"
                    + "\(took) · \(entry.outcome.label)")
            // Terminated, because what is written here is a script and the
            // statements arrive one at a time without one. Only where it is
            // missing: whether a step keeps its own semicolon depends on how the
            // script it came from was split, and `;;` is a syntax error on
            // servers that do not read it as an empty statement.
            lines.append(entry.sql.hasSuffix(";") ? entry.sql : entry.sql + ";")
        }
        return lines.joined(separator: "\n") + "\n"
    }

    /// Drops everything, here and on disk. Irreversible, which is why the panel
    /// asks before calling it.
    func clear() {
        entries = []
        defaults.removeObject(forKey: Self.key)
    }

    private func save() {
        // A failure here costs the user their history and nothing else, so it is
        // not worth a banner interrupting the result they just ran — and the
        // only way `[QueryHistoryEntry]` fails to encode is a bug in this file.
        guard let data = try? JSONEncoder().encode(entries) else { return }
        defaults.set(data, forKey: Self.key)
    }

    /// Reads what a previous launch wrote, and starts empty when it cannot.
    ///
    /// Unreadable data is dropped rather than migrated: this is a convenience
    /// that has never been anything else, and an application that refuses to
    /// open because a build from last week wrote a different shape would be a
    /// far worse trade than one that opens having forgotten.
    /// Returns all of what was stored; the cap is `init`'s to apply, since it is
    /// the one that then writes the shortened list back.
    private static func load(from defaults: UserDefaults) -> [QueryHistoryEntry] {
        guard let data = defaults.data(forKey: key),
            let decoded = try? JSONDecoder().decode([QueryHistoryEntry].self, from: data)
        else { return [] }
        return decoded
    }
}
