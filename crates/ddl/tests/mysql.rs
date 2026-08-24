//! MySQL DDL, against hand-fed answers and against a live server.
//!
//! The same two halves as the SQLite and ClickHouse files. The fake driver
//! answers with the columns `SHOW CREATE …` really has — the object's name
//! first, then its statement — because reading the right one out of them is most
//! of what this renderer does, and a fake with a single column would agree with
//! a renderer that counted from the left.
//!
//! The live half seeds a database of its own rather than reading the driver's,
//! so the two test suites cannot break each other, and seeds it through
//! `mysql_async` rather than through the driver, for the reason the driver's own
//! tests give: a fixture built by the code under test proves nothing about it.

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor, DatabaseInfo, DbResult, Driver,
    IndexInfo, RelationInfo, RelationKind, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo,
    TriggerInfo, TxStep, UniqueKeyInfo,
};
use driver_mysql::MySqlSource;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// The fixture lives in a database of its own, named for what made it.
const URL: &str = "mysql://root:test@127.0.0.1:53306/dbclient_ddl";
const ROOT_URL: &str = "mysql://root:test@127.0.0.1:53306/";

// ---------------------------------------------------------------------------
// What the database was told
// ---------------------------------------------------------------------------

const CREATE_PARTS: &str = "CREATE TABLE parts (
    id INT NOT NULL AUTO_INCREMENT,
    sku VARCHAR(32) NOT NULL,
    qty INT NOT NULL DEFAULT 1,
    PRIMARY KEY (id),
    UNIQUE KEY parts_sku_key (sku)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

const CREATE_OPEN_PARTS: &str = "CREATE VIEW open_parts AS SELECT id, sku FROM parts WHERE qty > 0";

// ---------------------------------------------------------------------------
// A database that answers from a list
// ---------------------------------------------------------------------------

/// One row of a `SHOW CREATE …` result, with the columns the server sends.
struct Rows {
    schema: SchemaRef,
    batch: Option<RecordBatch>,
}

impl Rows {
    fn holding(columns: &[(&str, Option<&str>)]) -> Self {
        let fields: Vec<Field> = columns
            .iter()
            .map(|(name, _)| Field::new(*name, DataType::Utf8, true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let values: Vec<ArrayRef> = columns
            .iter()
            .map(|(_, value)| Arc::new(StringArray::from(vec![*value])) as ArrayRef)
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), values).expect("one row per column");
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

/// One `SHOW CREATE …` result: every column the server sends, in order, holding
/// the value in its single row.
type Answer = Vec<(String, Option<String>)>;

/// A database that answers with one result and remembers what it was asked.
struct Fixture {
    answer: Mutex<Option<Answer>>,
    asked: Mutex<Vec<String>>,
}

impl Fixture {
    fn answering(columns: &[(&str, Option<&str>)]) -> Self {
        Self {
            answer: Mutex::new(Some(
                columns
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.map(str::to_string)))
                    .collect(),
            )),
            asked: Mutex::new(Vec::new()),
        }
    }

    /// A result with no rows at all, which is what a dropped relation looks like.
    fn empty() -> Self {
        Self {
            answer: Mutex::new(None),
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
    // of this renderer: MySQL writes the statement itself, so reassembling one
    // out of columns and keys would be describing the table twice and agreeing
    // with itself only by luck.
    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        unreachable!("MySQL's DDL is the server's own text, not columns put back together")
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        unreachable!("a view's body arrives inside SHOW CREATE VIEW")
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("an index is inside the CREATE TABLE the server writes")
    }

    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("a unique key is inside the CREATE TABLE the server writes")
    }

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a foreign key is inside the CREATE TABLE the server writes")
    }

    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a table's own DDL does not name what points at it")
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        unreachable!("a constraint is inside the CREATE TABLE the server writes")
    }

    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("upstream leaves triggers out of a MySQL table's DDL")
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
            .take();
        match answer {
            Some(columns) => {
                let borrowed: Vec<(&str, Option<&str>)> = columns
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_deref()))
                    .collect();
                Ok(Box::new(Rows::holding(&borrowed)))
            }
            None => Ok(Box::new(Empty)),
        }
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
        }
    }

    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("reading a stored statement opens no transaction")
    }
}

/// A result that ended before its first row.
struct Empty;

