import Foundation

/// The HTTP half of the MCP server, kept free of the network and of the app.
///
/// Everything here is a pure function from bytes to values: what a request
/// said, where it routes, whether its origin is this machine, whether its
/// token is ours. The `NWListener` wiring that feeds it owns the sockets and
/// nothing else. The split is what lets `--verify-mcp` hold this whole
/// surface — the reference implementation this was measured against buried
/// its parsing and dispatch where no test could reach them, and its test file
/// is honest about the result: the protocol surface has no coverage at all.
enum MCPHTTP {

    /// One parsed HTTP/1.1 request.
    struct Request {
        let method: String
        let path: String
        let query: [String: String]
        /// Header names lowercased once here, because HTTP says they are
        /// case-insensitive and every reader doing its own caseless compare is
        /// the bug waiting to happen.
        let headers: [String: String]
        let body: Data
    }

    /// What a buffer of bytes turned out to be.
    ///
    /// `incomplete` and `malformed` are different answers to different
    /// callers: an incomplete request means keep reading, a malformed one
    /// means answer 400 and close. A parser that returns nil for both leaves
    /// the connection waiting on bytes that will never make it whole.
    enum Parse: Equatable {
        case incomplete
        case malformed
        case request(Request)
    }

    /// Bodies are read by `Content-Length` alone. `Transfer-Encoding:
    /// chunked` is not implemented and parses as malformed rather than as a
    /// guess — MCP clients POST small JSON bodies with a declared length, and
    /// a chunked parser would be attack surface serving no known caller.
    static func parse(_ data: Data) -> Parse {
        guard let headerEnd = data.range(of: Data("\r\n\r\n".utf8)) else {
            return .incomplete
        }
        let head = String(decoding: data[..<headerEnd.lowerBound], as: UTF8.self)
        var lines = head.components(separatedBy: "\r\n")
        let requestLine = lines.removeFirst().split(separator: " ")
        guard requestLine.count == 3, requestLine[2].hasPrefix("HTTP/") else {
            return .malformed
        }

        var headers: [String: String] = [:]
        for line in lines {
            // Split on the first colon only: a value is allowed to contain
            // colons of its own, and `Mcp-Session-Id: a:b:c` must survive.
            guard let colon = line.firstIndex(of: ":") else { return .malformed }
            let name = line[..<colon].lowercased()
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            headers[name] = value
        }
        if headers["transfer-encoding"] != nil { return .malformed }

        let length = Int(headers["content-length"] ?? "0") ?? -1
        guard length >= 0 else { return .malformed }
        let bodyStart = headerEnd.upperBound
        guard data.count - bodyStart >= length else { return .incomplete }
        let body = data.subdata(in: bodyStart..<bodyStart + length)

        let target = requestLine[1]
        let path = String(target.prefix(while: { $0 != "?" }))
        var query: [String: String] = [:]
        if let mark = target.firstIndex(of: "?") {
            for pair in target[target.index(after: mark)...].split(separator: "&") {
                let parts = pair.split(separator: "=", maxSplits: 1)
                guard let name = String(parts[0]).removingPercentEncoding else { continue }
                let value = parts.count == 2 ? String(parts[1]) : ""
                query[name] = value.removingPercentEncoding ?? value
            }
        }
        return .request(
            Request(
                method: String(requestLine[0]), path: path, query: query,
                headers: headers, body: body))
    }

    /// Where a request goes, decided by method and path together.
    enum Route: Equatable {
        case mcp
        case endSession
        case health
        case notFound
        /// The refusal names what would have been accepted, because a 405
        /// without an `Allow` header is a door that says only "no".
        case methodNotAllowed(allow: String)
    }

    static func route(_ method: String, _ path: String) -> Route {
        switch (method, path) {
        case ("POST", "/mcp"): return .mcp
        case ("DELETE", "/mcp"): return .endSession
        case (_, "/mcp"): return .methodNotAllowed(allow: "POST, DELETE")
        case ("GET", "/health"): return .health
        case (_, "/health"): return .methodNotAllowed(allow: "GET")
        default: return .notFound
        }
    }

    /// Whether a browser page could be driving this request.
    ///
    /// An absent header passes: non-browser clients — every MCP client — send
    /// none, and the bearer token is the actual gate. What this refuses is the
    /// one thing the token cannot: a malicious web page using DNS rebinding to
    /// aim a browser at the loopback port, because the browser will say where
    /// the page came from and `https://127.0.0.1.evil.example` is not here.
    static func isLoopbackOrigin(_ origin: String?) -> Bool {
        guard let origin, !origin.isEmpty else { return true }
        guard var host = URLComponents(string: origin)?.host?.lowercased() else {
            return false
        }
        // Foundation has answered `[::1]` both with and without its brackets
        // across releases; the address is the same either way.
        host = host.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        return host == "127.0.0.1" || host == "::1" || host == "localhost"
    }

    /// Whether the presented `Authorization` header carries the server's
    /// token, compared in constant time.
    ///
    /// The token exists because loopback binding is not authentication: any
    /// process running as this user can reach the port, and "can open a TCP
    /// connection" must not mean "can read every exposed database". Constant
    /// time because a byte-by-byte early exit would let that same process
    /// guess the token one byte at a time by clock.
    static func authorized(_ header: String?, token: String) -> Bool {
        guard let header, header.hasPrefix("Bearer ") else { return false }
        let presented = Array(header.dropFirst("Bearer ".count).utf8)
        let expected = Array(token.utf8)
        guard presented.count == expected.count else { return false }
        var difference: UInt8 = 0
        for (a, b) in zip(presented, expected) { difference |= a ^ b }
        return difference == 0
    }

    /// One serialized HTTP/1.1 response, connection-per-request.
    ///
    /// `Connection: close` on everything: a tool call is seconds of thinking
    /// apart from the next one, and re-arming for keep-alive means owning
    /// pipelining and leftover-byte bookkeeping for a handshake nobody is
    /// waiting on.
    static func response(status: Int, reason: String, headers: [(String, String)] = [], body: Data)
        -> Data
    {
        var head = "HTTP/1.1 \(status) \(reason)\r\n"
        for (name, value) in headers { head += "\(name): \(value)\r\n" }
        head += "Content-Length: \(body.count)\r\nConnection: close\r\n\r\n"
        return Data(head.utf8) + body
    }
}

extension MCPHTTP.Request: Equatable {}
