import Foundation

/// Passwords held for the life of this process and never written down.
///
/// A connection with `savesPassword` off has nowhere on disk for its secret to
/// live, and being asked again on every reconnect within one sitting is what
/// makes people turn the flag back on. This is the middle: typed once, kept
/// until the application quits, gone with it.
///
/// Not a Keychain item with an ephemeral attribute, and not a file under the
/// app's data directory: the whole of what the flag promises is that nothing
/// outlives the process, and memory this process owns is the only store that
/// cannot break that promise by accident.
@MainActor
enum SessionPasswords {
    private static var held: [UUID: String] = [:]

    /// Empty is forgotten rather than stored, so that "nothing held" and "held,
    /// and it is blank" stay one answer — which is what `ConnectionKeychain`
    /// already does with an empty string, and two stores disagreeing about that
    /// would show up as a form that fills itself in with nothing.
    static func remember(_ password: String, for id: UUID) {
        if password.isEmpty {
            held.removeValue(forKey: id)
        } else {
            held[id] = password
        }
    }

    static func password(for id: UUID) -> String? {
        held[id]
    }

    static func forget(_ id: UUID) {
        held.removeValue(forKey: id)
    }
}
