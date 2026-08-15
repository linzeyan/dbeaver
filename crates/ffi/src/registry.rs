//! Which driver a connection string names.
//!
//! One rule: a connection string is `<scheme>://<rest>`, the scheme names the
//! driver, and what the rest means is that driver's business. There is no
//! fallback for a string without one — a guess about which database a bare
//! `host=… port=…` refers to would be right until the day somebody pastes a
//! MySQL string in the same shape.
//!
//! The registry is static. There are fifteen drivers planned and all of them are
//! known at compile time, which is what lets this be a `match` rather than the
//! plugin system, extension registry and manifest parsing that upstream needs to
//! discover its own drivers at startup.

use dbconn::{DbError, Driver};
use driver_athena::AthenaSource;
use driver_bigquery::BigQuerySource;
use driver_cassandra::CassandraSource;
use driver_clickhouse::ChSource;
use driver_databricks::DatabricksSource;
use driver_duckdb::DuckSource;
use driver_flightsql::FlightSqlSource;
use driver_mongodb::MongoSource;
use driver_mssql::MsSqlSource;
use driver_mysql::MySqlSource;
use driver_postgres::PgSource;
use driver_redis::RedisSource;
use driver_snowflake::SnowflakeSource;
use driver_sqlite::SqliteSource;
use driver_trino::TrinoSource;
use serde::Serialize;

/// What a connection to this kind of database is made of.
///
/// The connection form asks for different things depending on the answer, and
/// this is how it knows without having a list of database names in it. A form
/// that hardcoded "sqlite means show a file picker" would offer a file picker
/// for DuckDB the day DuckDB arrived, or not offer one, depending on whether
/// somebody remembered.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// Host, port, database, user, password.
    Server,
    /// A path, and nothing else. There is no server to authenticate to.
    File,
}

/// One database this build can open.
#[derive(Debug, Clone, Serialize)]
pub struct Catalogued {
    /// The scheme a connection string starts with, and the identity the front
    /// end stores.
    pub scheme: &'static str,
    /// What to show in the picker. The database's own name, capitalised as its
    /// vendor capitalises it.
    pub label: &'static str,
    pub shape: Shape,
    /// The port to offer before the user has typed one, for a `Server`. `None`
    /// for a file.
    pub default_port: Option<u16>,
}

