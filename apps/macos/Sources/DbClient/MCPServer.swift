import Foundation
import Network
import Security

/// The socket half of the MCP server: one loopback listener, one connection
/// per request, and no protocol knowledge of its own.
///
/// Everything that decides — routing, auth, session rules, dispatch — lives
/// in `respond(to:)`, a pure function the checks drive without a socket. What
/// is left here is plumbing: accept, accumulate bytes until `MCPHTTP.parse`
/// says the request is whole, answer, close. Connection-per-request costs a
/// handshake per tool call and buys freedom from pipelining and leftover-byte
/// bookkeeping; a tool call sits seconds of model-thinking from the next, so
/// the handshake is noise.
final class MCPServer {

    /// One server's fixed facts, separated from its mutable session so that
    /// `respond` can be a function of (request, state) and nothing else.
    struct Configuration {
        let token: String
        let source: MCPDataSource
        let rowCap: Int
        let serverVersion: String
    }

    private let configuration: Configuration
    private var listener: NWListener?
    /// The one live MCP session id, nil before the first `initialize`.
    ///
    /// One rather than many: a session here is not state worth multiplying —
    /// it exists so that a client that restarted mid-conversation gets a 404
    /// and knows to initialize again, instead of resuming into assumptions
    /// the server never made.
    private var sessionID: String?
    /// Everything mutable is confined to this queue; the listener and every
    /// connection call in through it.
    private let queue = DispatchQueue(label: "dev.dbclient.mcp.wire")

    init(configuration: Configuration) {
        self.configuration = configuration
    }

    /// A fresh bearer token, minted at each server start.
    ///
    /// Per start rather than persisted: a token that lives in a file outlives
    /// the decision to turn the server off, and re-pairing a client after a
    /// restart is one paste from the Settings pane.
    static func mintToken() -> String {
        var bytes = [UInt8](repeating: 0, count: 24)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            // The system refusing entropy is not a state to serve requests in.
            return UUID().uuidString + UUID().uuidString
        }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    func start(port: UInt16) throws {
        let parameters = NWParameters.tcp
        // The binding itself is the first wall: not reachable off this
        // machine, however the later checks fare. Pinned here and not also
        // passed to the listener — Network rejects saying it twice.
        parameters.requiredLocalEndpoint = NWEndpoint.hostPort(
            host: "127.0.0.1", port: NWEndpoint.Port(rawValue: port) ?? 8765)
        let listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] connection in
            self?.serve(connection)
        }
        listener.start(queue: queue)
        self.listener = listener
    }

    func stop() {
        listener?.cancel()
        listener = nil
        queue.async { [weak self] in self?.sessionID = nil }
    }

    // MARK: - The wire

    private func serve(_ connection: NWConnection) {
        connection.start(queue: queue)
        read(connection, buffered: Data())
    }

    /// Sixteen MiB, far above any real MCP body; past it the sender is not an
    /// MCP client and the answer is a refusal rather than more memory.
    private static let bodyLimit = 16 * 1024 * 1024

    private func read(_ connection: NWConnection, buffered: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self, error == nil else {
                connection.cancel()
                return
            }
            var buffer = buffered
            if let data { buffer.append(data) }
            switch MCPHTTP.parse(buffer) {
            case .incomplete:
                if isComplete || buffer.count > Self.bodyLimit {
                    // The peer stopped talking mid-request, or is not going
                    // to fit; re-arming would spin on a closed socket.
                    connection.cancel()
                } else {
                    self.read(connection, buffered: buffer)
                }
            case .malformed:
                self.send(
                    MCPHTTP.response(status: 400, reason: "Bad Request", body: Data()),
                    over: connection)
            case .request(let request):
                let (reply, session) = Self.respond(
                    to: request, session: self.sessionID, configuration: self.configuration,
                    minting: Self.mintToken)
                self.sessionID = session
                self.send(reply, over: connection)
            }
        }
    }

    private func send(_ data: Data, over connection: NWConnection) {
        connection.send(
            content: data,
            completion: .contentProcessed { _ in connection.cancel() })
    }

    // MARK: - The rules

    /// Answers one request, given the one piece of state the server holds,
    /// and returns that state as it now stands.
    ///
    /// Pure, and the order of the walls is the contract: the route first, then
    /// origin, then the token, then the session — so a browser page learns
    /// nothing about the token from a 403, and a stranger with no token
    /// learns nothing about sessions from a 401.
    static func respond(
        to request: MCPHTTP.Request, session: String?, configuration: Configuration,
        minting mint: () -> String
    ) -> (reply: Data, session: String?) {
        switch MCPHTTP.route(request.method, request.path) {
        case .health:
            // Unauthenticated on purpose: it says "running", which the port
            // answering already says, and nothing else.
            return (MCPHTTP.response(status: 200, reason: "OK", body: Data("OK".utf8)), session)
        case .notFound:
            return (MCPHTTP.response(status: 404, reason: "Not Found", body: Data()), session)
        case .methodNotAllowed(let allow):
            return (
                MCPHTTP.response(
                    status: 405, reason: "Method Not Allowed", headers: [("Allow", allow)],
                    body: Data()),
                session
            )
        case .endSession, .mcp:
            guard MCPHTTP.isLoopbackOrigin(request.headers["origin"]) else {
                return (
                    MCPHTTP.response(status: 403, reason: "Forbidden", body: Data()), session
                )
            }
            guard MCPHTTP.authorized(request.headers["authorization"], token: configuration.token)
            else {
                return (
                    MCPHTTP.response(
                        status: 401, reason: "Unauthorized",
                        headers: [("WWW-Authenticate", "Bearer")], body: Data()),
                    session
                )
            }
            if case .endSession = MCPHTTP.route(request.method, request.path) {
                return (MCPHTTP.response(status: 200, reason: "OK", body: Data()), nil)
            }
            return answer(request, session: session, configuration: configuration, minting: mint)
        }
    }

    private static func answer(
        _ request: MCPHTTP.Request, session: String?, configuration: Configuration,
        minting mint: () -> String
    ) -> (reply: Data, session: String?) {
        let method =
            ((try? JSONSerialization.jsonObject(with: request.body)) as? [String: Any])?["method"]
            as? String
        let isInitialize = method == "initialize"

        // The session rule: initialize starts one, everything else must name
        // it. 404 and not 400, because 404 is the answer the protocol tells a
        // client to reinitialize on — a client that restarted resumes with
        // one round trip instead of an error it has no rule for.
        var current = session
        if isInitialize {
            current = mint()
        } else if session == nil || request.headers["mcp-session-id"] != session {
            return (
                MCPHTTP.response(status: 404, reason: "Session Not Found", body: Data()), session
            )
        }

        guard
            let reply = MCPDispatch.handle(
                request.body, source: configuration.source, rowCap: configuration.rowCap,
                serverVersion: configuration.serverVersion)
        else {
            // A notification: accepted, no body to answer with.
            return (MCPHTTP.response(status: 202, reason: "Accepted", body: Data()), current)
        }
        var headers = [("Content-Type", "application/json")]
        if let current { headers.append(("Mcp-Session-Id", current)) }
        return (MCPHTTP.response(status: 200, reason: "OK", headers: headers, body: reply), current)
    }
}
