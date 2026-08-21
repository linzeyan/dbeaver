import CryptoKit
import Foundation

/// The database passwords this Mac keeps for itself.
///
/// A file rather than the login Keychain, because the Keychain asks and this
/// build is signed ad-hoc: its signature changes on every rebuild, macOS treats
/// each build as a different application, and "Always Allow" never holds — see
/// `ConnectionKeychain` for the measurement. A feature that made somebody
/// authorise a panel per connection per build is a feature nobody switches on,
/// which is what the Keychain answer had become.
///
/// What the encryption is for, stated plainly: the key is derived from this
/// machine and this account and is written down nowhere, so the file is bytes
/// anywhere else — in a Time Machine backup, in a dotfiles repository somebody
/// committed by accident, on a disk that leaves the building. It is *not*
/// protection from code already running as this user: that code can derive the
/// same key, and it could ask the login Keychain too. The threat this answers is
/// the file travelling, which is the one that actually happens.
///
/// Never in iCloud, whatever `ConnectionStorage` says. That setting moves a
/// document its owner chose to sync; a key that only works here would arrive on
/// the second Mac as a file that cannot be opened, and a key that travelled
/// would make the paragraph above false.
struct CredentialFile {
    let url: URL

    /// Beside `connections.json`, under `$XDG_CONFIG_HOME/dbclient`. A secret
    /// filed next to the document it belongs to is one somebody can find and
    /// delete; a secret filed elsewhere under a rule of its own is one they
    /// discover years later.
    ///
    /// Computed rather than stored, for the reason `ConnectionDirectories.system`
    /// is: the directory comes from the environment, and a `let` would freeze
    /// whatever `XDG_CONFIG_HOME` happened to say the first time anything asked.
    static var shared: CredentialFile {
        CredentialFile(
            url: ConnectionDirectories.system.local
                .appending(path: "dbclient")
                .appending(path: "credentials"))
    }

    func password(for id: UUID) -> String? {
        read()[id.uuidString]
    }

    /// Empty forgets rather than stores, which is what `ConnectionKeychain` does
    /// and what `SessionPasswords` does. Three stores disagreeing about what an
    /// empty password means would show up as a form filling itself in with
    /// nothing.
    func save(_ password: String, for id: UUID) {
        var held = read()
        if password.isEmpty {
            held.removeValue(forKey: id.uuidString)
        } else {
            held[id.uuidString] = password
        }
        write(held)
    }

    func delete(for id: UUID) {
        var held = read()
        guard held.removeValue(forKey: id.uuidString) != nil else { return }
        write(held)
    }

    private func read() -> [String: String] {
        guard let blob = try? Data(contentsOf: url),
            let key = Self.machineKey(),
            let box = try? AES.GCM.SealedBox(combined: blob),
            let plain = try? AES.GCM.open(box, using: key),
            let held = try? JSONDecoder().decode([String: String].self, from: plain)
        else {
            // Anything unreadable is nothing stored, deliberately. A file written
            // on another machine, or by a build before this format, has to leave
            // the form asking for a password rather than putting an error in
            // front of somebody who cannot act on it. The next write replaces it.
            return [:]
        }
        return held
    }

    private func write(_ held: [String: String]) {
        let manager = FileManager.default
        try? manager.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        // Nothing left behind when the last password goes, rather than an empty
        // encrypted map: "there is no file" is a state somebody can check by
        // looking, and it is the state they expect after withdrawing everything.
        guard !held.isEmpty else {
            try? manager.removeItem(at: url)
            return
        }
        guard let key = Self.machineKey(),
            let plain = try? JSONEncoder().encode(held),
            let sealed = try? AES.GCM.seal(plain, using: key),
            let blob = sealed.combined
        else { return }
        try? blob.write(to: url, options: [.atomic])
        // After the write: an atomic write is a rename over the target, so
        // permissions set before it belong to a file that is already gone.
        try? manager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }

    /// This machine and this account, hashed.
    ///
    /// `gethostuuid` rather than anything in IOKit: one call, no object to
    /// release, and no framework to link. The uid is in the digest so that two
    /// accounts on one Mac cannot read each other's file even where the
    /// permissions were lost — a restore from a backup is exactly where they are.
    private static func machineKey() -> SymmetricKey? {
        var host = [UInt8](repeating: 0, count: 16)
        var timeout = timespec(tv_sec: 2, tv_nsec: 0)
        guard gethostuuid(&host, &timeout) == 0 else { return nil }
        var digest = SHA256()
        digest.update(data: Data("dbclient credentials v1".utf8))
        digest.update(data: Data(host))
        digest.update(data: withUnsafeBytes(of: getuid()) { Data($0) })
        return SymmetricKey(data: digest.finalize())
    }
}
