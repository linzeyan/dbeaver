import Foundation

/// Throwaway `UserDefaults` suites for the checks, and the cleanup they were all
/// missing.
///
/// A suite is a persistent defaults domain like any other: `UserDefaults(suiteName:)`
/// writes a plist under `~/Library/Preferences` and nothing removes it again. The
/// checks mint one per model per run, so `make test-swift` was leaving several
/// hundred domains behind for good — the same defect as a Keychain item a check
/// saves and never deletes, in a different store.
///
/// The names come from one place because that is what makes `release` possible.
/// Five suites each remembering their own was five chances to forget, and four
/// of them did.
enum ScratchDefaults {
    /// The object as well as the name, because `release` needs both. See there.
    private static var minted: [(name: String, store: UserDefaults)] = []

    /// A suite nothing else is using, remembered so `release` can drop it.
    ///
    /// `label` says which check suite asked for it, so a domain that does
    /// survive — a crash between minting and releasing — can be traced back to
    /// the file that made it.
    static func store(_ label: String) -> UserDefaults {
        let name = "dev.dbclient.\(label).\(UUID().uuidString)"
        // Force-unwrapped, as every call site here already did: the only names
        // `UserDefaults(suiteName:)` refuses are the standard domains, and this
        // one carries a UUID.
        let store = UserDefaults(suiteName: name)!
        minted.append((name, store))
        return store
    }

    /// Drops every suite handed out so far.
    ///
    /// Reached through `defer` in each suite's `run`, so one that fails half way
    /// tidies up as thoroughly as one that passes — cleanup that only happens on
    /// success happens exactly when it is not needed.
    ///
    /// Removed through the suite's *own* object, not through `.standard`. The
    /// two are not the same operation: `.standard` deletes the plist and leaves
    /// the suite object holding the values it had, and that object is still
    /// alive — the checks hand it to a `Preferences` or a `QueryFavorites` that
    /// outlives the call — so the values are written straight back out when the
    /// process ends. Measured, not assumed: with `.standard` a full run left
    /// exactly the fourteen suites that had been written to and none of the ones
    /// that had not.
    static func release() {
        for (name, store) in minted {
            store.removePersistentDomain(forName: name)
            store.synchronize()
            // And then the domain itself, or the file comes back. Emptying a
            // suite leaves `cfprefsd` still holding it, and what it holds it
            // writes out again when the process ends — measured on
            // 2026-08-24 as one 42-byte plist per store minted, on every run
            // of every suite, which is where the six thousand that had
            // accumulated came from.
            UserDefaults.standard.removeSuite(named: name)
            // And then the file, because the call above does not remove it.
            // Emptying a domain leaves the plist `cfprefsd` wrote for it on
            // disk — 42 bytes holding no keys, which `defaults read` reports as
            // "does not exist" and `defaults domains` goes on listing for ever.
            // Measured: without this line a full `make test-swift` left exactly
            // fourteen of them behind, one per suite that had been written to,
            // and the count grew by fourteen on every run after that.
            //
            // The path is not a guess. It is the documented layout for a named
            // suite, and these are files this type asked for by name a moment
            // ago — nothing else can be at that path.
            let file = URL(filePath: NSHomeDirectory())
                .appending(path: "Library/Preferences/\(name).plist")
            try? FileManager.default.removeItem(at: file)
        }
        minted.removeAll()
    }
}
