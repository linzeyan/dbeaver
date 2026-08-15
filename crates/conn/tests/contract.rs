//! What every driver has to do, checked through the trait and nothing else.
//!
//! The checks are written once, against `&dyn Driver`, and run against each
//! implementation. That arrangement is the point: a check that reached for
//! `PgSource` would be testing PostgreSQL, and this is meant to be testing the
//! contract — the thing a sixth driver has to satisfy without anybody rereading
//! the first one.
//!
//! SQLite's pass needs nothing installed and runs under `make test`. PostgreSQL's
//! and MongoDB's are the same checks against a server, so they are marked
//! `ignore` and run under `make test-integration`.
//!
//! MongoDB is why `Subject` carries statements rather than building them. The
//! first version of this file assembled `SELECT {key} FROM {relation}` and was
//! therefore checking that every driver speaks SQL, which is a claim the trait
//! never made — `query` takes text the database understands, and MongoDB's is a
//! command document. The statements moved into the subject and nothing else
//! about the checks changed, which is the useful result: the contract was about
//! databases after all, and only the harness had assumed otherwise.

use dbconn::{Driver, TxStep};
use std::path::PathBuf;
use tempfile::TempDir;

const PG_CONN: &str = "postgres://bench:bench@127.0.0.1:55432/bench";

/// A driver, plus the least a database has to contain for these checks to mean
/// anything: somewhere to look, and a table of ascending integers to read.
struct Subject {
    driver: Box<dyn Driver>,
    schema: String,
    relation: String,
    key: String,
    /// Reads `key` from `relation` in ascending order, in this database's own
    /// language.
    read: String,
    /// A statement broken somewhere in the middle rather than truncated, for the
    /// error-position check. Truncated input is deliberately avoided: SQLite
    /// reports no offset for it, so a check written that way would be asserting
    /// PostgreSQL's behaviour under the name of the contract.
    broken: String,
    /// A statement naming a relation that is not there.
    missing: String,
    /// Whether reading a relation that does not exist is a failure at all.
    ///
    /// The two answers are both defensible and the databases give different
    /// ones. SQL refuses to plan a query over a name it cannot resolve.
    /// MongoDB returns an empty cursor, because a collection is created by
    /// writing to it and "not there yet" is an ordinary state rather than a
    /// mistake. The contract cannot require either without calling one of them
    /// wrong, so it requires only that the driver be consistent about which it
    /// does.
    missing_is_a_failure: bool,
    /// Whether this database can hold a cursor open at all.
    ///
    /// False for GreptimeDB, and it is the one place protocol compatibility
    /// stops short. It serves the PostgreSQL wire protocol, accepts `DECLARE`
    /// and answers `FETCH` correctly under the simple query protocol — psql
    /// pages through a table with no trouble. Under the extended protocol,
    /// which is what any client sending typed parameters uses, its `FETCH`
    /// replies with a DataRow whose field count does not match the
    /// RowDescription it just sent, and the connection cannot go on.
    ///
    /// Nothing in this driver can fix that, and the workarounds are worse than
    /// the gap: `LIMIT`/`OFFSET` is what a cursor exists instead of, and the
    /// simple query protocol returns every value as text.
    cursors: bool,
    /// Whether this database says where in a statement a fault is.
    ///
    /// Recorded per subject rather than required of everyone, because the
    /// databases genuinely differ and the trait says so: a failure carries a
    /// position or it does not. Asserting `is_some()` for all of them was the
    /// harness deciding that every database is a SQL parser with an offset —
    /// MongoDB's server rejects a well-formed command by naming the field it
    /// disliked, and there is no offset to have.
    positions: bool,
    /// Somewhere to write, for the transaction check — `None` where there is no
    /// transaction to control.
    ///
    /// Kept in step with `Driver::transactional` by the check rather than
    /// derived from it, so that a driver which gains a session connection fails
    /// here until somebody gives it a fixture, instead of silently testing
    /// nothing.
    scratch: Option<Scratch>,
    /// Kept alive for the length of the test, and unused otherwise.
    _fixture: Option<TempDir>,
}

/// A table the transaction check writes to, created and dropped by the check.
///
/// Statements rather than a table name, for the reason the rest of this file
/// carries statements: building `INSERT INTO {table}` in the check would smuggle
/// in the claim that every database with transactions speaks SQL. `Scratch::sql`
/// is where that claim is made, by the subjects that can honour it.
struct Scratch {
    create: String,
    clear: String,
    /// Adds one row. Run more than once, so nothing in it may be unique.
    insert: String,
    /// Reads the rows back; the check counts them and looks at nothing else.
    read: String,
    drop: String,
}

impl Scratch {
    /// The statements in ordinary SQL, for the subjects that speak it.
    ///
    /// Not all of them do, and `CREATE TABLE IF NOT EXISTS` is where they part:
    /// SQL Server has no such clause and writes the check out as an `IF`. That
    /// subject builds its own `Scratch` rather than this growing a dialect
    /// switch, which would put a decision about one database in the path of
    /// every other.
    fn sql(table: &str) -> Self {
        Self {
            create: format!("CREATE TABLE IF NOT EXISTS {table} (n INT)"),
            clear: format!("DELETE FROM {table}"),
            insert: format!("INSERT INTO {table} (n) VALUES (1)"),
            read: format!("SELECT n FROM {table}"),
            drop: format!("DROP TABLE {table}"),
        }
    }
}

