import Foundation

/// How a connection says which database it is, in two marks.
///
/// Not brand logos, which is what dbx uses. Fifteen coloured marks on this
/// palette are fifteen colours nobody chose, each with an asset and a licence to
/// keep — and the one colour a connection is allowed to carry here already means
/// something else (`Connection.*`, the mark somebody sets so that prod does not
/// look like staging).
///
/// So: a **shape** for the family, and an **abbreviation** for the product. The
/// shape answers the question that decides the tree's levels and the pane set —
/// relational, document, wide-column, key-value, warehouse, engine — and the
/// abbreviation answers which of them this is. Neither is the only signal:
/// wherever these are drawn the full name is on the same row.
///
/// Keyed by scheme, which is the identity the core hands out
/// (`crates/ffi/src/registry.rs`) and the one a saved connection stores. An
/// unrecognised scheme is not a blank: it gets the generic cylinder and its own
/// name, so a driver added to the registry and forgotten here is legible rather
/// than invisible. `DriverBadgeChecks` fails when that happens, which is what
/// keeps the fallback a safety net rather than the answer.
enum DriverBadge {
    /// The SF Symbol for this scheme's family.
    static func familySymbol(forScheme scheme: String) -> String {
        switch scheme {
        // Relational, over a socket. The cylinder is the disk-stack every
        // database drawing has used since before any of these existed.
        case "postgres", "postgresql", "mysql", "sqlserver", "clickhouse":
            return "cylinder"
        // Relational, and a file on this machine. The distinction earns its own
        // shape because it is the one that changes what the connection form
        // asks for — a path, not a host (`DriverInfo.Shape.file`).
        case "sqlite", "duckdb":
            return "externaldrive"
        case "mongodb", "mongodb+srv":
            return "curlybraces"
        case "cassandra":
            return "tablecells"
        case "redis":
            return "key"
        // Warehouses: the thing they have in common is that the data is not
        // where you are.
        case "snowflake", "bigquery", "databricks", "athena":
            return "cloud"
        // Query engines over somebody else's storage. Neither of these owns a
        // byte of what it reads, which is why they are not cylinders.
        case "trino", "flightsql":
            return "bolt.horizontal"
        default:
            return "cylinder"
        }
    }

    /// The two-letter mark for this scheme's product.
    ///
    /// Two letters throughout, so a column of them is a column and not a ragged
    /// edge. An unknown scheme returns itself rather than a placeholder — it is
    /// longer than two characters and it should look wrong.
    static func abbreviation(forScheme scheme: String) -> String {
        switch scheme {
        case "postgres", "postgresql": return "Pg"
        case "mysql": return "My"
        case "sqlserver": return "MS"
        case "clickhouse": return "CH"
        case "sqlite": return "Li"
        case "duckdb": return "Du"
        case "mongodb", "mongodb+srv": return "Mo"
        case "cassandra": return "Ca"
        case "redis": return "Re"
        case "snowflake": return "Sf"
        case "bigquery": return "BQ"
        case "databricks": return "Db"
        case "athena": return "At"
        case "trino": return "Tr"
        case "flightsql": return "FS"
        default: return scheme
        }
    }

    /// Whether this scheme is one the table above names.
    ///
    /// For the check that fails when a driver is added to the registry without
    /// being added here. Asked of the abbreviation rather than the symbol
    /// because the symbol's fallback is a defensible answer for a relational
    /// database nobody mapped, while the abbreviation's fallback never is.
    static func isMapped(scheme: String) -> Bool {
        abbreviation(forScheme: scheme) != scheme
    }
}
