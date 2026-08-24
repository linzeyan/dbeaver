//! DuckDB DDL, against hand-fed answers and against a real database file.
//!
//! Nothing here is `#[ignore]`d: DuckDB is a library in this process, so the
//! fixture is a file in a temporary directory and the whole file runs under
//! plain `cargo test`. It is built with the `duckdb` crate directly, so that the
//! fixture does not depend on the code under test being right.
//!
//! The two halves prove different things. The fake driver pins the question this
//! crate asks — a wrong filter would still find a statement on a database with
//! one table in it — and the live half pins that the question finds the row, and
//! that what DuckDB stores is what ends up in the pane.

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor, DatabaseInfo, DbResult, Driver,
    IndexInfo, RelationInfo, RelationKind, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo,
    TriggerInfo, TxStep, UniqueKeyInfo,
};
use driver_duckdb::DuckSource;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// A database that answers from a list
// ---------------------------------------------------------------------------

struct Rows {
    schema: SchemaRef,
    batch: Option<RecordBatch>,
}

impl Rows {
    fn holding(values: Vec<Option<String>>) -> Self {
        let schema: SchemaRef =
            Arc::new(Schema::new(vec![Field::new("sql", DataType::Utf8, true)]));
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

/// A database that answers one query and remembers what it was asked.
#[derive(Default)]
struct Fixture {
    answer: Mutex<Option<Vec<Option<String>>>>,
    asked: Mutex<Vec<String>>,
    definition: Option<String>,
}

impl Fixture {
    fn answering(rows: &[Option<&str>]) -> Self {
        Self {
            answer: Mutex::new(Some(
                rows.iter().map(|sql| sql.map(str::to_string)).collect(),
            )),
            ..Self::default()
        }
    }

    fn defining(statement: &str) -> Self {
        Self {
            definition: Some(statement.to_string()),
            ..Self::default()
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
    // of this renderer: DuckDB's DDL is the statement the database kept, so
    // reassembling one out of columns and keys would be describing the table
    // twice and agreeing with itself only by luck.
    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        unreachable!("DuckDB's DDL is stored text, not columns put back together")
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        Ok(self.definition.clone())
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("upstream leaves indexes out of a DuckDB table's DDL")
    }

    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("a unique key reaches the script through constraints()")
    }

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a foreign key is already inside the stored CREATE TABLE")
    }

    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a table's own DDL does not name what points at it")
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        unreachable!("a constraint is already inside the stored CREATE TABLE")
    }

    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("DuckDB has no triggers to render")
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
    dbddl::definition(driver, &dbsql::DUCKDB, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The question upstream asks, with both of this driver's levels in it.
///
/// The filter is what this test is about. A database with one table answers any
/// filter at all, so the live half below cannot tell a right one from a wrong
/// one; this can. The database half is `current_database()` rather than a name:
/// `main` is a schema in every attached database, and a lookup that dropped the
/// level would answer with whichever one the catalog listed first.
#[tokio::test]
async fn a_table_is_asked_for_by_database_schema_and_name() {
    let fixture = Fixture::answering(&[Some("CREATE TABLE parts(id INTEGER);")]);
    rendered(&fixture, &relation("main", "parts", RelationKind::Table)).await;
    assert_eq!(
        fixture.asked(),
        ["SELECT sql FROM duckdb_tables() \
          WHERE database_name = current_database() AND schema_name = 'main' \
          AND table_name = 'parts'"]
    );
}

/// A view sends no query at all: its statement is already a metadata call.
#[tokio::test]
async fn a_view_is_read_through_the_metadata_call_that_already_has_it() {
    let fixture = Fixture::defining("CREATE VIEW open_parts AS SELECT id FROM parts;");
    let ddl = rendered(
        &fixture,
        &relation("main", "open_parts", RelationKind::View),
    )
    .await;
    assert_eq!(ddl, "CREATE VIEW open_parts AS SELECT id FROM parts;");
    assert!(
        fixture.asked().is_empty(),
        "a view's DDL should not send a query of its own"
    );
}

/// A table the catalog has no statement for is refused, not left blank.
#[tokio::test]
async fn a_table_with_no_stored_statement_is_refused() {
    let fixture = Fixture::answering(&[None]);
    let error = dbddl::definition(
        &fixture,
        &dbsql::DUCKDB,
        &relation("main", "vanished", RelationKind::Table),
    )
    .await
    .expect_err("a table with no statement rendered as something");
    assert!(
        error.to_string().contains("vanished"),
        "the refusal does not say which object it is about: {error}"
    );
}

/// And so is a view, where upstream prints a comment saying it found nothing.
#[tokio::test]
async fn a_view_with_no_stored_statement_is_refused() {
    let error = dbddl::definition(
        &Fixture::default(),
        &dbsql::DUCKDB,
        &relation("main", "vanished", RelationKind::View),
    )
    .await
    .expect_err("a view with no statement rendered as something");
    assert!(error.to_string().contains("vanished"), "{error}");
}

/// A kind DuckDB does not have is refused before anything is asked.
#[tokio::test]
async fn a_kind_duckdb_does_not_have_is_refused() {
    let fixture = Fixture::default();
    let error = dbddl::definition(
        &fixture,
        &dbsql::DUCKDB,
        &relation("main", "rollup", RelationKind::MaterializedView),
    )
    .await
    .expect_err("a materialized view rendered as something");
    assert!(error.to_string().contains("MaterializedView"), "{error}");
    assert!(fixture.asked().is_empty());
}

// ---------------------------------------------------------------------------
// Against a database file
// ---------------------------------------------------------------------------

/// A database file that lives as long as the test does.
struct File {
    _dir: TempDir,
    path: PathBuf,
}

impl File {
    fn holding(setup: &str) -> Self {
        let dir = tempfile::tempdir().expect("no temporary directory");
        let path = dir.path().join("fixture.duckdb");
        {
            let conn = duckdb::Connection::open(&path).expect("could not create the fixture");
            conn.execute_batch(setup).expect("fixture setup failed");
        }
        Self { _dir: dir, path }
    }

    async fn connect(&self) -> DuckSource {
        DuckSource::connect(self.path.to_str().unwrap())
            .await
            .expect("fixture database unreachable")
    }
}

const SETUP: &str = "CREATE TABLE parts (
    id INTEGER PRIMARY KEY,
    sku TEXT NOT NULL,
    qty INTEGER DEFAULT 1
);
CREATE INDEX parts_qty ON parts (qty);
CREATE VIEW open_parts AS SELECT id, sku FROM parts WHERE qty > 0;";

async fn listed(source: &DuckSource, name: &str) -> RelationInfo {
    source
        .relations("main")
        .await
        .expect("listing the fixture database")
        .into_iter()
        .find(|relation| relation.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the fixture database"))
}

/// A table is the statement DuckDB stored, and only that.
///
/// `parts_qty` is in the file and not in the output, which is the assertion
/// about `DuckMetaModel.getTableDDL`: it reads one row from `duckdb_tables()`
/// and stops, where the shared path PostgreSQL takes would append every index.
#[tokio::test]
async fn a_table_is_the_statement_duckdb_stored() {
    let file = File::holding(SETUP);
    let source = file.connect().await;
    let ddl = rendered(&source, &listed(&source, "parts").await).await;

    assert!(ddl.starts_with("CREATE TABLE parts("), "{ddl}");
    assert!(ddl.contains("sku VARCHAR NOT NULL"), "{ddl}");
    assert!(!ddl.contains("parts_qty"), "{ddl}");
}

/// A view is the statement DuckDB stored for it.
#[tokio::test]
async fn a_view_is_the_statement_duckdb_stored() {
    let file = File::holding(SETUP);
    let source = file.connect().await;
    let relation = listed(&source, "open_parts").await;
    assert_eq!(relation.kind, RelationKind::View);

    let ddl = rendered(&source, &relation).await;
    assert!(ddl.starts_with("CREATE VIEW open_parts AS"), "{ddl}");
    assert!(ddl.contains("qty > 0"), "{ddl}");
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

/// The statement written for a file's columns is one DuckDB runs.
///
/// The golden strings in the crate's own tests say what each database is *told*;
/// only the database says whether it understood. DuckDB is a library in this
/// process, so this one can be asked for real, and what it is asked for is the
/// column list read back out of the table it made.
#[tokio::test]
async fn a_table_made_for_a_files_columns_is_one_duckdb_runs() {
    let statement = dbddl::create_table(&dbsql::DUCKDB, "landed", &a_files_columns())
        .expect("DuckDB would not write a table for a file's columns");
    let file = File::holding(&statement);
    let source = file.connect().await;
    let columns: Vec<(String, String)> = source
        .columns("main", "landed")
        .await
        .expect("listing the new table's columns failed")
        .into_iter()
        .map(|column| (column.name, column.data_type))
        .collect();
    assert_eq!(
        columns,
        vec![
            ("id".to_string(), "BIGINT".to_string()),
            ("Order Date".to_string(), "DATE".to_string()),
            ("amount".to_string(), "DECIMAL(12,2)".to_string()),
            ("ratio".to_string(), "DOUBLE".to_string()),
            ("paid".to_string(), "BOOLEAN".to_string()),
            ("note".to_string(), "VARCHAR".to_string()),
            ("seen_at".to_string(), "TIMESTAMP".to_string()),
        ]
    );
}
