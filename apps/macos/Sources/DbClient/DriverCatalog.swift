import CDbFfi
import Foundation

/// A database this build can open, as the core reports it.
///
/// Read from the core rather than written out here. The list of drivers is the
/// registry's, and a second copy in Swift is a copy that goes stale: the form
/// would offer a database the build cannot open, or fail to offer one it can,
/// and either way the mistake shows up only when somebody tries.
struct DriverInfo: Decodable, Identifiable, Hashable {
    /// What a connection string for this database starts with. Also the identity
    /// stored with a remembered connection.
    let scheme: String
    let label: String
    let shape: Shape
    let defaultPort: UInt16?

    /// Whether an SSL section on this driver's form would mean anything.
    ///
    /// Asked of the core rather than decided here, for the reason `shape` is: a
    /// form that hardcoded "PostgreSQL has SSL settings" would be a second copy
    /// of the driver table, and the copy that drifts is always the one in the
    /// front end.
    let honoursSslMode: Bool

    var id: String { scheme }

    /// Whether the connection form offers a keep-alive interval for this
    /// database.
    ///
    /// A wire-protocol connection is a socket that a server timeout, a NAT
    /// table or a sleeping laptop will quietly drop for being idle, and a ping
    /// down it costs one empty round trip on a connection already paid for.
    /// The REST families have neither half of that: there is no idle socket to
    /// lose, and every request is billed like any other — a keep-alive there
    /// would spend API money keeping nothing alive.
    ///
    /// Decided here rather than asked of the core, against the rule the fields
    /// above follow, because M-KA is a Swift-only change. Spelled as the list
    /// of wire schemes — `docs/drivers.md`'s Tier 1 table — rather than as the
    /// REST exclusions, so that a scheme this list has not heard of gets no
    /// keep-alive: the stale-list failure that never costs anybody money.
    var supportsKeepAlive: Bool { Self.wireSchemes.contains(scheme) }

    private static let wireSchemes: Set<String> = [
        "postgres", "mysql", "sqlserver", "sqlite", "clickhouse", "duckdb", "mongodb",
        "cassandra", "redis"
    ]

    /// What a connection to this kind of database is made of.
    ///
    /// The form asks for different things depending on this, which is how it
    /// avoids holding a list of database names. A form that knew "sqlite means a
    /// file picker" would have to be told again for DuckDB.
    enum Shape: String, Decodable {
        case server
        case file
    }

    enum CodingKeys: String, CodingKey {
        case scheme, label, shape
        case defaultPort = "default_port"
        case honoursSslMode = "honours_sslmode"
    }
}

enum DriverCatalog {
    /// Every database the core can open.
    ///
    /// Read once. The answer is compiled into the binary the caller is already
    /// running, so it cannot change while the application is open.
    static let all: [DriverInfo] = load()

    /// The one to select in an empty form. First in the catalog rather than
    /// named here, so the core decides which database is the obvious default.
    static var first: DriverInfo? { all.first }

    /// The same list, by name, for anything that shows all of them at once.
    ///
    /// The catalog's own order is the registry's, which is neither alphabetical
    /// nor anything a reader can infer — so a menu of fifteen in that order is a
    /// menu you have to read from the top every time. Sorted here rather than in
    /// `all` because `first` reads the catalog's order on purpose: sorting the
    /// list everything shares would quietly move the default a new connection
    /// opens on from PostgreSQL to whatever sorts first.
    ///
    /// `localizedStandardCompare` rather than `<`, so the order is the one the
    /// reader's own language puts things in and digits in a name sort the way
    /// somebody counting would.
    static let inNameOrder: [DriverInfo] = all.sorted {
        $0.label.localizedStandardCompare($1.label) == .orderedAscending
    }

    static func named(_ scheme: String) -> DriverInfo? {
        all.first { $0.scheme == scheme }
    }

    private static func load() -> [DriverInfo] {
        var err: UnsafeMutablePointer<CChar>?
        guard let raw = db_drivers_json(&err) else {
            if let e = err { db_string_free(e) }
            // An empty catalog rather than a crash. The form then offers nothing
            // and says so, which is a bad state but a legible one — where a trap
            // here would take down the application before it drew a window.
            return []
        }
        defer { db_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        return (try? JSONDecoder().decode([DriverInfo].self, from: data)) ?? []
    }
}
