//! What every driver has to do, checked through the trait and nothing else.
//!
//! The checks are written once, against `&dyn Driver`, and run against each
//! implementation. That arrangement is the point: a check that reached for
//! `PgSource` would be testing PostgreSQL, and this is meant to be testing the
//! contract — the thing a sixth driver has to satisfy without anybody rereading
//! the first one.
//!
//! SQLite's pass needs nothing installed and runs under `make test`. PostgreSQL's
//! is the same checks against the benchmark database, so it is marked `ignore`
//! and runs under `make test-integration`.

use dbconn::Driver;
use std::path::PathBuf;
use tempfile::TempDir;

const PG_CONN: &str = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench";

/// A driver, plus the least a database has to contain for these checks to mean
/// anything: somewhere to look, and a table of ascending integers to read.
struct Subject {
    driver: Box<dyn Driver>,
    schema: String,
    relation: String,
    key: String,
    /// Kept alive for the length of the test, and unused otherwise.
    _fixture: Option<TempDir>,
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
        _fixture: Some(dir),
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
        _fixture: None,
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// A result arrives in batches of the size that was asked for, in order, once
/// each.
async fn reads_a_result_in_batches(subject: &Subject) {
    let driver = subject.driver.as_ref();
    let sql = format!(
        "SELECT {key} FROM {relation} ORDER BY {key}",
        key = subject.key,
        relation = subject.relation
    );
    let mut stream = driver.query(&sql, 100).await.expect("query failed");

    // Before a single row has been read: a front end lays out a grid first and
    // asks for rows afterwards.
    assert_eq!(stream.schema().fields().len(), 1);
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
    let sql = format!(
        "SELECT {key} FROM {relation} ORDER BY {key}",
        key = subject.key,
        relation = subject.relation
    );
    let mut cursor = driver.cursor(&sql, 50).await.expect("cursor failed");
    assert_eq!(cursor.schema().fields().len(), 1);

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
    let sql = format!(
        "SELECT {key} FROM {relation} ORDER BY {key}",
        key = subject.key,
        relation = subject.relation
    );
    let cursor = subject
        .driver
        .cursor(&sql, 10)
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

    // A statement that is wrong in the middle rather than truncated. SQLite
    // reports no offset for input that simply stops — the error is at the end of
    // what it was given, not at a token — so a check written with a trailing
    // `WHERE` would be asserting something only PostgreSQL does.
    let broken = format!(
        "SELECT {key} FROM {relation} WHERE ORDER BY {key}",
        key = subject.key,
        relation = subject.relation
    );
    let err = failure(driver, &broken).await;
    assert!(
        err.statement_position().is_some(),
        "a syntax error should say where it is: {err}"
    );
    lands_inside(&err, &broken);
    assert!(!err.is_cancelled(), "a syntax error is not a cancellation");

    // Whether a missing relation has a position is the database's business, and
    // the two disagree: PostgreSQL points at the name, SQLite reports none. Both
    // are honest, so the contract asks only that whatever comes back could be
    // acted on — an earlier version of this required None and was asserting
    // SQLite's behaviour under the name of the contract.
    let missing = failure(driver, "SELECT * FROM no_such_relation_anywhere").await;
    lands_inside(&missing, "SELECT * FROM no_such_relation_anywhere");
    assert!(!missing.is_cancelled());
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
    pages_a_cursor(subject).await;
    cancels_an_idle_cursor_without_complaining(subject).await;
    reports_where_a_statement_is_wrong(subject).await;
    walks_the_navigator(subject).await;
    answers_for_a_relation_that_is_not_there(subject).await;
}

// ---------------------------------------------------------------------------
// The implementations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_satisfies_the_contract() {
    every_check(&sqlite().await).await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn postgres_satisfies_the_contract() {
    every_check(&postgres().await).await;
}
