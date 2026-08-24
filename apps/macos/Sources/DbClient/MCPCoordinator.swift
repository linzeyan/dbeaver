import Foundation

/// Keeps the MCP server matching what the preferences say it should be.
///
/// The pattern is desired state, not event handling: every defaults change
/// funnels into "compute what the server should look like, compare, apply".
/// `UserDefaults.didChangeNotification` fires for every preference in the
/// application, so the comparison is the filter — a font size change computes
/// the same desired state and does nothing.
@MainActor
final class MCPCoordinator {
    static let shared = MCPCoordinator()

    /// Everything about the server that preferences decide. `Equatable` is
    /// the point of the type: apply-only-on-change is a comparison.
    struct DesiredState: Equatable {
        let enabled: Bool
        let port: Int
        let rowCap: Int
    }

    static func desiredState(of preferences: Preferences) -> DesiredState {
        DesiredState(
            enabled: preferences.mcpServerEnabled,
            port: preferences.mcpServerPort,
            // Folded here and not only at launch: the field holds whatever is
            // typed, including the 0 an emptied box leaves behind, and a
            // server carrying that cap answers every query with no rows.
            rowCap: Preferences.foldedRowCap(preferences.mcpRowCap))
    }

    /// The running server's bearer token, for the Settings pane and nowhere
    /// else — it is never written to disk, and it dies with the server.
    private(set) var token: String?
    var isRunning: Bool { server != nil }

    private var server: MCPServer?
    private var source: MCPLiveSource?
    private var applied: DesiredState?
    private var preferences: Preferences?
    private var connections: (@MainActor () -> [SavedConnection])?
    private var observer: NSObjectProtocol?

    /// Starts following the preferences, and applies them once now.
    ///
    /// Called once at launch. The connections closure is read on the main
    /// actor every time the wire side asks, so exposure changes take effect
    /// on the next tool call without the server restarting.
    func follow(
        preferences: Preferences,
        connections: @escaping @MainActor () -> [SavedConnection]
    ) {
        self.preferences = preferences
        self.connections = connections
        observer = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { _ in
            // On the main queue by the observer's own terms; stated rather
            // than hopped so the apply is synchronous with the change.
            MainActor.assumeIsolated { MCPCoordinator.shared.apply() }
        }
        apply()
    }

    private func apply() {
        guard let preferences, let connections else { return }
        let desired = Self.desiredState(of: preferences)
        guard desired != applied else { return }
        applied = desired

        // Any change is stop-then-start: a port move cannot rebind in place,
        // and a fresh token per start is the token's whole design.
        server?.stop()
        server = nil
        source?.closeAll()
        source = nil
        token = nil
        guard desired.enabled else { return }
        // Typed digit by digit, the port passes through here as 8, 87, 876…
        // — an out-of-range moment is a keystroke, not a mistake, so the
        // answer is "not running yet" rather than a crash or a fold to some
        // port nobody asked for.
        guard let port = UInt16(exactly: desired.port), port >= 1024 else {
            fputs("MCP server not started: port \(desired.port) is out of range\n", stderr)
            return
        }

        let source = MCPLiveSource(entriesProvider: {
            // Called from the wire queue, which must not race the model, so
            // the read hops to the main queue and waits. Safe under the
            // standing invariant `MCPLiveSource` states: the main actor only
            // ever `async`s toward MCP, so this sync cannot close a cycle.
            // The main-thread arm exists for the checks, where a sync onto
            // the queue we are on would deadlock.
            if Thread.isMainThread {
                return MainActor.assumeIsolated { MCPLiveSource.entries(of: connections()) }
            }
            return DispatchQueue.main.sync {
                MainActor.assumeIsolated { MCPLiveSource.entries(of: connections()) }
            }
        })
        let minted = MCPServer.mintToken()
        let started = MCPServer(
            configuration: MCPServer.Configuration(
                token: minted, source: source, rowCap: desired.rowCap,
                serverVersion: Bundle.main.infoDictionary?["CFBundleShortVersionString"]
                    as? String ?? "dev"))
        do {
            try started.start(port: port)
            server = started
            self.source = source
            token = minted
        } catch {
            // Stderr and a stopped server, not an alert: the one way this
            // fails in practice is the port being taken, the pane shows the
            // server is not running, and the fix is typing another port.
            fputs("MCP server failed to start on port \(desired.port): \(error)\n", stderr)
        }
    }
}
