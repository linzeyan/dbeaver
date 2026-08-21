import Foundation

/// Secrets held for the life of this process and never written down.
///
/// A connection with `savesPassword` off has nowhere on disk for its secrets to
/// live, and being asked again on every reconnect within one sitting is what
/// makes people turn the flag back on. This is the middle: typed once, kept
/// until the application quits, gone with it.
///
/// Both of a connection's secrets, because the flag is one answer about one
/// connection: an arrangement where the database password was held and the
/// bastion's was asked for again would be the flag keeping half its promise, on
/// the connections where it matters most.
///
/// Not a Keychain item with an ephemeral attribute, and not a file under the
/// app's data directory: the whole of what the flag promises is that nothing
/// outlives the process, and memory this process owns is the only store that
/// cannot break that promise by accident.
@MainActor
enum SessionPasswords {
    /// Keyed by the same account string the Keychain uses, so that the two
    /// stores cannot come to disagree about which secret is which. A connection
    /// behind a bastion has two of them and they are not interchangeable.
    private static var held: [String: String] = [:]

    /// Empty is forgotten rather than stored, so that "nothing held" and "held,
    /// and it is blank" stay one answer — which is what `ConnectionKeychain`
    /// already does with an empty string, and two stores disagreeing about that
    /// would show up as a form that fills itself in with nothing.
    static func remember(
        _ password: String, for id: UUID, _ secret: ConnectionKeychain.Secret = .password
    ) {
        if password.isEmpty {
            held.removeValue(forKey: key(id, secret))
        } else {
            held[key(id, secret)] = password
        }
    }

    static func password(for id: UUID, _ secret: ConnectionKeychain.Secret = .password) -> String? {
        held[key(id, secret)]
    }

    /// Both of them. A connection being forgotten with one secret still in memory
    /// is a secret nothing will ever hand back or clear, for the rest of the
    /// process.
    static func forget(_ id: UUID) {
        for secret in [ConnectionKeychain.Secret.password, .ssh] {
            held.removeValue(forKey: key(id, secret))
        }
    }

    private static func key(_ id: UUID, _ secret: ConnectionKeychain.Secret) -> String {
        id.uuidString + secret.rawValue
    }
}
