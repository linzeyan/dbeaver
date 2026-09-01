//! ClickHouse DDL, against hand-fed answers and against a real server.
//!
//! The same arrangement as the SQLite tests: a fake driver pins what this crate
//! does with the rows — which statement it asks for, how the rows are joined,
//! which are dropped — and the live half says that the statement it asks finds
//! anything at all. The live tests read the container the ClickHouse driver's
//! own tests seed, so `make db-up-clickhouse` is what makes them runnable.
//!
//! There are no constants here holding what upstream emits, unlike the
//! PostgreSQL and SQLite files. Nothing is assembled: the whole of this renderer
//! is the statement it sends and the text the server answers with, and the one
//! rule it applies to that text — leaving it alone — is stated as the assertion
//! that `Decimal(9, 4)` survives with its comma intact.

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor, DatabaseInfo, DbResult, Driver,
    IndexInfo, RelationInfo, RelationKind, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo,
    ServerProcesses, TriggerInfo, TxStep, UniqueKeyInfo,
};
use driver_clickhouse::ChSource;
use std::sync::{Arc, Mutex};

const URL: &str = "http://default:test@127.0.0.1:58123/bench";

// ---------------------------------------------------------------------------
// A database that answers from a list
// ---------------------------------------------------------------------------

/// One batch of `SHOW CREATE TABLE` rows, handed over the way the driver would.
struct Rows {
    schema: SchemaRef,
    batch: Option<RecordBatch>,
}

impl Rows {
    fn holding(values: Vec<Option<String>>) -> Self {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new(
            "statement",
            DataType::Utf8,
            true,
        )]));
        let column = Arc::new(StringArray::from(values)) as ArrayRef;
        let batch =
            RecordBatch::try_new(schema.clone(), vec![column]).expect("one column, one field");
        Self {
            schema,
            batch: Some(batch),
        }
    }
}

#[async_trait::async_trait]
impl ResultStream for Rows {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn rows_affected(&self) -> Option<u64> {
        None
    }

    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(self.batch.take())
    }
}

/// A database that answers with one result and remembers what it was asked.
struct Fixture {
    answer: Mutex<Option<Vec<Option<String>>>>,
    asked: Mutex<Vec<String>>,
}

impl Fixture {
    fn answering(rows: &[Option<&str>]) -> Self {
        Self {
            answer: Mutex::new(Some(
                rows.iter().map(|sql| sql.map(str::to_string)).collect(),
            )),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .expect("no test panics holding this")
            .clone()
    }
}

#[async_trait::async_trait]
impl Driver for Fixture {
    async fn server_info(&self) -> DbResult<ServerInfo> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }

    async fn relations(&self, _: &str) -> DbResult<Vec<RelationInfo>> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }

    // Every metadata call below is unreachable, and together they are the point
    // of this renderer: ClickHouse's DDL is the statement the server writes, so
    // reassembling one out of columns and keys would be describing the table
    // twice and agreeing with itself only by luck.
    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        unreachable!("ClickHouse's DDL is the server's own text, not columns put back together")
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        unreachable!("a view's body is inside the statement the server writes")
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("an index is inside the statement the server writes")
    }

    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("ClickHouse has no unique constraint to render")
    }

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("ClickHouse has no foreign keys to render")
    }

    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a table's own DDL does not name what points at it")
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        unreachable!("a constraint is inside the statement the server writes")
    }

    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("ClickHouse has no triggers to render")
    }

    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("DDL is rendered from what the fixture holds, never from a browse")
    }
    async fn query(&self, statement: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        self.asked
            .lock()
            .expect("no test panics holding this")
            .push(statement.to_string());
        let answer = self
            .answer
            .lock()
            .expect("no test panics holding this")
            .take()
            .expect("the renderer asked one more question than the fixture has answers for");
        Ok(Box::new(Rows::holding(answer)))
    }

    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("one statement is one row, read in one go")
    }

    async fn cancel(&self) -> DbResult<()> {
        unreachable!("nothing here is long enough to cancel")
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: false,
            switches_database: false,
            schema_is_the_database: false,
            // A rendered statement is read off the metadata it was
            // handed; this double is never asked for routines.
            reports_routines: false,
            // Nor sequences, for the same reason as the line above.
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
        }
    }

    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("reading a stored statement opens no transaction")
    }
}

fn relation(schema: &str, name: &str, kind: RelationKind) -> RelationInfo {
    RelationInfo {
        schema: schema.to_string(),
        name: name.to_string(),
        kind,
        estimated_rows: None,
    }
}