/// Every database this build can open.
///
/// The one place the list lives. `connect` matches against it, the error for an
/// unknown scheme is built from it, and the front end reads it over the FFI
/// rather than keeping a second copy that drifts — which is the whole reason it
/// is a table and not just a `match`.
///
/// Alternate spellings of the same driver are deliberately absent. `postgresql`
/// and `mongodb+srv` are accepted by `connect` because they are what somebody
/// pastes; they are not offered in a picker, where two entries for one database
/// is a question the user cannot answer.
pub const CATALOG: &[Catalogued] = &[
    Catalogued {
        scheme: "postgres",
        label: "PostgreSQL",
        shape: Shape::Server,
        default_port: Some(5432),
    },
    Catalogued {
        scheme: "mongodb",
        label: "MongoDB",
        shape: Shape::Server,
        default_port: Some(27017),
    },
    Catalogued {
        scheme: "sqlserver",
        label: "SQL Server",
        shape: Shape::Server,
        default_port: Some(1433),
    },
    Catalogued {
        scheme: "mysql",
        label: "MySQL",
        shape: Shape::Server,
        default_port: Some(3306),
    },
    Catalogued {
        scheme: "clickhouse",
        label: "ClickHouse",
        shape: Shape::Server,
        default_port: Some(8123),
    },
    Catalogued {
        scheme: "cassandra",
        label: "Cassandra",
        shape: Shape::Server,
        default_port: Some(9042),
    },
    Catalogued {
        scheme: "duckdb",
        label: "DuckDB",
        shape: Shape::File,
        default_port: None,
    },
    Catalogued {
        scheme: "sqlite",
        label: "SQLite",
        shape: Shape::File,
        default_port: None,
    },
    Catalogued {
        scheme: "redis",
        label: "Redis",
        shape: Shape::Server,
        default_port: Some(6379),
    },
    Catalogued {
        scheme: "trino",
        label: "Trino",
        shape: Shape::Server,
        default_port: Some(8080),
    },
    // The one entry that names a protocol rather than a database. What is behind
    // an Arrow Flight SQL endpoint is not knowable from the connection string,
    // and the label says so rather than guessing at the engine.
    Catalogued {
        scheme: "flightsql",
        label: "Arrow Flight SQL",
        shape: Shape::Server,
        default_port: Some(31337),
    },
    // 443, because the host is an account name under `snowflakecomputing.com`
    // and there is no other port to reach it on — Snowflake publishes no
    // plaintext endpoint. Offered anyway rather than hidden, so that somebody
    // behind a proxy on another port has somewhere to type it.
    Catalogued {
        scheme: "snowflake",
        label: "Snowflake",
        shape: Shape::Server,
        default_port: Some(443),
    },
    // 443 again, and for the same reason: a workspace is an HTTPS host and the
    // warehouse is named by a query parameter rather than by a port.
    Catalogued {
        scheme: "databricks",
        label: "Databricks",
        shape: Shape::Server,
        default_port: Some(443),
    },
    // The two cloud databases, and the two entries this table fits worst.
    //
    // `Shape` has `Server` and `File`, and neither of these is either. There is
    // no host to name: BigQuery's endpoints are global and Athena's is derived
    // from the region, so the field the form calls Host holds a project id and a
    // region respectively — which is stated in each driver's `connect` and is the
    // one place a two-shape connection form does not reach a cloud service. The
    // port is 443 because that is what both endpoints listen on, and it is
    // offered so the form has something true to put in the box; neither driver
    // reads it, because neither has anywhere else to go.
    //
    // Athena fits the rest of the form exactly: the access key id and the secret
    // are a name and a secret, and go in the user and password fields. BigQuery
    // does not, and that is worth writing down rather than leaving to be
    // discovered — its credential is a *file*, named by `?credentials=`, and the
    // form has no box for a file. A third `Shape` is what that asks for, and it
    // is not added here, because it is a change to the connection form and to the
    // Swift that reads this table — and neither of these drivers has yet been run
    // against anything.
    Catalogued {
        scheme: "bigquery",
        label: "BigQuery",
        shape: Shape::Server,
        default_port: Some(443),
    },
    Catalogued {
        scheme: "athena",
        label: "Athena",
        shape: Shape::Server,
        default_port: Some(443),
    },
];

