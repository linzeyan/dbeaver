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

use dbconn::{Browse, Driver, TxStep};
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
    /// Whether this database has savepoints at all.
    ///
    /// A property of the database and not of the driver, which is why it is
    /// recorded here beside the statements rather than asked of `Driver`. The
    /// trait already anticipated the split — "a step this database does not have
    /// is refused rather than skipped" — and DuckDB is the case it named:
    /// `SAVEPOINT`, `ROLLBACK TO` and `RELEASE` are syntax errors in its parser,
    /// not features behind a setting.
    ///
    /// It does not weaken `transactional`. The question that answers is whether
    /// statements on the session can be wrapped in a transaction, and DuckDB's
    /// can; a client that hid Commit and Rollback over a missing savepoint would
    /// be withholding the two buttons the database does have.
    savepoints: bool,
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
            savepoints: true,
        }
    }

    /// The same, on a database with no savepoints.
    fn without_savepoints(mut self) -> Self {
        self.savepoints = false;
        self
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
        scratch: Some(Scratch::sql("contract_tx")),
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
        // The one subject here whose database has transactions and no
        // savepoints. `BEGIN`, `COMMIT` and `ROLLBACK` all work on the session
        // connection; the other three are syntax errors in DuckDB's parser, and
        // the check above insists the driver says so rather than passing over
        // them.
        scratch: Some(Scratch::sql("contract_tx").without_savepoints()),
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

/// Database 9, because a Redis server has sixteen numbered databases and no way
/// to make a seventeenth. The driver's own suite uses another one, for the reason
/// `mysql()` gives: `cargo test --workspace -- --ignored` runs both binaries at
/// once, and a shared fixture would turn a scheduling accident into a contract
/// violation.
const REDIS_URL: &str = "redis://127.0.0.1:56379/9";

/// Redis, whose relations are its six value types rather than its keys.
///
/// The subject that stretches this file furthest, and every field below is
/// answered from what Redis actually does rather than from what the other
/// subjects needed.
///
/// Seeded through the `redis` crate rather than through the driver, so the
/// fixture does not depend on the code under test being right.
async fn redis() -> Subject {
    let client = redis::Client::open(REDIS_URL).expect("the fixture URL should parse");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("Redis unreachable; run `make db-up-redis`");
    redis::cmd("FLUSHDB")
        .exec_async(&mut conn)
        .await
        .expect("could not clear the fixture");
    // Five hundred string keys and nothing else, which is what makes two of the
    // fields below mean anything: `string` has rows to page through, and
    // `stream` has none at all.
    let mut seed = redis::pipe();
    for i in 1..=500 {
        seed.cmd("SET").arg(format!("nums:{i}")).arg(i).ignore();
    }
    seed.exec_async(&mut conn)
        .await
        .expect("seeding the fixture");

    let driver = driver_redis::RedisSource::connect(REDIS_URL)
        .await
        .expect("the Redis driver could not connect");
    Subject {
        driver: Box::new(driver),
        // Redis's databases are numbered and cannot be named; `db9` is what the
        // navigator shows and `SELECT 9` is what a statement sends.
        schema: "db9".to_string(),
        // A type, not a key. A relation per key would put half a million entries
        // in the navigator, so the rows of `string` are the keys that hold
        // strings — see the driver's crate doc.
        relation: "string".to_string(),
        // Every listing of keys carries the key it lists, and it is the primary
        // key in every sense that matters: unique, and the only way to address
        // the row.
        key: "key".to_string(),
        // Two lines, because Redis has no way to name a database inside a
        // command: the database is a property of the connection, so a statement
        // that reads db9 has to select it first.
        read: "SELECT 9\nSCAN 0 TYPE string".to_string(),
        // Broken in the middle in this database's own language: line one is
        // fine, line two is not a command. Two lines on purpose — the driver
        // reports the offset of the line that failed and declines to report one
        // for a single-line statement, where "line 1" locates nothing.
        broken: "SELECT 9\nWIBBLE nums".to_string(),
        // The nearest thing Redis has to a relation that is not there: a type
        // with no keys of it. The fixture seeds only strings, so this scans the
        // whole database and finds nothing.
        missing: "SELECT 9\nSCAN 0 TYPE stream".to_string(),
        // False, and it is not a near miss. The six relations always exist —
        // they are the fixed vocabulary of the database, not something anybody
        // created — so there is no name a browse could fail on. Reading a type
        // with no keys walks the keyspace and returns nothing, which is the same
        // answer as reading an empty table.
        missing_is_a_failure: false,
        // False, and this is the one place Redis cannot meet the trait. `SCAN`
        // is its own cursor and it gives the first half of what `Driver::cursor`
        // asks for exactly — page *n* costs what page one costs — but not the
        // second: a key present throughout is returned at least once, and *may
        // be returned twice* if the hash table is resized under the iteration,
        // which adding or removing enough keys causes.
        //
        // The driver could hide the repeats by remembering every key it has
        // returned. It does not, and `RedisSource::cursor` argues why at length:
        // the memory is unbounded in the size of the browse, which is what
        // paging exists to avoid, and it would still not make a key created
        // mid-iteration appear. So the cursor is real, the Content tab uses it,
        // and the driver's own suite pages through it — but the guarantee is not
        // claimed here, because claiming it quietly would be worse than not
        // having it.
        cursors: false,
        // True, and not from anything Redis sends. A rejected command names what
        // it disliked and never where in the text it was; what the driver knows
        // instead is its own grammar — one command per line — so it reports the
        // offset of the line whose command failed. That points at the right
        // command and no closer, which is the same bargain the SQL Server driver
        // strikes with a line number.
        positions: true,
        // No fixture, because there is no transaction to control. `MULTI` queues
        // commands and `EXEC` runs the queue; a command sent between them answers
        // QUEUED rather than a value, so nothing can be read back before deciding
        // whether to keep it. That is a batch, and the check below asserts the
        // driver says so rather than offering buttons that would mislead.
        scratch: None,
        _fixture: None,
    }
}

const CASSANDRA_NODE: &str = "127.0.0.1:59042";

/// Cassandra, in the one database here whose rows cannot be given a total order
/// by asking for one.
///
/// `read` pins the partition, and that is not a stylistic choice. CQL allows
/// `ORDER BY` only on clustering columns and only once the partition key is
/// restricted, so `SELECT id FROM nums ORDER BY id` over a whole table is not a
/// slower plan, it is a refused statement. The fixture is a single partition —
/// `bucket = 0` — which is the one table shape that can honour what `read`
/// promises the checks. `Driver::browse` reads the whole table and therefore
/// declines to order it at all; see the driver.
///
/// Seeded through the `scylla` crate rather than the driver, as MongoDB's and
/// MySQL's are, with the one concession that the seeding session installs the
/// same address translator `CassandraSource::connect` does. Without it the driver
/// crate cannot reach a Cassandra published on any host port but 9042 — the
/// reasoning is in `OneEndpoint` in the driver, and `driver-cassandra`'s own
/// suite pins it.
async fn cassandra() -> Subject {
    use scylla::client::session_builder::SessionBuilder;
    use scylla::errors::TranslationError;
    use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
    use std::net::SocketAddr;
    use std::sync::Arc;

    struct AtNode(SocketAddr);

    #[async_trait::async_trait]
    impl AddressTranslator for AtNode {
        async fn translate_address(
            &self,
            _peer: &UntranslatedPeer,
        ) -> Result<SocketAddr, TranslationError> {
            Ok(self.0)
        }
    }

    let at: SocketAddr = CASSANDRA_NODE.parse().expect("a literal address");
    let session = SessionBuilder::new()
        .known_node_addr(at)
        .address_translator(Arc::new(AtNode(at)))
        .build()
        .await
        .expect("Cassandra unreachable; run `make db-up-cassandra`");

    // Written in fifties, in the one case Cassandra's own documentation endorses
    // a batch for: every statement hits the same partition, so the coordinator
    // applies it as a single mutation instead of fanning out.
    let mut seed = vec![
        "CREATE KEYSPACE IF NOT EXISTS dbclient_contract WITH replication = \
         {'class': 'SimpleStrategy', 'replication_factor': 1}"
            .to_string(),
        "CREATE TABLE IF NOT EXISTS dbclient_contract.nums \
         (bucket int, id int, label text, PRIMARY KEY (bucket, id))"
            .to_string(),
    ];
    for chunk in (1..=500).collect::<Vec<i32>>().chunks(50) {
        let mut batch = String::from("BEGIN UNLOGGED BATCH ");
        for i in chunk {
            batch.push_str(&format!(
                "INSERT INTO dbclient_contract.nums (bucket, id, label) \
                 VALUES (0, {i}, 'row-{i}'); "
            ));
        }
        batch.push_str("APPLY BATCH");
        seed.push(batch);
    }
    for statement in &seed {
        session
            .query_unpaged(statement.as_str(), &[])
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }

    let driver =
        driver_cassandra::CassandraSource::connect("cassandra://127.0.0.1:59042/dbclient_contract")
            .await
            .expect("the Cassandra driver could not connect");
    Subject {
        driver: Box::new(driver),
        // A keyspace, which is the level Cassandra has and the only one.
        schema: "dbclient_contract".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM dbclient_contract.nums WHERE bucket = 0 ORDER BY id".to_string(),
        broken: "SELECT id FROM dbclient_contract.nums WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM dbclient_contract.no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        cursors: true,
        // Cassandra's parser reports `line 1:34`, counted in characters, which
        // is one of the few here that needs no reconstructing.
        positions: true,
        // Nothing to write to, because there is no transaction to control. A
        // lightweight transaction is one statement's compare-and-set and a
        // BATCH is sent whole, so neither has a moment in which a client could
        // read its own uncommitted change — which is the thing the transaction
        // check exists to observe.
        scratch: None,
        _fixture: None,
    }
}

const TRINO_ORIGIN: &str = "http://127.0.0.1:58080";

/// Trino, which stores nothing and is therefore the first subject here whose
/// fixture has to name the system the data is in.
///
/// Its schema is `memory.dbclient_contract` — a catalog and a schema, flattened
/// into the one string the trait has, which is DuckDB's arrangement arrived at
/// for a different reason: DuckDB can attach two databases with a schema called
/// `main`, and every Trino name has a catalog in it because a catalog is which
/// connector the rows come from.
///
/// `memory` because it is the only catalog on a stock coordinator that takes a
/// write at all; `tpch` and `system` are both read-only. Its own schema rather
/// than the driver suite's, for the reason `mysql()` gives.
///
/// Seeded over the client protocol directly rather than through the driver, so
/// the fixture does not depend on the code under test being right. Trino has no
/// vendor crate on crates.io to reach for — the client protocol is HTTP and JSON,
/// and `trino_seed` below is the whole of it.
async fn trino() -> Subject {
    for statement in [
        "CREATE SCHEMA IF NOT EXISTS memory.dbclient_contract",
        "DROP TABLE IF EXISTS memory.dbclient_contract.nums",
        "CREATE TABLE memory.dbclient_contract.nums AS \
         SELECT id, 'row-' || CAST(id AS varchar) AS label \
         FROM UNNEST(sequence(1, 500)) AS t(id)",
    ] {
        trino_seed(statement).await;
    }

    let driver =
        driver_trino::TrinoSource::connect(&format!("{TRINO_ORIGIN}/memory/dbclient_contract"))
            .await
            .expect("Trino unreachable; run `make db-up-trino`");
    Subject {
        driver: Box::new(driver),
        schema: "memory.dbclient_contract".to_string(),
        relation: "nums".to_string(),
        key: "id".to_string(),
        read: "SELECT id FROM nums ORDER BY id".to_string(),
        broken: "SELECT id FROM nums WHERE ORDER BY id".to_string(),
        missing: "SELECT * FROM no_such_relation_anywhere".to_string(),
        missing_is_a_failure: true,
        // A query and a cursor are the same call here. The chain of `nextUri`s a
        // statement answers with is one execution read forward, so both of the
        // properties the trait asks a cursor for hold without a second mechanism.
        cursors: true,
        // `line 1:45`, counted in code points rather than bytes or UTF-16 code
        // units — the one thing this driver got for free that the two before it
        // had to reconstruct.
        positions: true,
        // Nothing to write to, and the reason is the interesting part: Trino's
        // protocol does have interactive transactions, and they work. What does
        // not work is writing inside one. A Trino transaction belongs to the
        // coordinator and each connector decides whether it will take a write in
        // it; `memory`, the only writable catalog here, refuses every one with
        // `AUTOCOMMIT_WRITE_CONFLICT` and the refusal aborts the transaction. So
        // there is no fixture to give this — `Scratch::insert` inside `BEGIN`
        // cannot succeed on any catalog a stock coordinator has — which is the
        // same conclusion `Driver::transactional` reaches from the other side.
        // The driver's own suite pins the measurement.
        scratch: None,
        _fixture: None,
    }
}

/// One statement over Trino's client protocol, read to the end.
///
/// `POST /v1/statement`, then follow `nextUri` until there is none. Everything
/// this needs from the answer is whether an `error` appeared, so the body is read
/// as a `serde_json::Value` rather than given a shape.
async fn trino_seed(sql: &str) {
    use http_body_util::{BodyExt, Full};
    use hyper::{Method, Request};

    let client: hyper_util::client::legacy::Client<_, Full<bytes::Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let mut method = Method::POST;
    // The coordinator refuses a request with no user even with authentication
    // switched off, so this is not optional.
    let mut uri = format!("{TRINO_ORIGIN}/v1/statement");
    let mut body = Full::new(bytes::Bytes::from(sql.to_string()));
    loop {
        let request = Request::builder()
            .method(method)
            .uri(&uri)
            .header("X-Trino-User", "contract")
            .body(body)
            .expect("a request");
        let response = client
            .request(request)
            .await
            .expect("Trino unreachable; run `make db-up-trino`");
        let answer: serde_json::Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("a body")
                .to_bytes(),
        )
        .expect("Trino answers JSON");
        if let Some(failure) = answer.get("error") {
            panic!("seeding failed on {sql}: {failure}");
        }
        let Some(next) = answer.get("nextUri").and_then(|next| next.as_str()) else {
            return;
        };
        method = Method::GET;
        uri = next.to_string();
        body = Full::default();
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
        scratch: Some(Scratch::sql("contract_tx")),
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
            // `SAVE TRANSACTION` and `ROLLBACK TRANSACTION <name>`, spelled by
            // the driver. Release is the T-SQL no-op described there.
            savepoints: true,
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
    scratch: Option<Scratch>,
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
        // Per server rather than per driver, which is the one place these two
        // subjects disagree: the MySQL driver probes for transaction control at
        // connect, and TiDB and StarRocks answer differently.
        scratch,
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
    if scratch.savepoints {
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
    } else {
        // Checked rather than skipped, which is the whole difference between a
        // database that has no savepoints and a driver that forgot them. All
        // three have to say no: one that quietly did nothing would leave
        // somebody believing there is a point they can come back to, and they
        // would find out by rolling back further than they meant to.
        for step in [
            TxStep::Savepoint("halfway".to_string()),
            TxStep::RollbackTo("halfway".to_string()),
            TxStep::Release("halfway".to_string()),
        ] {
            assert!(
                driver.transaction(&step).await.is_err(),
                "{step:?} should be refused by a database that has no savepoints"
            );
        }
        // And refusing one leaves the transaction usable, or the refusal has
        // cost more than the missing feature.
        run(driver, &scratch.insert).await;
        assert_eq!(rows(driver, &scratch.read).await, 3);
    }
    driver
        .transaction(&TxStep::Rollback)
        .await
        .expect("could not roll back");
    assert_eq!(
        rows(driver, &scratch.read).await,
        1,
        "the transaction was rolled back, savepoints or no savepoints"
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
    browses_a_relation(subject).await;
    if subject.cursors {
        pages_a_cursor(subject).await;
        cancels_an_idle_cursor_without_complaining(subject).await;
    }
    reports_where_a_statement_is_wrong(subject).await;
    walks_the_navigator(subject).await;
    answers_for_a_relation_that_is_not_there(subject).await;
    controls_a_transaction(subject).await;
}

/// The statement a navigator writes for a table is one this database runs.
///
/// The check that did not exist, and the defect it would have caught was in the
/// front end rather than in any driver: a window that had only met PostgreSQL
/// assembled `SELECT * FROM "schema"."relation"` for every database it could
/// open. MySQL reads those quotes as a string and answers with a syntax error;
/// MongoDB has no SELECT at all. Both are now the driver's answer, and this runs
/// it rather than comparing it to an expected spelling — an expected string
/// would be this file deciding what MySQL's quoting is.
async fn browses_a_relation(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let keys = [subject.key.clone()];
    let statement = driver.browse(&Browse {
        schema: &subject.schema,
        relation: &subject.relation,
        filter: None,
        order: None,
        keys: &keys,
        limit: None,
    });
    let mut stream = driver
        .query(&statement, 10)
        .await
        .unwrap_or_else(|e| panic!("the browse statement did not run: {statement}: {e}"));
    let first = stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("the browse statement failed while running: {statement}: {e}"))
        .expect("a browse of a table with rows should produce a batch");
    assert!(
        first.schema().field_with_name(&subject.key).is_ok(),
        "a browse reads every column, so it carries the key: {statement}"
    );
    // Let go before asking for anything else. A result holds the session
    // connection on every driver that can keep a transaction open, so a second
    // statement started while this one is alive waits for it — forever, since
    // nothing here would ever read the rest of it.
    drop(stream);

    // The row ceiling, which is the one part of the statement a caller adds
    // rather than the driver: a front end seeding an editor wants a statement
    // that cannot fetch a million rows by accident.
    let bounded = driver.browse(&Browse {
        schema: &subject.schema,
        relation: &subject.relation,
        filter: None,
        order: None,
        keys: &keys,
        limit: Some(3),
    });
    let mut stream = driver
        .query(&bounded, 10)
        .await
        .unwrap_or_else(|e| panic!("the bounded browse did not run: {bounded}: {e}"));
    let mut rows = 0;
    while let Some(batch) = stream.next_batch().await.expect("bounded browse failed") {
        rows += batch.num_rows();
    }
    assert_eq!(
        rows, 3,
        "a browse limited to three rows should read three: {bounded}"
    );
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

#[tokio::test]
#[ignore = "requires a Redis server"]
async fn redis_satisfies_the_contract() {
    every_check(&redis().await).await;
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn cassandra_satisfies_the_contract() {
    every_check(&cassandra().await).await;
}

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn trino_satisfies_the_contract() {
    every_check(&trino().await).await;
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
        // Transactions are the part of MySQL that TiDB implements completely,
        // savepoints included.
        Some(Scratch::sql("contract_tx")),
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
///
/// Transactions are where the shape finally shows through, and the driver has to
/// probe for them to find out. `BEGIN` and `COMMIT` work; `SAVEPOINT` is a
/// syntax error, and a `SELECT` inside an open transaction refuses to read a
/// table that transaction has written. Neither is something the MySQL driver can
/// paper over, and both are things a front end would offer buttons for, so this
/// subject carries no transaction fixture and the check asserts the refusal.
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
        // The one check this server does not satisfy, and the driver says so
        // itself rather than being told: `SAVEPOINT` is a syntax error here, and
        // a statement inside an open transaction cannot read a table the
        // transaction has written — "SELECT cannot read table 't' modified
        // earlier in the same transaction". So `controls_a_transaction` asserts
        // the refusal instead, which is a real check: the day StarRocks grows
        // savepoints the probe notices, `transactional` turns true, and this
        // fails until somebody puts a fixture back.
        None,
    )
    .await;
    every_check(&subject).await;
}
