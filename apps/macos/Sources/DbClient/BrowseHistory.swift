import Foundation

/// One place this window has been: a relation, and the tab it was shown on.
///
/// The tab is part of where you were, not something restored alongside it.
/// Reading a table's structure and then browsing its rows are two places a
/// person went, and Back from the second means the first — not the table before
/// it.
///
/// The relation is an id rather than a `RelationInfo` because the history
/// outlives the values in the sidebar: a refresh replaces every one of them, and
/// a path holding the old ones would send Back to a table that no longer
/// compares equal to anything on screen.
struct Visit: Equatable {
    let relationID: String
    let tab: DetailTab
}

/// Where this window has been, and where Back and Forward go.
///
/// One list and a cursor, rather than a back stack and a forward stack. The two
/// shapes behave identically; this one can be checked by reading it, because the
/// list is the path and the cursor says where on it we are.
struct BrowseHistory: Equatable {
    /// The path, oldest first.
    private(set) var visits: [Visit] = []

    /// Where on the path we are, as an index into `visits`. Nil before anything
    /// has been visited, which is the only state in which `current` is nil.
    private(set) var position: Int?

    var current: Visit? { position.map { visits[$0] } }

    var canGoBack: Bool { (position ?? 0) > 0 }

    var canGoForward: Bool {
        guard let position else { return false }
        return position + 1 < visits.count
    }

    /// Records arriving somewhere.
    ///
    /// Arriving anywhere after going Back discards what was ahead, which is how
    /// browsers and editors both behave: the forward path described a route this
    /// visit has just left.
    ///
    /// Arriving where we already are records nothing. A refresh re-selects the
    /// same relation, and a tab click may land on the tab already showing;
    /// without this, each would leave an entry that Back walks through while the
    /// window appears not to move.
    mutating func visit(_ visit: Visit) {
        guard current != visit else { return }
        if let position { visits.removeSubrange((position + 1)...) }
        visits.append(visit)
        position = visits.count - 1
    }

    /// Steps back and returns where to go, or nil at the start of the path.
    ///
    /// Returns the destination rather than moving the cursor and leaving the
    /// caller to read `current`, so that "there was nowhere to go" and "go here"
    /// are one answer that cannot be acted on in the wrong order.
    mutating func goBack() -> Visit? {
        guard canGoBack, let position else { return nil }
        self.position = position - 1
        return current
    }

    /// Steps forward and returns where to go, or nil at the end of the path.
    mutating func goForward() -> Visit? {
        guard canGoForward, let position else { return nil }
        self.position = position + 1
        return current
    }

    /// Forgets the path, for a window that has connected somewhere else.
    ///
    /// The ids are `schema.name` strings, and the same string names a different
    /// table on a different server — a Back that crossed a reconnection would
    /// open the wrong table under the right name.
    mutating func clear() {
        visits = []
        position = nil
    }
}