#[async_trait::async_trait]
impl ResultStream for Empty {
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::empty())
    }

    fn rows_affected(&self) -> Option<u64> {
        None
    }

    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(None)
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
    dbddl::definition(driver, &dbsql::MYSQL, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A table is the statement the server wrote, and the question is upstream's.
#[tokio::test]
async fn a_table_is_the_statement_the_server_wrote() {
    let fixture = Fixture::answering(&[
        ("Table", Some("parts")),
        ("Create Table", Some(CREATE_PARTS)),
    ]);
    let ddl = rendered(&fixture, &relation("shop", "parts", RelationKind::Table)).await;
    assert_eq!(ddl, CREATE_PARTS);
    assert_eq!(fixture.asked(), ["SHOW CREATE TABLE shop.parts"]);
}

/// A partitioned table is a table, because upstream's only test is `isView()`.
#[tokio::test]
async fn a_partitioned_table_is_asked_for_as_a_table() {
    let fixture = Fixture::answering(&[
        ("Table", Some("parts")),
        ("Create Table", Some(CREATE_PARTS)),
    ]);
    rendered(
        &fixture,
        &relation("shop", "parts", RelationKind::PartitionedTable),
    )
    .await;
    assert_eq!(fixture.asked(), ["SHOW CREATE TABLE shop.parts"]);
}

/// The statement is taken by name, not by position.
///
/// `SHOW CREATE VIEW` answers with four columns and the view's own name is the
/// first of them, so a renderer that read column zero would print `open_parts`
/// as the DDL of `open_parts`.
#[tokio::test]
async fn a_view_is_read_out_of_the_column_that_holds_it() {
    let fixture = Fixture::answering(&[
        ("View", Some("open_parts")),
        (
            "Create View",
            Some(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER \
                 VIEW `shop`.`open_parts` AS select `id` from `parts`",
            ),
        ),
        ("character_set_client", Some("utf8mb4")),
        ("collation_connection", Some("utf8mb4_0900_ai_ci")),
    ]);
    let ddl = rendered(
        &fixture,
        &relation("shop", "open_parts", RelationKind::View),
    )
    .await;
    assert_eq!(
        ddl,
        "CREATE OR REPLACE ALGORITHM=UNDEFINED VIEW `shop`.`open_parts` AS select `id` from `parts`"
    );
    assert_eq!(fixture.asked(), ["SHOW CREATE VIEW shop.open_parts"]);
}

/// A head with no algorithm loses the same clauses and gains nothing.
///
/// Upstream's regular expression matches nothing here and `params` stays empty,
/// which is the branch that would otherwise write `ALGORITHM=` with no value
/// after it.
#[tokio::test]
async fn a_view_with_no_algorithm_is_rewritten_without_one() {
    let fixture = Fixture::answering(&[
        ("View", Some("open_parts")),
        (
            "Create View",
            Some("CREATE DEFINER=`root`@`%` VIEW `shop`.`open_parts` AS select 1"),
        ),
    ]);
    let ddl = rendered(
        &fixture,
        &relation("shop", "open_parts", RelationKind::View),
    )
    .await;
    assert_eq!(
        ddl,
        "CREATE OR REPLACE VIEW `shop`.`open_parts` AS select 1"
    );
}

/// A statement with nothing to cut at is passed through as it arrived.
#[tokio::test]
async fn a_view_whose_head_cannot_be_found_is_left_alone() {
    let fixture = Fixture::answering(&[
        ("View", Some("open_parts")),
        ("Create View", Some("CREATE VIEW open_parts AS select 1")),
    ]);
    let ddl = rendered(
        &fixture,
        &relation("shop", "open_parts", RelationKind::View),
    )
    .await;
    assert_eq!(ddl, "CREATE VIEW open_parts AS select 1");
}

/// A relation the server will not describe is refused rather than left blank.
#[tokio::test]
async fn a_relation_the_server_will_not_describe_is_refused() {
    let fixture = Fixture::empty();
    let error = dbddl::definition(
        &fixture,
        &dbsql::MYSQL,
        &relation("shop", "vanished", RelationKind::Table),
    )
    .await
    .expect_err("a relation with no statement rendered as something");
    assert!(
        error.to_string().contains("vanished"),
        "the refusal does not say which object it is about: {error}"
    );
}

/// A kind MySQL does not have is refused before a statement is sent.
#[tokio::test]
async fn a_kind_mysql_does_not_have_is_refused() {
    let fixture = Fixture::empty();
    let error = dbddl::definition(
        &fixture,
        &dbsql::MYSQL,
        &relation("shop", "rollup", RelationKind::MaterializedView),
    )
    .await
    .expect_err("a materialized view rendered as something");
    assert!(
        error.to_string().contains("MaterializedView"),
        "the refusal does not say what it was handed: {error}"
    );
    assert!(
        fixture.asked().is_empty(),
        "a kind MySQL does not have should not reach the server"
    );
}

// ---------------------------------------------------------------------------
// Against a server
// ---------------------------------------------------------------------------

static FIXTURE: OnceCell<()> = OnceCell::const_new();

async fn live() -> MySqlSource {
    FIXTURE.get_or_init(seed).await;
    MySqlSource::connect(URL)
        .await
        .expect("MySQL unreachable; run 'make db-up-mysql'")
}

async fn seed() {
    let opts = Opts::from_url(ROOT_URL).expect("the fixture URL should parse");
    let mut conn = Conn::new(opts)
        .await
        .expect("MySQL unreachable; run 'make db-up-mysql'");
    for statement in [
        "DROP DATABASE IF EXISTS dbclient_ddl",
        "CREATE DATABASE dbclient_ddl",
        "USE dbclient_ddl",
        CREATE_PARTS,
        CREATE_OPEN_PARTS,
    ] {
        conn.query_drop(statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }
    conn.disconnect()
        .await
        .expect("closing the seed connection");
}

async fn listed(source: &MySqlSource, name: &str) -> RelationInfo {
    source
        .relations("dbclient_ddl")
        .await
        .expect("listing the fixture database")
        .into_iter()
        .find(|relation| relation.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the fixture database"))
}

/// The rendered table is the server's own text, character for character.
///
/// Read a second time through `mysql_async` rather than compared against a
/// constant: what this proves is that the renderer picked the right column and
/// changed nothing in it, and a constant would also be asserting which storage
/// engine and collation this particular server defaults to.
#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_table_renders_as_the_server_spells_it() {
    let source = live().await;
    let relation = listed(&source, "parts").await;
    let ddl = rendered(&source, &relation).await;

    let mut conn = Conn::new(Opts::from_url(URL).unwrap())
        .await
        .expect("a connection of the test's own");
    let (_, expected): (String, String) = conn
        .query_first("SHOW CREATE TABLE dbclient_ddl.parts")
        .await
        .expect("asking the server directly")
        .expect("a row");
    conn.disconnect().await.expect("closing");

    assert_eq!(ddl, expected);
}

/// The rendered view has lost the account that created it.
///
/// The `DEFINER` clause is the reason upstream rewrites this head at all: it
/// names a user that exists on the server the view came from, so a DDL carrying
/// it is one that cannot be replayed anywhere else.
#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_view_renders_without_the_definer_it_was_made_with() {
    let source = live().await;
    let relation = listed(&source, "open_parts").await;
    assert_eq!(relation.kind, RelationKind::View);
    let ddl = rendered(&source, &relation).await;

    assert!(
        ddl.starts_with("CREATE OR REPLACE ALGORITHM=UNDEFINED VIEW `open_parts`"),
        "{ddl}"
    );
    assert!(!ddl.contains("DEFINER"), "{ddl}");
    assert!(ddl.contains("from `parts`"), "{ddl}");
}

// ---------------------------------------------------------------------------
// A table made for a file
// ---------------------------------------------------------------------------

/// The seven kinds a file can ask for, and a name that has to be quoted.
fn a_files_columns() -> arrow::datatypes::Schema {
    use arrow::datatypes::{DataType, Field, TimeUnit};
    arrow::datatypes::Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("Order Date", DataType::Date32, true),
        Field::new("amount", DataType::Decimal128(12, 2), true),
        Field::new("ratio", DataType::Float64, true),
        Field::new("paid", DataType::Boolean, true),
        Field::new("note", DataType::Utf8, true),
        Field::new(
            "seen_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ])
}

/// A statement run for its effect, drained so that it has finished before the
/// next one starts.
async fn run(source: &impl dbconn::Driver, statement: &str) {
    let mut stream = source
        .query(statement, 1)
        .await
        .unwrap_or_else(|e| panic!("the server refused this:\n{statement}\n{e}"));
    while stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("reading back from {statement}: {e}"))
        .is_some()
    {}
}

/// The statement written for a file's columns is one MySQL runs.
///
/// As with PostgreSQL, what is checked is the column list read back out of the
/// table that was made. `DATETIME` is the word this cares about most: `TIMESTAMP`
/// would also have been accepted here and would have held nothing before 1970.
#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_table_made_for_a_files_columns_is_one_mysql_runs() {
    let source = live().await;
    let statement = dbddl::create_table(
        &dbsql::MYSQL,
        "dbclient_ddl.ddl_from_a_file",
        &a_files_columns(),
    )
    .expect("MySQL would not write a table for a file's columns");
    run(&source, "DROP TABLE IF EXISTS dbclient_ddl.ddl_from_a_file").await;
    run(&source, &statement).await;

    let columns: Vec<(String, String)> = source
        .columns("dbclient_ddl", "ddl_from_a_file")
        .await
        .expect("listing the new table's columns failed")
        .into_iter()
        .map(|column| (column.name, column.data_type))
        .collect();
    run(&source, "DROP TABLE dbclient_ddl.ddl_from_a_file").await;
    assert_eq!(
        columns,
        vec![
            ("id".to_string(), "bigint".to_string()),
            ("Order Date".to_string(), "date".to_string()),
            ("amount".to_string(), "decimal(12,2)".to_string()),
            ("ratio".to_string(), "double".to_string()),
            ("paid".to_string(), "tinyint(1)".to_string()),
            ("note".to_string(), "text".to_string()),
            ("seen_at".to_string(), "datetime".to_string()),
        ]
    );
}
