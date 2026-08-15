//! A transfer between two different databases, against a real server.
//!
//! The DuckDB suite next door cannot check the thing that matters most here.
//! DuckDB answers `query` only once the statement has run, so a violated
//! constraint comes back from `query` itself and a writer that never read the
//! stream would look correct. PostgreSQL accepts the statement first and reports
//! the failure on the first read, which is why `TargetWriter` drains — and this
//! is the only place that distinction is observable.
//!
//! Requires `make db-seed`. Run with `make test-postgres`.

use dbtransfer::transfer;
use driver_duckdb::DuckSource;
use driver_postgres::PgSource;

const CONN: &str = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench";

async fn postgres() -> PgSource {
    PgSource::connect(CONN)
        .await
        .expect("benchmark database unreachable; run `make db-seed`")
}

/// Runs a statement to completion, which on PostgreSQL means reading it.
async fn run_pg(conn: &PgSource, statement: &str) {
    let mut stream = conn.query(statement, 1).await.expect("statement accepted");
    while stream
        .next_batch()
        .await
        .expect("statement executed")
        .is_some()
    {}
}

async fn run_duck(conn: &DuckSource, statement: &str) {
    let mut stream = conn.query(statement, 1).await.expect("statement accepted");
    while stream
        .next_batch()
        .await
        .expect("statement executed")
        .is_some()
    {}
}

async fn count_pg(conn: &PgSource, sql: &str) -> i64 {
    let mut stream = conn.query(sql, 1).await.expect("count accepted");
    let batch = stream
        .next_batch()
        .await
        .expect("count executed")
        .expect("a count always answers a row");
    batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("count comes back as bigint")
        .value(0)
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn rows_read_from_duckdb_land_in_postgresql() {
    let source = DuckSource::connect(":memory:").await.expect("duckdb");
    let target = postgres().await;

    run_duck(&source, "CREATE TABLE t (id INTEGER, note VARCHAR)").await;
    // Over the 200 rows one statement carries, so more than one reaches the
    // server, and one value that would end its own literal if unescaped.
    let mut values: Vec<String> = (0..300).map(|i| format!("({i}, 'row{i}')")).collect();
    values.push("(300, 'O''Brien')".to_string());
    run_duck(
        &source,
        &format!("INSERT INTO t VALUES {}", values.join(",")),
    )
    .await;

    run_pg(&target, "DROP TABLE IF EXISTS transfer_landing").await;
    run_pg(
        &target,
        "CREATE TABLE transfer_landing (id integer, note text)",
    )
    .await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 128).await.unwrap();
    let rows = transfer(
        &mut cursor,
        &target,
        &dbsql::POSTGRES,
        "transfer_landing".to_string(),
    )
    .await
    .expect("transfer failed");

    assert_eq!(rows, 301);
    assert_eq!(
        count_pg(&target, "SELECT count(*) FROM transfer_landing").await,
        301,
        "every row the source held is on the server"
    );
    assert_eq!(
        count_pg(
            &target,
            "SELECT count(*) FROM transfer_landing WHERE note = 'O''Brien'",
        )
        .await,
        1,
        "the apostrophe arrived as data, not as syntax"
    );

    run_pg(&target, "DROP TABLE transfer_landing").await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_row_postgresql_refuses_fails_the_transfer() {
    let source = DuckSource::connect(":memory:").await.expect("duckdb");
    let target = postgres().await;

    run_duck(&source, "CREATE TABLE t (id INTEGER)").await;
    run_duck(&source, "INSERT INTO t VALUES (1)").await;

    run_pg(&target, "DROP TABLE IF EXISTS transfer_refused").await;
    run_pg(
        &target,
        "CREATE TABLE transfer_refused (id integer primary key)",
    )
    .await;
    run_pg(&target, "INSERT INTO transfer_refused VALUES (1)").await;

    let mut cursor = source.cursor("SELECT id FROM t", 128).await.unwrap();
    let result = transfer(
        &mut cursor,
        &target,
        &dbsql::POSTGRES,
        "transfer_refused".to_string(),
    )
    .await;

    // The server accepted this statement and then refused it. Drop the stream
    // instead of draining it and the error is never read: the transfer returns
    // Ok(1) for a row that does not exist.
    let error = result.expect_err("a duplicate key must not be reported as written");
    assert!(
        error.to_string().contains("duplicate key"),
        "the failure should say what the server said, got: {error}"
    );
    assert_eq!(
        count_pg(&target, "SELECT count(*) FROM transfer_refused").await,
        1,
        "the refused row did not arrive"
    );

    run_pg(&target, "DROP TABLE transfer_refused").await;
}
