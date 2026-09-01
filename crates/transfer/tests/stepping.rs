//! A transfer taken one batch at a time, watched, and stopped.
//!
//! Two in-memory DuckDB connections again — see `transfer.rs` for why that is a
//! real database-to-database transfer — and the same rule about what is
//! asserted: the target, not the number the transfer reported. A stepper that
//! counted rows it never sent would pass every count assertion in this file if
//! the target were not read back.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arrow::array::{Int32Array, Int64Array};
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor, CursorCancel, DatabaseInfo, DbResult,
    Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo,
    ServerProcesses, TriggerInfo, TxStep, UniqueKeyInfo,
};
use dbtransfer::{Step, Transfer};
use driver_duckdb::DuckSource;

async fn pair() -> (DuckSource, Arc<DuckSource>) {
    let source = DuckSource::connect(":memory:").await.unwrap();
    let target = DuckSource::connect(":memory:").await.unwrap();
    (source, Arc::new(target))
}

async fn run(conn: &DuckSource, statement: &str) {
    let mut stream = conn.query(statement, 1).await.unwrap();
    while stream.next_batch().await.unwrap().is_some() {}
}

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

/// The ids on the target, in order, so a partial transfer can be told from a
/// scrambled one.
async fn ids(conn: &DuckSource, table: &str) -> Vec<i32> {
    let mut stream = conn
        .query(&format!("SELECT id FROM {table} ORDER BY id"), 1)
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        out.extend((0..batch.num_rows()).map(|row| column.value(row)));
    }
    out
}

const ONE_COLUMN: &str = "CREATE TABLE t (id INTEGER)";

/// Seeds `count` rows into `t` on both sides' shape, source only.
async fn seed(source: &DuckSource, rows: i32) {
    let values: Vec<String> = (0..rows).map(|i| format!("({i})")).collect();
    run(
        source,
        &format!("INSERT INTO t VALUES {}", values.join(",")),
    )
    .await;
}

/// A count that is readable between batches, which is the whole reason this
/// exists: the blocking `transfer` answers once, at the end, and a person
/// watching a million rows move has nothing to read until it is over.
#[tokio::test]
async fn the_count_is_readable_between_batches() {
    let (source, target) = pair().await;
    run(&source, ONE_COLUMN).await;
    run(&target, ONE_COLUMN).await;
    seed(&source, 6).await;

    // Two rows per fetch, so the total has to arrive in three visible pieces
    // rather than in one.
    let mut cursor = source
        .cursor("SELECT id FROM t ORDER BY id", 2)
        .await
        .unwrap();
    let mut moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());

    let mut seen = Vec::new();
    loop {
        match moving.step(&mut cursor).await.unwrap() {
            Step::Moved(rows) => seen.push(rows),
            Step::Done(rows) => {
                seen.push(rows);
                break;
            }
            Step::Stopped(rows) => panic!("nothing asked it to stop, at {rows}"),
        }
    }

    assert_eq!(
        seen,
        vec![2, 4, 6, 6],
        "each step reports the running total, and the last one reports it again as done"
    );
    assert_eq!(moving.moved(), 6, "and the transfer holds the same number");
    assert_eq!(ids(&target, "t").await, (0..6).collect::<Vec<_>>());
}

/// Stop is answered at the next step, and what had already arrived stays.
///
/// The rows are the point of the assertion. A transfer is not a transaction —
/// the target may not have one — so the honest promise is "nothing more is
/// sent", and a caller that believed it was all-or-nothing would leave half a
/// table behind and call it clean.
#[tokio::test]
async fn a_stop_is_answered_at_the_next_step_and_leaves_what_arrived() {
    let (source, target) = pair().await;
    run(&source, ONE_COLUMN).await;
    run(&target, ONE_COLUMN).await;
    seed(&source, 10).await;

    let mut cursor = source
        .cursor("SELECT id FROM t ORDER BY id", 2)
        .await
        .unwrap();
    let mut moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());
    let stopper = moving.stopper(Arc::from(cursor.canceller()));

    assert_eq!(moving.step(&mut cursor).await.unwrap(), Step::Moved(2));
    assert!(!stopper.was_asked(), "nobody has pressed it yet");

    stopper.stop().await.expect("stopping is delivered");
    assert!(stopper.was_asked());

    assert_eq!(
        moving.step(&mut cursor).await.unwrap(),
        Step::Stopped(2),
        "the step after the stop sends nothing and reports what had gone"
    );
    // And it stays stopped: a second step is not a way back in.
    assert_eq!(moving.step(&mut cursor).await.unwrap(), Step::Stopped(2));

    assert_eq!(
        ids(&target, "t").await,
        vec![0, 1],
        "the rows already sent are still there, and no more followed them"
    );
}

/// A stop before anything has moved sends nothing at all.
///
/// The case that would be missed by checking a partial transfer: the writer is
/// built from the first batch, so a stop that arrived before it would be a step
/// asked to check a flag it had not reached yet.
#[tokio::test]
async fn a_stop_before_the_first_batch_sends_nothing() {
    let (source, target) = pair().await;
    run(&source, ONE_COLUMN).await;
    run(&target, ONE_COLUMN).await;
    seed(&source, 4).await;

    let mut cursor = source.cursor("SELECT id FROM t", 2).await.unwrap();
    let mut moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());
    moving
        .stopper(Arc::from(cursor.canceller()))
        .stop()
        .await
        .expect("stopping is delivered");

    assert_eq!(moving.step(&mut cursor).await.unwrap(), Step::Stopped(0));
    assert_eq!(count(&target, "SELECT count(*) FROM t").await, 0);
}

