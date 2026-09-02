import Foundation

/// One editor buffer, as it was when the window closed.
///
/// The name as well as the text, because a buffer somebody renamed to `migration`
/// coming back as `query 3` is a buffer they have to identify again by reading it.
struct RestoredBuffer: Codable, Equatable {
    var name: String
    var text: String
}

/// One tab: what it was pointed at, and what was typed in it.
///
/// Not what it was *showing*. The selected table, the filters and the paging
/// position are all answers a server gave, and a restored tab has not asked one —
/// putting them back would be drawing a table's rows under a form that has not
/// connected. What survives is the two things nothing else can rebuild: which
/// connection this tab was for, and the SQL in it.
struct RestoredTab: Codable, Equatable {
    /// The saved connection this tab was opened from, or nil for one dialled by
    /// hand.
    ///
    /// An id rather than a copy of the fields. A connection deleted between the
    /// two launches is a connection somebody deleted, and a tab that put its
    /// host, port and user back on screen would be undoing that — so an id that
    /// names nothing restores an empty form rather than the entry it used to
    /// name.
    var connection: UUID?

    /// The fields a tab with no saved entry was dialled with, and nil where
    /// there is an entry to read them from.
    ///
    /// Quick connect and `--conn` have nowhere else to keep them. There is no
    /// password here and there is no field for one: `ConnectionSettings` has
    /// never carried the secret, which is what makes it the half of a connection
    /// that can be written to a plain file.
    var settings: ConnectionSettings?

    /// What the tab was called, so the strip reads the same on the way back in.
    ///
    /// Kept rather than derived. The name on a saved entry is whatever somebody
    /// typed, and rebuilding the label from the address would rename every tab
    /// on the first launch after this feature existed.
    var label: String

    var buffers: [RestoredBuffer]

    /// Which of them the editor was in. Folded into the list on the way back in
    /// rather than trusted: this is a file somebody can edit.
    var activeBuffer: Int
}

/// What one window had open.
///
/// Every restored tab is unconnected. Nothing here dials anything, because a
/// client that opens five connections because it was launched is a client that
/// touches a production server before anybody has looked at the screen — and
/// "no connecting on launch" is a rule this application already had. What restore
/// saves is the retyping: the form is filled in, the password is in the Keychain,
/// and Enter connects.
struct RestoredWindow: Codable, Equatable {
    var tabs: [RestoredTab]
    var activeTab: Int
}

/// The file: every window that was open, in the order they were made.
///
/// A wrapper around the list rather than a bare list, for the reason
/// `SavedConnections` gives: the document needs somewhere to say which shape it
/// is in.
struct RestoredWindows: Codable, Equatable {
    /// The shape this build writes and will read back. A file numbered anything
    /// else is ignored rather than migrated, which is `NavigatorCache`'s rule and
    /// is right here for the same reason: the cost of ignoring it is one launch
    /// that opens empty, and the code that would migrate it is code that has to
    /// be right about a format nobody can see.
    static let currentVersion = 2

    var version = RestoredWindows.currentVersion
    var windows: [RestoredWindow]
}

/// Where the restored window is kept.
///
/// Under `$XDG_CONFIG_HOME/dbclient`, beside `connections.json` rather than in
/// the cache directory, and the rule that decides it is the one `NavigatorCache`
/// states: whether somebody would miss it. Half of this file is SQL that was
/// never saved anywhere, and that is missed the moment it is gone — a navigator
/// tree is not.
///
/// Its own file rather than a field in `connections.json`, which is a list of
/// servers somebody may carry to another machine and has no business holding what
/// was on this screen. A file rather than `UserDefaults`, because a buffer can be
/// hundreds of kilobytes and the defaults system reads a whole domain into every
/// process that asks it one question.
struct SessionRestoreStore {
    let file: URL

    static var system: SessionRestoreStore {
        SessionRestoreStore(
            file: ConnectionDirectories.localDirectory(
                xdgConfigHome: ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"],
                home: FileManager.default.homeDirectoryForCurrentUser
            ).appending(path: "dbclient/session.json"))
    }

    /// What was open last time, or nothing where there is nothing to put back —
    /// no file, an unreadable one, or one written in a shape this build does not
    /// know.
    ///
    /// A window with no tabs in it is dropped rather than restored. A window is a
    /// list of tabs and a pointer into it, so a list of none describes no window,
    /// and read literally it would leave the pointer aimed past the end.
    func load() -> [RestoredWindow] {
        guard let data = try? Data(contentsOf: file),
            let document = try? JSONDecoder().decode(RestoredWindows.self, from: data),
            document.version == RestoredWindows.currentVersion
        else { return [] }
        return document.windows.filter { !$0.tabs.isEmpty }
    }

    /// Failures are silent, for the reason `NavigatorCache.save` gives: this runs
    /// while the process is ending, and there is no window left to report into.
    func save(_ windows: [RestoredWindow]) {
        guard let data = try? JSONEncoder().encode(RestoredWindows(windows: windows)) else {
            return
        }
        try? FileManager.default.createDirectory(
            at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: file, options: [.atomic])
    }

    /// Forgets what was open, for somebody who has turned restore off.
    ///
    /// Deleting rather than merely not writing. Turning the setting off is asking
    /// for the last session's SQL not to be kept, and a file left behind from
    /// before would go on keeping it.
    func clear() {
        try? FileManager.default.removeItem(at: file)
    }

    /// The windows a launch should put back, which is none when the setting says
    /// so.
    ///
    /// The setting is answered here rather than at the call site because that
    /// call site builds `NSWindow`s: this is the half of the decision that can be
    /// checked with nothing on screen, and it is the half that can be wrong.
    func windowsToRestore(restoring: Bool) -> [RestoredWindow] {
        restoring ? load() : []
    }

    /// Writes down what was open, or clears the file where nothing is to be kept.
    ///
    /// Both answers in one place for the reason above, and clearing rather than
    /// merely declining to write for the reason `clear` gives.
    func remember(_ windows: [RestoredWindow], restoring: Bool) {
        guard restoring else { return clear() }
        save(windows)
    }
}