async fn sqlite() -> Subject {
    let dir = tempfile::tempdir().expect("no temporary directory");
    let path: PathBuf = dir.path().join("contract.db");
    let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
    conn.execute_batch(
        "CREATE TABLE nums (id INTEGER PRIMARY KEY, label TEXT);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 500)
         INSERT INTO nums (id, label) SELECT x, 'row-' || x FROM c;",
    )
    .expect("fixture setup failed");
    drop(conn);

    let driver = driver_sqlite::SqliteSource::connect(path.to_str().unwrap())
        .await
        .expect("fixture database unreachable");
    Subject {
        driver: Box::new(driver),
        schema: "main".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM nums ORDER BY id".to_string(),
        broken: "SELECT id FROM nums WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        positions: true,
        // Each statement opens a connection of its own here, so a transaction
        // could not span two of them.
        scratch: None,
        _fixture: Some(dir),
    }
}

/// DuckDB, which like SQLite needs nothing installed and so runs under plain
/// `cargo test`.
///
/// Its schema is `memory.main` rather than `main`: DuckDB has a catalog level
/// above the schema and the trait has one string, so the driver flattens the two
/// into a qualified name. `ATTACH` is ordinary usage there and produces two
/// schemas both called `main`, so the level cannot simply be dropped.
async fn duckdb() -> Subject {
    let driver = driver_duckdb::DuckSource::connect(":memory:")
        .await
        .expect("an in-memory database should always open");
    driver
        .query(
            "CREATE TABLE nums AS \
             SELECT i AS id, 'row-' || i AS label FROM range(1, 501) t(i)",
            1,
        )
        .await
        .expect("fixture setup failed");

    Subject {
        driver: Box::new(driver),
        schema: "memory.main".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM nums ORDER BY id".to_string(),
        broken: "SELECT id FROM nums WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        positions: true,
        // As SQLite: a connection per piece of work, and no transaction can
        // reach across two of them.
        scratch: None,
        _fixture: None,
    }
}

async fn postgres() -> Subject {
    let driver = driver_postgres::PgSource::connect(PG_CONN)
        .await
        .expect("benchmark database unreachable; run `make db-seed`");
    Subject {
        driver: Box::new(driver),
        schema: "public".to_string(),
        relation: "bench_wide".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM bench_wide ORDER BY id".to_string(),
        broken: "SELECT id FROM bench_wide WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        positions: true,
        scratch: Some(Scratch::sql("contract_tx")),
        _fixture: None,
    }
}

const CLICKHOUSE_URL: &str = "http://default:test@127.0.0.1:58123/bench";

/// ClickHouse, which is the one here with no cursors of its own to speak of.
///
/// Its `cursor` and `query` are the same call, and that is not a shortcut: a
/// ClickHouse response body already is a snapshot being read forward, so the two
/// properties the trait asks a cursor for come free. The fixture is seeded by
/// the driver's own test suite (`make db-up-clickhouse`), under the same table
/// name the PostgreSQL benchmark uses.
async fn clickhouse() -> Subject {
    let driver = driver_clickhouse::ChSource::connect(CLICKHOUSE_URL)
        .await
        .expect("ClickHouse unreachable; run `make db-up-clickhouse`");
    Subject {
        driver: Box::new(driver),
        schema: "bench".to_string(),
        relation: "bench_wide".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM bench_wide ORDER BY id".to_string(),
        broken: "SELECT id FROM bench_wide WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        positions: true,
        // ClickHouse's transactions are experimental, off by default, and cover
        // one INSERT rather than a session's worth of statements.
        scratch: None,
        _fixture: None,
    }
}

const MONGO_URI: &str = "mongodb://127.0.0.1:57017";

/// The same fixture as the others, in the one database here that has no schema.
///
/// Seeded through the `mongodb` crate rather than through the driver, so the
/// fixture does not depend on the code under test being right.
async fn mongodb() -> Subject {
    let client = mongodb::Client::with_uri_str(MONGO_URI)
        .await
        .expect("MongoDB unreachable; run `make db-up-mongo`");
    let db = client.database("dbclient_contract");
    db.drop().await.expect("could not clear the fixture");
    let rows: Vec<bson::Document> = (1..=500)
        .map(|i| bson::doc! { "_id": i, "label": format!("row-{i}") })
        .collect();
    db.collection::<bson::Document>("nums")
        .insert_many(rows)
        .await
        .expect("seeding the fixture");

    let driver = driver_mongodb::MongoSource::connect(&format!("{MONGO_URI}/dbclient_contract"))
        .await
        .expect("driver could not connect");
    Subject {
        driver: Box::new(driver),
        schema: "dbclient_contract".to_string(),
        relation: "nums".to_string(),
        // MongoDB's guaranteed key, which is what `id` is standing in for
        // everywhere else in this file.
        key: "_id".to_string(),
        // Projected down to the key, which is what `SELECT id FROM ...` does for
        // the others: a find with no projection returns whole documents, and the
        // check counts columns.
        read: r#"{"find": "nums", "sort": {"_id": 1}, "projection": {"_id": 1}}"#.to_string(),
        // Broken in the middle in this database's own language: the sort is a
        // string where a document belongs, so the statement parses as JSON and
        // is refused by the server.
        broken: r#"{"find": "nums", "sort": "sideways"}"#.to_string(),
        missing: r#"{"find": "no_such_relation_anywhere"}"#.to_string(),
        cursors: true,
        missing_is_a_failure: false,
        // The server names the field it disliked; it does not say where in the
        // text that field was written, and inventing an offset from a field name
        // would put the caret wherever that name first appeared.
        positions: false,
        // MongoDB's transactions need a replica set and a session this driver
        // does not hold.
        scratch: None,
        _fixture: None,
    }
}

