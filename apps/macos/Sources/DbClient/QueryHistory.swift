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

    /// How many statements are kept. Far more than anyone scrolls, and small
    /// enough that the whole list decodes at launch without being noticed. Past
    /// it the oldest go, because a history is read from the top.
    static let limit = 200

    /// And how many of those may be statements nobody typed.
    ///
    /// A browse runs every time a table is picked, so a single cap on the total
    /// would leave the list all browses within a minute of opening a database —
    /// and the statement somebody typed twenty minutes ago, which is the one
    /// thing this exists to give back, would have been pushed off the end by the
    /// sidebar. Half the list is reserved for the typed ones by capping the
    /// others at half.
    static let untypedLimit = 100

    private static let key = "dev.dbclient.queryHistory"

    private let defaults: UserDefaults

    /// The store is injectable so that a capture can be given a scratch one; see
    /// `--history-store`. Everything else takes the default.
    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        entries = Self.load(from: defaults)
    }

    /// Records a statement that was actually sent.
    ///
    /// A failure is recorded like any other outcome, and is the entry most worth
    /// keeping: the statement with the typo in it is precisely the one someone
    /// comes back for.
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
        var untyped = 0
        var kept: [QueryHistoryEntry] = []
        kept.reserveCapacity(min(entries.count, Self.limit))
        for entry in entries {
            if entry.origin != .query {
                untyped += 1
                if untyped > Self.untypedLimit { continue }
            }
            kept.append(entry)
            if kept.count == Self.limit { break }
        }
        entries = kept
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
    private static func load(from defaults: UserDefaults) -> [QueryHistoryEntry] {
        guard let data = defaults.data(forKey: key),
            let decoded = try? JSONDecoder().decode([QueryHistoryEntry].self, from: data)
        else { return [] }
        return Array(decoded.prefix(limit))
    }
}