async fn rendered(driver: &dyn Driver, relation: &RelationInfo) -> String {
    dbddl::definition(driver, &dbsql::CLICKHOUSE, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The one question this renderer asks, in the form upstream asks it.
///
/// `SHOW CREATE TABLE` for a view as well as for a table, because that is the
/// statement ClickHouse answers with for both and `getViewDDL` is one line
/// calling `getTableDDL`.
#[tokio::test]
async fn a_view_is_asked_for_the_way_a_table_is() {
    let fixture = Fixture::answering(&[Some("CREATE VIEW bench.open AS SELECT 1")]);
    rendered(&fixture, &relation("bench", "open", RelationKind::View)).await;
    assert_eq!(fixture.asked(), ["SHOW CREATE TABLE bench.open"]);
}

/// A statement long enough to arrive in pieces is put back in order.
///
/// Upstream accumulates the rows with a newline after each, which is the only
/// joining rule it has, and a server that split a statement across rows would
/// otherwise have it rendered as a single unreadable line.
#[tokio::test]
async fn rows_are_joined_in_the_order_they_arrive() {
    let fixture = Fixture::answering(&[
        Some("CREATE TABLE bench.parts"),
        Some("("),
        Some("    `id` UInt64"),
        Some(")"),
        Some("ENGINE = MergeTree"),
    ]);
    let ddl = rendered(&fixture, &relation("bench", "parts", RelationKind::Table)).await;
    assert_eq!(
        ddl,
        "CREATE TABLE bench.parts\n(\n    `id` UInt64\n)\nENGINE = MergeTree"
    );
}

/// A NULL row is dropped rather than rendered as an empty line.
///
/// `getTableDDL` skips a null with `if (line == null) continue`, and a renderer
/// that appended it would leave a gap in the middle of a statement.
#[tokio::test]
async fn a_null_row_leaves_no_blank_line() {
    let fixture = Fixture::answering(&[Some("CREATE TABLE bench.parts"), None, Some(")")]);
    let ddl = rendered(&fixture, &relation("bench", "parts", RelationKind::Table)).await;
    assert_eq!(ddl, "CREATE TABLE bench.parts\n)");
}

/// A relation the server will not describe is refused rather than left blank.
///
/// Upstream shows the empty string it accumulated. The refusal names the object,
/// because the case it covers is a navigator holding a relation that has since
/// been dropped.
#[tokio::test]
async fn a_relation_the_server_will_not_describe_is_refused() {
    let fixture = Fixture::answering(&[]);
    let error = dbddl::definition(
        &fixture,
        &dbsql::CLICKHOUSE,
        &relation("bench", "vanished", RelationKind::Table),
    )
    .await
    .expect_err("a relation with no statement rendered as something");
    assert!(
        error.to_string().contains("vanished"),
        "the refusal does not say which object it is about: {error}"
    );
}

// ---------------------------------------------------------------------------
// Against a server
// ---------------------------------------------------------------------------

async fn live(name: &str) -> (ChSource, RelationInfo) {
    let source = ChSource::connect(URL)
        .await
        .expect("ClickHouse unreachable; run 'make db-up-clickhouse'");
    let relation = source
        .relations("bench")
        .await
        .expect("listing the fixture database")
        .into_iter()
        .find(|listed| listed.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the fixture database"));
    (source, relation)
}

/// A table, as the server writes it — including the comma this does not touch.
///
/// `Decimal(9, 4)` is the assertion that matters. Upstream's `normalizeDDL`
/// breaks the line after every comma in the column list, which on a server that
/// pretty-prints its own output splits that type across two lines; this renderer
/// leaves the text alone, and that is what the type spelling here checks.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_table_is_the_statement_the_server_writes() {
    let (source, relation) = live("types_all").await;
    let ddl = rendered(&source, &relation).await;
    assert!(ddl.starts_with("CREATE TABLE bench.types_all"), "{ddl}");
    assert!(ddl.contains("`d32` Decimal(9, 4)"), "{ddl}");
}

/// A view, through the same call as a table.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_view_is_the_statement_the_server_writes() {
    let (source, relation) = live("plain_view").await;
    assert_eq!(relation.kind, RelationKind::View);
    let ddl = rendered(&source, &relation).await;
    assert!(ddl.starts_with("CREATE VIEW bench.plain_view"), "{ddl}");
}

/// And a materialized view, which is the kind `SHOW CREATE TABLE` is named for.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_materialized_view_keeps_the_table_it_writes_into() {
    let (source, relation) = live("mat_view").await;
    let ddl = rendered(&source, &relation).await;
    assert!(
        ddl.starts_with("CREATE MATERIALIZED VIEW bench.mat_view TO bench.mv_target"),
        "{ddl}"
    );
}