/// An empty result is done rather than moved, and writes no statement.
///
/// `INSERT INTO t (id) VALUES ;` is a syntax error, so a writer built before the
/// first batch arrived fails here instead of writing nothing.
#[tokio::test]
async fn an_empty_result_is_done_at_the_first_step() {
    let (source, target) = pair().await;
    run(&source, ONE_COLUMN).await;
    run(&target, ONE_COLUMN).await;

    let mut cursor = source.cursor("SELECT id FROM t", 2).await.unwrap();
    let mut moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());

    assert_eq!(moving.step(&mut cursor).await.unwrap(), Step::Done(0));
    assert_eq!(count(&target, "SELECT count(*) FROM t").await, 0);
}

/// A target that refuses a statement fails the step, with the rows before it
/// still counted.
///
/// The count is what a caller shows next to Stop, and after a failure it has to
/// describe the target rather than the attempt.
#[tokio::test]
async fn a_refused_statement_fails_the_step() {
    let (source, target) = pair().await;
    run(&source, ONE_COLUMN).await;
    run(&target, "CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    run(&target, "INSERT INTO t VALUES (99)").await;
    run(&source, "INSERT INTO t VALUES (1), (99)").await;

    let mut cursor = source
        .cursor("SELECT id FROM t ORDER BY id", 1)
        .await
        .unwrap();
    let mut moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());

    assert_eq!(moving.step(&mut cursor).await.unwrap(), Step::Moved(1));
    let error = moving
        .step(&mut cursor)
        .await
        .expect_err("a duplicate key must not be reported as written");
    let said = error.to_string().to_lowercase();
    assert!(
        said.contains("constraint") || said.contains("duplicate"),
        "the failure should name the constraint, got: {error}"
    );
    assert_eq!(
        moving.moved(),
        1,
        "and the count still describes the target: one row went, the other did not"
    );
}

/// A target that answers nothing and counts the cancels it is sent.
///
/// Everything but `cancel` is unreachable, which is the point: this exists to
/// witness one call. A real database cannot witness it — `Driver::cancel` on an
/// idle DuckDB connection succeeds whether or not anything was listening, so a
/// test against the real thing would pass with the target half deleted.
struct Deaf {
    cancels: AtomicUsize,
}

#[async_trait]
impl Driver for Deaf {
    async fn cancel(&self) -> DbResult<()> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: false,
            // A transfer reads rows and writes rows; a routine is never either
            // end of one.
            reports_routines: false,
            // Nor sequences, for the same reason as the line above.
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
        }
    }
    async fn server_info(&self) -> DbResult<ServerInfo> {
        unreachable!("a stop asks nobody who they are")
    }
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        unreachable!("a transfer names its table")
    }
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        unreachable!("a transfer names its table")
    }
    async fn relations(&self, _: &str) -> DbResult<Vec<RelationInfo>> {
        unreachable!("a transfer names its table")
    }
    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        unreachable!("the batch carries the schema")
    }
    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        unreachable!("nothing here reads a view")
    }
    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("nothing here reads an index")
    }
    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("nothing here names a row")
    }
    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("nothing here follows a key")
    }
    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("nothing here follows a key")
    }
    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        unreachable!("the server enforces its own constraints")
    }
    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("the server fires its own triggers")
    }
    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("a transfer writes INSERTs, not SELECTs")
    }
    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("this test stops before anything is sent")
    }
    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("a target is written to, not read from")
    }
    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("a transfer is not a transaction")
    }
}

/// A source canceller that counts, for the same reason.
struct Counted {
    cancels: Arc<AtomicUsize>,
}

#[async_trait]
impl CursorCancel for Counted {
    async fn cancel(&self) -> DbResult<()> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Stop reaches both ends, and this is the check that says so.
///
/// The write half is the half that was missing. A transfer waits on a fetch from
/// the source and then on an INSERT into the target, in turn, so a stop that
/// only cancelled the source would land whenever the source happened to be the
/// one waiting — about half the time, and never on the batch that takes the
/// longest, since a large INSERT is exactly when somebody reaches for Stop.
#[tokio::test]
async fn a_stop_reaches_the_target_as_well_as_the_source() {
    let target = Arc::new(Deaf {
        cancels: AtomicUsize::new(0),
    });
    let source_cancels = Arc::new(AtomicUsize::new(0));
    let moving = Transfer::new(target.clone(), &dbsql::DUCKDB, "t".to_string());
    let stopper = moving.stopper(Arc::new(Counted {
        cancels: source_cancels.clone(),
    }));

    stopper.stop().await.expect("both requests were delivered");

    assert_eq!(
        source_cancels.load(Ordering::SeqCst),
        1,
        "the fetch waiting on the source is stopped"
    );
    assert_eq!(
        target.cancels.load(Ordering::SeqCst),
        1,
        "and so is the INSERT waiting on the target — the half that had nothing"
    );
}