/// The schemes this build answers to, for the message a wrong one gets.
fn known() -> String {
    CATALOG
        .iter()
        .map(|d| d.scheme)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The driver `url` names, or `""` for a string that names none.
///
/// The scheme decides two things and this is where both of them read it: which
/// driver opens the connection, and which SQL the editor is written in. A second
/// copy of the rule elsewhere would be a place for a connection opened as one
/// database to be highlighted as another.
pub fn scheme_of(url: &str) -> &str {
    url.split_once("://").map_or("", |(scheme, _)| scheme)
}

/// Opens whichever database `url` names.
pub async fn connect(url: &str) -> Result<Box<dyn Driver>, DbError> {
    let Some((scheme, rest)) = url.split_once("://").filter(|(s, _)| !s.is_empty()) else {
        // Deliberately without the string that failed. A connection string holds
        // a password, and an error message is the one place it is certain to be
        // shown on screen and written to a log.
        return Err(DbError::new(
            "a connection string starts with the driver it names, \
             as in postgres://user@host/database or sqlite:///path/to/file.db",
        ));
    };

    match scheme {
        // Both spellings, because neither is ours to choose: a connection string
        // is pasted from whichever console handed it out, and cloud providers
        // print both. Refusing the vendor's own spelling is a defect dressed as
        // consistency.
        //
        // Passed on whole, scheme included, since libpq's URL form is what
        // tokio-postgres parses.
        "postgres" | "postgresql" => Ok(Box::new(PgSource::connect(url).await?)),
        // Everything after the scheme is the path, so three slashes give an
        // absolute one and two give a path relative to where the client was
        // started. That is the convention every other tool in this space uses,
        // and inventing a different one would only be a thing to look up.
        // Passed on whole for the same reason PostgreSQL is: the scheme is part
        // of what the MongoDB URI parser reads, and `mongodb+srv` in particular
        // means "look the hosts up in DNS" rather than naming one.
        "mongodb" | "mongodb+srv" => Ok(Box::new(MongoSource::connect(url).await?)),
        // The one scheme whose rest is rewritten into a different grammar
        // rather than a different scheme: SQL Server's own connection string is
        // ADO's `Server=tcp:host,port;…`, and the driver reads both because
        // both are what somebody arrives with.
        "sqlserver" => Ok(Box::new(MsSqlSource::connect(url).await?)),
        // Passed on whole for the third time, and for the third distinct
        // reason: `mysql://` is the URL form `mysql_async` reads, so the scheme
        // is part of what it parses rather than a prefix to strip off.
        "mysql" => Ok(Box::new(MySqlSource::connect(url).await?)),
        // Rewritten to `http://`, which is the transport: the driver reads
        // `FORMAT ArrowStream` over ClickHouse's HTTP interface, and the scheme
        // the user writes names the database rather than the protocol carrying
        // it. `clickhouses://` is the same thing over TLS.
        "clickhouse" => Ok(Box::new(
            ChSource::connect(&format!("http://{rest}")).await?,
        )),
        "clickhouses" => Ok(Box::new(
            ChSource::connect(&format!("https://{rest}")).await?,
        )),
        // Passed on whole, and this one for the plainest reason of the lot: the
        // driver reads the host, port and keyspace out of the URL itself, so
        // there is nothing here to rewrite. The path is a keyspace rather than a
        // database, which is the same slot in the same shape of string.
        "cassandra" => Ok(Box::new(CassandraSource::connect(url).await?)),
        // Same path convention as SQLite, and for the same reason: three
        // slashes give an absolute path, two give one relative to where the
        // client was started. `duckdb://:memory:` opens a database that is
        // never written down.
        "duckdb" => Ok(Box::new(DuckSource::connect(rest).await?)),
        "sqlite" => Ok(Box::new(SqliteSource::connect(rest).await?)),
        // Passed on whole, because the scheme is part of what redis-rs's URL
        // parser reads — and because the path after the host is the database
        // number rather than a name, so `redis://host:6379/3` opens on db3.
        "redis" => Ok(Box::new(RedisSource::connect(url).await?)),
        // Rewritten to `http://` for ClickHouse's reason — the client protocol is
        // HTTP and the scheme names the database rather than the transport — with
        // one difference worth stating: the path here is `catalog/schema` and not
        // a database, because Trino has a level above the schema. Both parts are
        // optional and both become session defaults, so `trino://host:8080/tpch`
        // opens on a catalog with no schema chosen.
        "trino" => Ok(Box::new(
            TrinoSource::connect(&format!("http://{rest}")).await?,
        )),
        // Passed on whole, and the path after the host is a catalog the navigator
        // is restricted to rather than a database to open — Flight SQL has no way
        // to switch to another, so it is a filter and not a target.
        "flightsql" => Ok(Box::new(FlightSqlSource::connect(url).await?)),
        // Rewritten to `https://` and not `http://`, which is where this one
        // differs from ClickHouse and Trino: there is no plaintext Snowflake to
        // fall back to, so the scheme names the database and the transport is
        // never in question. The path is `database/schema`, both optional, and
        // the credentials are query parameters — `private_key=` for the key-pair
        // path, `token=` for an OAuth access token.
        "snowflake" => Ok(Box::new(
            SnowflakeSource::connect(&format!("https://{rest}")).await?,
        )),
        // Rewritten to `https://` for Snowflake's reason — a workspace has no
        // plaintext endpoint — with one difference worth stating: the path here
        // is `catalog/schema`, and the thing that actually runs the statement is
        // named by `warehouse_id=` in the query rather than by anything in the
        // authority. A connection string without it is refused by the driver.
        "databricks" => Ok(Box::new(
            DatabricksSource::connect(&format!("https://{rest}")).await?,
        )),
        // Passed on whole, and the first driver here whose rest is not a host:
        // `bigquery://<project>/<dataset>` names a project and a dataset, and the
        // credential is a query parameter rather than a password because it is a
        // file on disk. No server has answered this driver — its crate comment
        // says so in the first sentence.
        "bigquery" => Ok(Box::new(BigQuerySource::connect(url).await?)),
        // Passed on whole for the same reason, with a different rest again:
        // `athena://<key id>:<secret>@<region>/<database>` puts the credentials
        // where a connection string usually puts them and the region where a host
        // usually goes. The workgroup and the S3 output location are query
        // parameters, and `AthenaSource::connect` argues why each belongs in the
        // string at all. No server has answered this one either.
        "athena" => Ok(Box::new(AthenaSource::connect(url).await?)),
        other => Err(DbError::new(format!(
            "no driver for {other}://. This build has: {}",
            known()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure `url` produces, insisting there is one.
    ///
    /// A helper because `Box<dyn Driver>` is not `Debug` — the trait describes
    /// live connections, and a blanket `Debug` on one would be an invitation to
    /// print a struct holding a password.
    async fn refusal(url: &str) -> String {
        match connect(url).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected this to name no driver: {url}"),
        }
    }

    #[tokio::test]
    async fn a_string_that_names_no_driver_says_what_one_looks_like() {
        // The shape libpq accepts and this does not. Guessing PostgreSQL from it
        // would work until somebody pastes a MySQL string in the same shape.
        let message = refusal("host=127.0.0.1 port=5432 dbname=bench").await;
        assert!(message.contains("postgres://"), "got: {message}");
    }

    #[tokio::test]
    async fn a_driver_this_build_does_not_have_says_which_it_does() {
        let message = refusal("oracle://scott@localhost/orcl").await;
        assert!(message.contains("oracle"), "got: {message}");
        assert!(message.contains("sqlite"), "got: {message}");
    }

    /// Every database offered in the picker can actually be opened.
    ///
    /// The failure this guards against is quiet and embarrassing: a scheme added
    /// to `CATALOG` but not to the `match` shows up in the form, and choosing it
    /// reports "no driver for duckdb://" — from the same build that just offered
    /// it. Connecting is expected to fail here, since none of these point at a
    /// server; what must not happen is failing for that reason.
    #[tokio::test]
    async fn every_database_the_picker_offers_can_be_asked_for() {
        for entry in CATALOG {
            let url = match entry.shape {
                Shape::Server => format!("{}://127.0.0.1:1/none", entry.scheme),
                Shape::File => format!("{}:///nonexistent/none.db", entry.scheme),
            };
            let message = refusal(&url).await;
            assert!(
                !message.contains("no driver for"),
                "{} is in the catalog but not in connect(): {message}",
                entry.scheme
            );
        }
    }

    #[tokio::test]
    async fn a_failure_never_repeats_the_string_it_was_given() {
        // Whatever else an error says, it must not put the password back on
        // screen — an error message is the one place it is certain to be shown
        // and logged.
        // A SQLite path is deliberately not covered: it is not a secret, and
        // naming the file that is not there is the whole value of that message.
        for url in [
            "host=127.0.0.1 password=hunter2",
            "oracle://scott:hunter2@localhost/orcl",
        ] {
            let message = refusal(url).await;
            assert!(
                !message.contains("hunter2"),
                "the string leaked into: {message}"
            );
        }
    }
}