const MYSQL_ROOT: &str = "mysql://root:test@127.0.0.1:53306/";

/// MySQL, in a database of this file's own rather than the driver's `bench`.
///
/// The driver's own suite begins by dropping and rebuilding `bench`, and
/// `cargo test --workspace -- --ignored` runs the two binaries at the same
/// time. Sharing the fixture would make this test fail whenever it lost that
/// race, which is a scheduling accident wearing the costume of a contract
/// violation.
async fn mysql() -> Subject {
    use mysql_async::prelude::Queryable;

    let opts = mysql_async::Opts::from_url(MYSQL_ROOT).expect("the fixture URL should parse");
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("MySQL unreachable; run `make db-up-mysql`");
    let rows: Vec<String> = (1..=500).map(|i| format!("({i}, 'row-{i}')")).collect();
    for statement in [
        "DROP DATABASE IF EXISTS dbclient_contract".to_string(),
        "CREATE DATABASE dbclient_contract".to_string(),
        "USE dbclient_contract".to_string(),
        "CREATE TABLE nums (id INT PRIMARY KEY, label VARCHAR(32))".to_string(),
        format!("INSERT INTO nums (id, label) VALUES {}", rows.join(", ")),
    ] {
        conn.query_drop(&statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }
    conn.disconnect()
        .await
        .expect("closing the seed connection");

    let driver = driver_mysql::MySqlSource::connect(&format!("{MYSQL_ROOT}dbclient_contract"))
        .await
        .expect("the MySQL driver could not connect");
    Subject {
        driver: Box::new(driver),
        // MySQL's schema is its database; there is no level above it.
        schema: "dbclient_contract".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM nums ORDER BY id".to_string(),
        broken: "SELECT id FROM nums WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        // MySQL's parse error names the text it stopped at — "near 'ORDER BY
        // id'" — and never an offset. Recovering one by searching for that
        // fragment would find the first occurrence rather than the one that
        // failed, which is a caret in the wrong place rather than no caret.
        positions: false,
        // The database has transactions; this driver has no connection to hold
        // one on, because every statement takes one from a pool.
        scratch: None,
        _fixture: None,
    }
}

const MSSQL_ADO: &str = "Server=tcp:127.0.0.1,51433;User Id=sa;Password=Str0ng!Passw0rd;\
                         Encrypt=true;TrustServerCertificate=true";

/// SQL Server, reached through the URL form the connection form builds.
///
/// The URL rather than the ADO string on purpose: this driver is the one that
/// accepts two spellings, and the one the front end will actually send is the
/// one worth checking end to end.
async fn mssql() -> Subject {
    use tiberius::{Client, Config};
    use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

    async fn open(database: &str) -> Client<Compat<tokio::net::TcpStream>> {
        let config = Config::from_ado_string(&format!("{MSSQL_ADO};Database={database}"))
            .expect("the fixture connection string should parse");
        let tcp = tokio::net::TcpStream::connect(config.get_addr())
            .await
            .expect("SQL Server unreachable; run `make db-up-mssql`");
        tcp.set_nodelay(true).expect("setting nodelay");
        Client::connect(config, tcp.compat_write())
            .await
            .expect("SQL Server refused the fixture connection")
    }

    let mut master = open("master").await;
    for statement in ["IF DB_ID('dbclient_contract') IS NULL CREATE DATABASE dbclient_contract"] {
        master
            .simple_query(statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"))
            .into_results()
            .await
            .expect("draining the seed statement");
    }

    let mut db = open("dbclient_contract").await;
    for statement in [
        "DROP TABLE IF EXISTS dbo.nums",
        "CREATE TABLE dbo.nums (
             id    int          NOT NULL CONSTRAINT pk_contract_nums PRIMARY KEY,
             label nvarchar(40) NOT NULL)",
        // Generated by the server rather than sent as 500 values: a TDS batch
        // of that size is slow enough over the emulation this image runs under
        // to be worth avoiding.
        "INSERT INTO dbo.nums (id, label)
         SELECT n, CONCAT(N'row-', n)
         FROM (SELECT TOP (500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n
               FROM sys.all_objects a CROSS JOIN sys.all_objects b) x",
    ] {
        db.simple_query(statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"))
            .into_results()
            .await
            .expect("draining the seed statement");
    }

    let driver = driver_mssql::MsSqlSource::connect(
        "sqlserver://sa:Str0ng%21Passw0rd@127.0.0.1:51433/dbclient_contract\
         ?TrustServerCertificate=true",
    )
    .await
    .expect("the SQL Server driver could not connect");
    Subject {
        driver: Box::new(driver),
        schema: "dbo".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM dbo.nums ORDER BY id".to_string(),
        // Two lines, which is what makes the position mean anything here: SQL
        // Server reports the line a fault is on and not an offset into the
        // text, so in a one-line statement the answer is always line 1 and
        // locates nothing. The driver reports no position at all in that case
        // rather than a caret confidently placed at the first character.
        broken: "SELECT id FROM dbo.nums\nWHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        positions: true,
        // T-SQL rather than `Scratch::sql`, because SQL Server has no `CREATE
        // TABLE IF NOT EXISTS`. Bending the shared helper to cover that would
        // hand every other subject a statement written for this one; the
        // statements live in the subject exactly so a database can spell them
        // its own way.
        scratch: Some(Scratch {
            create: "IF OBJECT_ID('dbo.contract_tx', 'U') IS NULL \
                     CREATE TABLE dbo.contract_tx (n int)"
                .to_string(),
            clear: "DELETE FROM dbo.contract_tx".to_string(),
            insert: "INSERT INTO dbo.contract_tx (n) VALUES (1)".to_string(),
            read: "SELECT n FROM dbo.contract_tx".to_string(),
            drop: "DROP TABLE dbo.contract_tx".to_string(),
        }),
        _fixture: None,
    }
}

/// A PostgreSQL-compatible database, seeded and connected through the
/// PostgreSQL driver.
///
/// The whole point is that no new driver code exists for these. Phase 2 claims
/// protocol compatibility is transitive — that CockroachDB and GreptimeDB are
/// reached by the driver already written — and a claim like that is worth
/// exactly as much as the test that runs against the real thing.
async fn pg_compatible(
    url: &str,
    seed: Vec<String>,
    relation: &str,
    key: &str,
    positions: bool,
    cursors: bool,
    scratch: Option<Scratch>,
) -> Subject {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("compatible database unreachable; see the Makefile target");
    // The connection is a task, not a value: tokio-postgres drives the socket
    // separately from the client handle, and dropping it closes the session.
    let pump = tokio::spawn(connection);
    for statement in &seed {
        client
            .batch_execute(statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }
    drop(client);
    pump.abort();

    let driver = driver_postgres::PgSource::connect(url)
        .await
        .expect("the PostgreSQL driver could not connect");
    Subject {
        driver: Box::new(driver),
        schema: "public".to_string(),
        relation: relation.to_string(),
        key: key.to_string(),
        read: format!("SELECT {key} FROM {relation} ORDER BY {key}"),
        broken: format!("SELECT {key} FROM {relation} WHERE ORDER BY {key}"),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors,
        positions,
        scratch,
        _fixture: None,
    }
}

/// A MySQL-compatible database, seeded and connected through the MySQL driver.
///
/// The mirror of `pg_compatible`, and there for the same reason: Phase 2 claims
/// TiDB and StarRocks are reached by the driver already written, and a claim
/// like that is worth exactly as much as the test that runs against the real
/// thing.
///
/// Seeded over `mysql_async` rather than through `MySqlSource`, so that the
/// fixture cannot be vouched for by the code it exists to examine. The database
/// is built here and the table is the caller's, because the table is the one
/// statement these servers spell differently — StarRocks wants a distribution
/// clause that MySQL has no word for — while `CREATE DATABASE` is the same
/// everywhere.
async fn mysql_compatible(
    server: &str,
    seed: Vec<String>,
    relation: &str,
    key: &str,
    positions: bool,
    cursors: bool,
) -> Subject {
    use mysql_async::prelude::Queryable;

    let opts = mysql_async::Opts::from_url(server).expect("the fixture URL should parse");
    // The same setting the driver connects with, for the same reason: left on,
    // the client reads `@@socket` during the handshake so it can move a local
    // connection onto a Unix socket, and StarRocks has no such variable to
    // report. Turning it off here keeps the seed connection honest about what
    // it is testing — a fixture that reached the server by a route the driver
    // does not use would be proving something else.
    let mut conn = mysql_async::Conn::new(mysql_async::Opts::from(
        mysql_async::OptsBuilder::from_opts(opts).prefer_socket(false),
    ))
    .await
    .expect("compatible database unreachable; see the Makefile target");
    let prelude = [
        "DROP DATABASE IF EXISTS dbclient_contract",
        "CREATE DATABASE dbclient_contract",
        "USE dbclient_contract",
    ]
    .into_iter()
    .map(str::to_string);
    for statement in prelude.chain(seed) {
        conn.query_drop(&statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }
    conn.disconnect()
        .await
        .expect("closing the seed connection");

    let driver = driver_mysql::MySqlSource::connect(&format!("{server}dbclient_contract"))
        .await
        .expect("the MySQL driver could not connect");
    Subject {
        driver: Box::new(driver),
        schema: "dbclient_contract".to_string(),
        relation: relation.to_string(),
        key: key.to_string(),
        read: format!("SELECT {key} FROM {relation} ORDER BY {key}"),
        broken: format!("SELECT {key} FROM {relation} WHERE ORDER BY {key}"),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors,
        positions,
        // The MySQL driver's answer, and it is the driver's rather than the
        // server's: TiDB and StarRocks both have transactions.
        scratch: None,
        _fixture: None,
    }
}

const COCKROACH: &str = "postgres://root@127.0.0.1:56257/defaultdb";
const GREPTIME: &str = "postgres://greptime@127.0.0.1:54003/public";
const TIDB: &str = "mysql://root@127.0.0.1:54000/";
const STARROCKS: &str = "mysql://root@127.0.0.1:59030/";

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// A result arrives in batches of the size that was asked for, in order, once
/// each.
async fn reads_a_result_in_batches(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let mut stream = driver
        .query(&subject.read, 100)
        .await
        .expect("query failed");

    // Before a single row has been read: a front end lays out a grid first and
    // asks for rows afterwards.
    //
    // That the key column is there, not that it is the only one. An exact count
    // would be asserting SQL's projection semantics: MongoDB's result carries a
    // trailing `_extra` column whatever the statement asked for, because a
    // schemaless database cannot promise a later document will fit the columns
    // inferred from an earlier one.
    let schema = stream.schema();
    assert!(
        schema.field_with_name(&subject.key).is_ok(),
        "the schema should name the column that was asked for"
    );
    // Zero is a real answer, so "not finished" cannot be zero.
    assert_eq!(stream.rows_affected(), None);

    let first = stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a first batch");
    assert_eq!(first.num_rows(), 100);
    let second = stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a second batch");
    assert_eq!(second.num_rows(), 100);
    // The Arrow type is deliberately not asserted. PostgreSQL's `id` is a 32-bit
    // integer and SQLite's is 64-bit, and both are right about their own column;
    // what the contract fixes is the shape of the reading, not the width of the
    // number.
}

/// A cursor pages forward without repeating or skipping, and reports its columns
/// before the first page.
async fn pages_a_cursor(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let mut cursor = driver
        .cursor(&subject.read, 50)
        .await
        .expect("cursor failed");
    assert!(cursor.schema().field_with_name(&subject.key).is_ok());

    let mut seen = 0usize;
    for page in 1..=3 {
        let batch = cursor
            .fetch()
            .await
            .expect("fetch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        assert_eq!(batch.num_rows(), 50);
        seen += batch.num_rows();
    }
    assert_eq!(seen, 150);

    // Closing is optional but has to work, and has to be safe to call on a
    // cursor with pages left in it.
    cursor.close().await.expect("close failed");
}

/// A cursor's canceller can be taken out and used while nothing is running.
///
/// Delivery is not interruption, so cancelling an idle cursor is a no-op rather
/// than an error — and a driver that returned one would make a front end's
/// Cancel button report a failure for pressing it at the wrong moment.
async fn cancels_an_idle_cursor_without_complaining(subject: &Subject) {
    let cursor = subject
        .driver
        .cursor(&subject.read, 10)
        .await
        .expect("cursor failed");
    cursor.canceller().cancel().await.expect("cancel failed");
    subject
        .driver
        .cancel()
        .await
        .expect("session cancel failed");
}

/// A failure says where it is, or says nothing — never something in between.
async fn reports_where_a_statement_is_wrong(subject: &Subject) {
    let driver = subject.driver.as_ref();

    let err = failure(driver, &subject.broken).await;
    if subject.positions {
        assert!(
            err.statement_position().is_some(),
            "this database reports positions, so a broken statement should have one: {err}"
        );
    }
    lands_inside(&err, &subject.broken);
    assert!(
        !err.is_cancelled(),
        "a broken statement is not a cancellation"
    );

    // Whether a missing relation has a position is the database's business, and
    // the two disagree: PostgreSQL points at the name, SQLite reports none. Both
    // are honest, so the contract asks only that whatever comes back could be
    // acted on — an earlier version of this required None and was asserting
    // SQLite's behaviour under the name of the contract.
    if subject.missing_is_a_failure {
        let missing = failure(driver, &subject.missing).await;
        lands_inside(&missing, &subject.missing);
        assert!(!missing.is_cancelled());
    } else {
        // Not a weaker check, a different one: a database that considers this
        // ordinary has to actually answer, and answer with nothing.
        let mut stream = driver
            .query(&subject.missing, 10)
            .await
            .expect("reading a relation that is not there should be allowed here");
        assert!(
            stream.next_batch().await.expect("batch error").is_none(),
            "a relation that is not there has no rows"
        );
    }
}

/// A position a front end could put a caret on: counted from one, and no further
/// than one past the end of what was sent.
///
/// Zero is the trap. It is what a driver produces by forgetting to convert from
/// a zero-based offset, it looks like a position, and the caret lands before the
/// first character — so it is checked for rather than assumed away.
fn lands_inside(err: &dbconn::DbError, sql: &str) {
    let Some(position) = err.statement_position() else {
        return;
    };
    assert!(position >= 1, "positions count from one, got {position}");
    assert!(
        position as usize <= sql.chars().count() + 1,
        "position {position} is past the end of a {}-character statement",
        sql.chars().count()
    );
}

/// Every metadata call answers for a relation that exists, and the answers agree
/// with each other.
async fn walks_the_navigator(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let (schema, relation) = (subject.schema.as_str(), subject.relation.as_str());

    let schemas = driver.schemas().await.expect("schemas failed");
    assert!(
        schemas.iter().any(|s| s.name == schema),
        "the navigator root should contain {schema}"
    );

    let relations = driver.relations(schema).await.expect("relations failed");
    let found = relations
        .iter()
        .find(|r| r.name == relation)
        .unwrap_or_else(|| panic!("{relation} should be listed under {schema}"));
    assert_eq!(found.schema, schema, "a relation knows where it lives");

    let columns = driver
        .columns(schema, relation)
        .await
        .expect("columns failed");
    assert!(!columns.is_empty());
    // One-based and ascending, whichever database it came from. A catalog that
    // counts from zero converts, or the same column is first here and zeroth
    // there.
    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(
            column.position,
            offset as i32 + 1,
            "column {} is out of position",
            column.name
        );
        assert!(!column.data_type.is_empty(), "a column states its own type");
    }
    assert!(
        columns.iter().any(|c| c.name == subject.key),
        "the key column should be listed"
    );

    // A table is not a view, and the distinction is what the structure pane
    // hangs a section on.
    assert_eq!(driver.definition(schema, relation).await.unwrap(), None);

    // The remaining four answer for a table that has none of them, which is the
    // case a driver is most likely to get wrong by failing instead.
    driver
        .indexes(schema, relation)
        .await
        .expect("indexes failed");
    driver
        .foreign_keys(schema, relation)
        .await
        .expect("foreign keys failed");
    driver
        .referenced_by(schema, relation)
        .await
        .expect("inbound references failed");
    driver
        .constraints(schema, relation)
        .await
        .expect("constraints failed");
    driver
        .triggers(schema, relation)
        .await
        .expect("triggers failed");
}

/// Asking about a relation that is not there is an empty answer, not a failure.
///
/// A navigator works from a tree that can be one refresh out of date, so this
/// happens in ordinary use and must not put an error on screen.
async fn answers_for_a_relation_that_is_not_there(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let schema = subject.schema.as_str();
    let missing = "no_such_relation_anywhere";

    assert!(driver.columns(schema, missing).await.unwrap().is_empty());
    assert!(driver.indexes(schema, missing).await.unwrap().is_empty());
    assert!(
        driver
            .foreign_keys(schema, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        driver
            .constraints(schema, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(driver.triggers(schema, missing).await.unwrap().is_empty());
    assert_eq!(driver.definition(schema, missing).await.unwrap(), None);
}

/// A transaction keeps a change to itself until it is committed, forgets it when
/// it is rolled back, and a savepoint undoes part of one without ending it.
///
/// What all three rest on is that the statements and the `BEGIN` reached the same
/// connection, which is why the transaction is read from while it is still open.
/// A driver that runs each statement on a borrowed connection passes every other
/// check in this file and still commits every statement on its own.
async fn controls_a_transaction(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let Some(scratch) = subject.scratch.as_ref() else {
        assert!(
            !driver.transactional(),
            "this driver offers transaction control, so the subject needs somewhere to write"
        );
        return;
    };
    assert!(
        driver.transactional(),
        "the subject has a fixture for transactions the driver says it cannot control"
    );

    // A table left behind by a run that failed part way through is the ordinary
    // state here, so the fixture is emptied rather than assumed empty.
    run(driver, &scratch.create).await;
    run(driver, &scratch.clear).await;

    driver
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");
    run(driver, &scratch.insert).await;
    assert_eq!(
        rows(driver, &scratch.read).await,
        1,
        "an open transaction should see its own change"
    );
    driver
        .transaction(&TxStep::Rollback)
        .await
        .expect("could not roll back");
    assert_eq!(
        rows(driver, &scratch.read).await,
        0,
        "a rolled-back change should be gone"
    );

    driver
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");
    run(driver, &scratch.insert).await;
    driver
        .transaction(&TxStep::Commit)
        .await
        .expect("could not commit");
    assert_eq!(
        rows(driver, &scratch.read).await,
        1,
        "a committed change should still be there"
    );

    driver
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");
    run(driver, &scratch.insert).await;
    driver
        .transaction(&TxStep::Savepoint("halfway".to_string()))
        .await
        .expect("could not set a savepoint");
    run(driver, &scratch.insert).await;
    driver
        .transaction(&TxStep::RollbackTo("halfway".to_string()))
        .await
        .expect("could not roll back to the savepoint");
    assert_eq!(
        rows(driver, &scratch.read).await,
        2,
        "rolling back to a savepoint should undo only what came after it"
    );
    driver
        .transaction(&TxStep::Release("halfway".to_string()))
        .await
        .expect("could not release the savepoint");
    driver
        .transaction(&TxStep::Rollback)
        .await
        .expect("could not roll back");
    assert_eq!(
        rows(driver, &scratch.read).await,
        1,
        "the transaction around the savepoint was rolled back too"
    );

    run(driver, &scratch.drop).await;
}

/// Runs `sql` for its effect, reading it to the end.
///
/// To the end because the trait leaves open when a statement is actually
/// executed — a driver may do the work on the first batch — so a statement sent
/// and not read is a statement that may not have run.
async fn run(driver: &dyn Driver, sql: &str) {
    let mut stream = driver
        .query(sql, 1)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    while stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .is_some()
    {}
}

/// How many rows `sql` returns.
async fn rows(driver: &dyn Driver, sql: &str) -> usize {
    let mut stream = driver
        .query(sql, 100)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut seen = 0;
    while let Some(batch) = stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        seen += batch.num_rows();
    }
    seen
}

/// The failure `sql` produces, insisting there is one.
///
/// A helper because the failure can come from either call: `query` resolving at
/// different moments per driver is something the trait deliberately leaves open,
/// so a check that only looked at one of them would pass on one database and
/// hang on the other.
async fn failure(driver: &dyn Driver, sql: &str) -> dbconn::DbError {
    match driver.query(sql, 10).await {
        Err(e) => e,
        Ok(mut stream) => match stream.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {sql}"),
        },
    }
}

async fn every_check(subject: &Subject) {
    reads_a_result_in_batches(subject).await;
    if subject.cursors {
        pages_a_cursor(subject).await;
        cancels_an_idle_cursor_without_complaining(subject).await;
    }
    reports_where_a_statement_is_wrong(subject).await;
    walks_the_navigator(subject).await;
    answers_for_a_relation_that_is_not_there(subject).await;
    controls_a_transaction(subject).await;
}

// ---------------------------------------------------------------------------
// The implementations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_satisfies_the_contract() {
    every_check(&sqlite().await).await;
}

#[tokio::test]
async fn duckdb_satisfies_the_contract() {
    every_check(&duckdb().await).await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn postgres_satisfies_the_contract() {
    every_check(&postgres().await).await;
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn mysql_satisfies_the_contract() {
    every_check(&mysql().await).await;
}

#[tokio::test]
#[ignore = "requires a SQL Server instance"]
async fn mssql_satisfies_the_contract() {
    every_check(&mssql().await).await;
}

#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn clickhouse_satisfies_the_contract() {
    every_check(&clickhouse().await).await;
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn mongodb_satisfies_the_contract() {
    every_check(&mongodb().await).await;
}

// ---------------------------------------------------------------------------
// Databases that are reached by a driver written for a different database
// ---------------------------------------------------------------------------

/// CockroachDB, through the PostgreSQL driver and no other code.
#[tokio::test]
#[ignore = "requires a CockroachDB server"]
async fn cockroachdb_satisfies_the_contract_through_the_postgres_driver() {
    let subject = pg_compatible(
        COCKROACH,
        vec![
            "DROP TABLE IF EXISTS nums".to_string(),
            "CREATE TABLE nums (id INT PRIMARY KEY, label STRING)".to_string(),
            "INSERT INTO nums (id, label) \
             SELECT g, 'row-' || g::STRING FROM generate_series(1, 500) AS g"
                .to_string(),
        ],
        "nums",
        "id",
        // The one thing that does not come across. CockroachDB speaks the
        // PostgreSQL wire protocol and this driver reads it with no changes,
        // but it does not send the error position field: it draws the caret
        // into the message text instead, under a "source SQL:" heading. So the
        // message is if anything more informative, and the editor cannot put a
        // caret anywhere from it.
        //
        // Parsing that caret back out of the prose is exactly what the position
        // field exists to avoid, and would break the day the wording changed.
        false,
        true,
        // Transactions are the part of PostgreSQL that CockroachDB is built
        // around, savepoints included.
        Some(Scratch::sql("contract_tx")),
    )
    .await;
    every_check(&subject).await;
}

/// GreptimeDB, through the PostgreSQL driver — and exactly how far that goes.
///
/// The data path works completely: it connects, runs statements, streams
/// batches and reports a syntax error, with no code written for it. The
/// navigator works down to the list of tables. Past that it stops, and the two
/// places it stops are worth stating rather than discovering:
///
/// **Cursors.** `DECLARE` and `FETCH` are accepted, and psql pages a table
/// happily. Under the extended query protocol — which is what any client
/// sending typed parameters uses — `FETCH` answers with a DataRow whose field
/// count contradicts the RowDescription it just sent, and the connection cannot
/// continue. There is no fix on this side; `LIMIT`/`OFFSET` is the thing a
/// cursor exists instead of.
///
/// **Column metadata.** `pg_index.indkey` is an int2vector in PostgreSQL and a
/// string in GreptimeDB, so the `attnum = ANY(indkey)` that finds the primary
/// key fails to plan. Rewriting that around a compatibility shim would put
/// PostgreSQL's own primary-key detection at risk to serve a database that does
/// not really have one, so it is left alone.
///
/// Five other differences did get fixed, because each was the driver assuming
/// PostgreSQL where the protocol did not require it: a null `reltuples`, `::int`
/// meaning 64 bits, `relkind` arriving as text, `FETCH FORWARD` where `FETCH`
/// says the same thing, and a missing `pg_get_triggerdef`. All five are also
/// correct against PostgreSQL, which is the test of whether a portability fix is
/// a fix or a concession.
#[tokio::test]
#[ignore = "requires a GreptimeDB server"]
async fn greptimedb_reads_data_through_the_postgres_driver() {
    let subject = pg_compatible(
        GREPTIME,
        vec![
            "DROP TABLE IF EXISTS nums".to_string(),
            // `n`, not `id`: GreptimeDB reserves `id` as a keyword. And every
            // table needs a TIME INDEX, which is the shape of the database
            // rather than a quirk — there is no table without a time column.
            "CREATE TABLE nums (\
                 n BIGINT, \
                 label STRING, \
                 ts TIMESTAMP TIME INDEX, \
                 PRIMARY KEY (n))"
                .to_string(),
            // Written out rather than generated: `generate_series` is a
            // PostgreSQL function, not part of the wire protocol under test.
            format!(
                "INSERT INTO nums (n, label, ts) VALUES {}",
                (1..=500)
                    .map(|i| format!("({i}, 'row-{i}', {})", i * 1000))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ],
        "nums",
        "n",
        false,
        false,
        // The driver says it is transactional because it is the PostgreSQL one;
        // this server is append-only and has no transactions to control. The
        // checks below are the ones it does satisfy.
        None,
    )
    .await;

    reads_a_result_in_batches(&subject).await;
    reports_where_a_statement_is_wrong(&subject).await;

    // The navigator, as far as it goes. Named checks rather than
    // `walks_the_navigator`, so that the day GreptimeDB fills in `indkey` this
    // test starts passing more of the contract instead of silently continuing
    // to assert less of it.
    let driver = subject.driver.as_ref();
    let schemas = driver.schemas().await.expect("schemas failed");
    assert!(schemas.iter().any(|s| s.name == subject.schema));
    let relations = driver
        .relations(&subject.schema)
        .await
        .expect("relations failed");
    assert!(
        relations.iter().any(|r| r.name == subject.relation),
        "the table should be listed"
    );
}

/// TiDB, through the MySQL driver and no other code.
///
/// Every check passes. Two differences in its catalog are worth stating anyway,
/// because both are invisible from here and neither is a fault the contract can
/// see.
///
/// TiDB names its system schemas in upper case — `INFORMATION_SCHEMA`,
/// `PERFORMANCE_SCHEMA`, and a `METRICS_SCHEMA` of its own — so the driver's
/// list of schemas to hide, which is written the way MySQL spells them, hides
/// none of them. A navigator against TiDB shows three schemas a navigator
/// against MySQL does not. Upper-casing the comparison would fix that and would
/// also newly hide a MySQL database genuinely named `MYSQL` or `Sys`, which on a
/// case-sensitive filesystem is a database somebody may have made on purpose.
///
/// And `information_schema.TABLES` compares `TABLE_SCHEMA` case-sensitively
/// while `information_schema.COLUMNS` does not, which is TiDB disagreeing with
/// itself. The driver's probe for `CHECK_CONSTRAINTS` asks the first of those,
/// so it concludes the table is absent when it is present, and check constraints
/// go unreported while unique constraints still work. Chasing that would mean
/// writing the probe around one server's inconsistency rather than around the
/// question it is asking.
#[tokio::test]
#[ignore = "requires a TiDB server"]
async fn tidb_satisfies_the_contract_through_the_mysql_driver() {
    let subject = mysql_compatible(
        TIDB,
        vec![
            "CREATE TABLE nums (id INT PRIMARY KEY, label VARCHAR(32))".to_string(),
            format!(
                "INSERT INTO nums (id, label) VALUES {}",
                (1..=500)
                    .map(|i| format!("({i}, 'row-{i}')"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ],
        "nums",
        "id",
        // No offset, for the same reason MySQL has none: the server sends the
        // fragment it stopped at rather than a place in the text.
        false,
        true,
    )
    .await;
    every_check(&subject).await;
}

/// StarRocks, through the MySQL driver and no other code.
///
/// Every check passes, which is further than its shape suggests it would: it is
/// a distributed column store, its tables declare how they are spread and how
/// many copies to keep, and none of that reaches the driver. Its
/// `information_schema` carries every table the nine metadata calls read except
/// `CHECK_CONSTRAINTS`, and that one is already asked about rather than assumed,
/// so unique constraints come back and checks are simply not claimed. The
/// capability probe was written for MariaDB and old MySQL and it turns out to
/// have been the right shape for this too, which is the useful result.
#[tokio::test]
#[ignore = "requires a StarRocks server"]
async fn starrocks_satisfies_the_contract_through_the_mysql_driver() {
    let subject = mysql_compatible(
        STARROCKS,
        vec![
            // A distribution and a replica count, which is where StarRocks
            // stops looking like MySQL: it is a distributed column store, so
            // every table says how it is spread and how many copies to keep,
            // and the single backend in the test container can only keep one.
            "CREATE TABLE nums (id INT, label VARCHAR(32)) \
             PRIMARY KEY(id) DISTRIBUTED BY HASH(id) \
             PROPERTIES ('replication_num' = '1')"
                .to_string(),
            format!(
                "INSERT INTO nums (id, label) VALUES {}",
                (1..=500)
                    .map(|i| format!("({i}, 'row-{i}')"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ],
        "nums",
        "id",
        false,
        true,
    )
    .await;
    every_check(&subject).await;
}
