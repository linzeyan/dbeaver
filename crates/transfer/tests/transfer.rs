//! One database's rows arriving in another.
//!
//! Two in-memory DuckDB connections are two independent databases, so these are
//! real database-to-database transfers with no server to start. What they check
//! is the target, never the number `transfer` returned: a writer that sent
//! nothing and counted correctly would pass a count assertion.

use arrow::array::{Array, Int32Array, Int64Array, StringArray};
use dbtransfer::transfer;
use driver_duckdb::DuckSource;

async fn pair() -> (DuckSource, DuckSource) {
    let source = DuckSource::connect(":memory:").await.unwrap();
    let target = DuckSource::connect(":memory:").await.unwrap();
    (source, target)
}

/// Runs a statement to completion.
///
/// Draining is what executes it: `query` returns once the statement is
/// accepted, so a test that dropped the stream would seed nothing and then
/// check an empty table against an empty table.
async fn run(conn: &DuckSource, statement: &str) {
    let mut stream = conn.query(statement, 1).await.unwrap();
    while stream.next_batch().await.unwrap().is_some() {}
}

/// Every `(id, note)` row in `table`, in id order.
async fn rows_of(conn: &DuckSource, table: &str) -> Vec<(i32, Option<String>)> {
    let mut stream = conn
        .query(&format!("SELECT id, note FROM {table} ORDER BY id"), 1)
        .await
        .unwrap();
    let mut rows = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let notes = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            let note = (!notes.is_null(row)).then(|| notes.value(row).to_string());
            rows.push((ids.value(row), note));
        }
    }
    rows
}

/// The one number a `SELECT count(*)` answered with.
async fn count(conn: &DuckSource, sql: &str) -> i64 {
    let mut stream = conn.query(sql, 1).await.unwrap();
    let batch = stream
        .next_batch()
        .await
        .unwrap()
        .expect("a count always answers a row");
    batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0)
}

const TWO_COLUMNS: &str = "CREATE TABLE t (id INTEGER, note VARCHAR)";

#[tokio::test]
async fn rows_put_into_one_database_come_out_of_the_other() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(&target, TWO_COLUMNS).await;
    run(&source, "INSERT INTO t VALUES (1, 'hello'), (2, 'world')").await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    let rows = transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string())
        .await
        .unwrap();

    assert_eq!(rows, 2);
    assert_eq!(
        rows_of(&target, "t").await,
        vec![(1, Some("hello".into())), (2, Some("world".into()))]
    );
}

#[tokio::test]
async fn an_apostrophe_survives_the_trip() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(&target, TWO_COLUMNS).await;
    run(&source, "INSERT INTO t VALUES (1, 'O''Brien')").await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string())
        .await
        .unwrap();

    // Unescaped, this is a syntax error rather than a wrong value — which is
    // the good case. The bad one is a value that ends the literal early and
    // leaves whatever follows it to be read as SQL.
    assert_eq!(
        rows_of(&target, "t").await,
        vec![(1, Some("O'Brien".into()))]
    );
}

#[tokio::test]
async fn a_null_arrives_as_a_null_and_not_as_an_empty_string() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(&target, TWO_COLUMNS).await;
    run(&source, "INSERT INTO t VALUES (1, NULL), (2, '')").await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string())
        .await
        .unwrap();

    // Asked of the database rather than of the rows read back, because this is
    // the distinction someone writes a WHERE clause against.
    assert_eq!(
        count(&target, "SELECT count(*) FROM t WHERE note IS NULL").await,
        1,
        "the NULL row is still NULL and the empty string is not"
    );
    assert_eq!(
        rows_of(&target, "t").await,
        vec![(1, None), (2, Some(String::new()))]
    );
}

#[tokio::test]
async fn a_result_longer_than_one_statement_arrives_whole() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(&target, TWO_COLUMNS).await;

    // More than the 200 rows one statement carries, so the batch is split and
    // the rows either side of the seam are the ones at risk.
    let values: Vec<String> = (0..500).map(|i| format!("({i}, 'row{i}')")).collect();
    run(
        &source,
        &format!("INSERT INTO t VALUES {}", values.join(",")),
    )
    .await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    let rows = transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string())
        .await
        .unwrap();

    assert_eq!(rows, 500);
    let arrived = rows_of(&target, "t").await;
    assert_eq!(arrived.len(), 500);
    assert_eq!(arrived[0], (0, Some("row0".into())));
    assert_eq!(
        arrived[199],
        (199, Some("row199".into())),
        "last of the first"
    );
    assert_eq!(
        arrived[200],
        (200, Some("row200".into())),
        "first of the next"
    );
    assert_eq!(arrived[499], (499, Some("row499".into())));
}

#[tokio::test]
async fn an_empty_result_sends_nothing_at_all() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(&target, TWO_COLUMNS).await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    let rows = transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string())
        .await
        .unwrap();

    assert_eq!(rows, 0);
    // An `INSERT INTO t (id, note) VALUES ;` is a syntax error, so a writer
    // built before the first batch arrived would fail here rather than write
    // nothing.
    assert_eq!(count(&target, "SELECT count(*) FROM t").await, 0);
}

#[tokio::test]
async fn a_statement_the_target_refuses_fails_the_transfer() {
    let (source, target) = pair().await;
    run(&source, TWO_COLUMNS).await;
    run(
        &target,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, note VARCHAR)",
    )
    .await;
    run(&target, "INSERT INTO t VALUES (1, 'already here')").await;
    run(&source, "INSERT INTO t VALUES (1, 'duplicate')").await;

    let mut cursor = source.cursor("SELECT id, note FROM t", 1).await.unwrap();
    let result = transfer(&mut cursor, &target, &dbsql::DUCKDB, "t".to_string()).await;

    // The statement is accepted and then refused, so this is what the drain in
    // `TargetWriter::write` is for. Without it the transfer reports the row as
    // written and the row is not there.
    let error = result.expect_err("a duplicate key must not be reported as written");
    assert!(
        error.to_string().to_lowercase().contains("constraint")
            || error.to_string().to_lowercase().contains("duplicate"),
        "the failure should name the constraint, got: {error}"
    );
    assert_eq!(
        count(&target, "SELECT count(*) FROM t").await,
        1,
        "the refused row did not arrive"
    );
}
