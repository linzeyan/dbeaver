//! SQLite DDL, against hand-fed answers and against a real database file.
//!
//! The same arrangement as the PostgreSQL tests and for the same reason: the
//! constants below are what upstream emits, established by reading
//! `SQLiteMetaModel.getTableDDL` and `SQLiteUtils.readMasterDefinition`, not by
//! running this and writing down what came out. What differs is that the live
//! half is not `#[ignore]` — SQLite is a file, so the fixture is built in a
//! temporary directory and the whole file runs under plain `cargo test`.
//!
//! The two halves still prove different things. A fake driver answers whatever it
//! is handed, so those tests pin the assembling: which statement comes first, how
//! many newlines separate them, which rows are dropped on the way. Only the live
//! tests can say that the query this crate sends finds those rows and no others,
//! which is also why the fake records what it was asked.
//!
//! Both halves are fed the same statement text, from the constants in the next
//! section, so a fixture edited on one side cannot quietly stop describing the
//! other.

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor, DatabaseInfo, DbResult, Driver,
    IndexInfo, RelationInfo, RelationKind, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo,
    ServerProcesses, TriggerInfo, TxStep, UniqueKeyInfo,
};
use driver_sqlite::SqliteSource;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// What the database was told
// ---------------------------------------------------------------------------

/// The table with one of everything on it.
///
/// Laid out over several lines on purpose. SQLite keeps the text it was given, so
/// this indentation is what a reader of the DDL tab sees, and a renderer that
/// reflowed anything would show up as a difference here.
const CREATE_PARTS: &str = "CREATE TABLE parts (
    id INTEGER PRIMARY KEY,
    sku TEXT NOT NULL UNIQUE,
    qty INTEGER NOT NULL DEFAULT 1 CHECK (qty > 0),
    email TEXT,
    shipment_id INTEGER REFERENCES shipments (id),
    shipped_at TEXT
)";
const PARTS_EMAIL_LOWER: &str = "CREATE INDEX parts_email_lower ON parts (lower(email))";
const PARTS_SKU_KEY: &str = "CREATE UNIQUE INDEX parts_sku_key ON parts (sku)";
const PARTS_PENDING: &str = "CREATE INDEX parts_pending ON parts (id) WHERE shipped_at IS NULL";

/// A trigger on `parts`, which exists to be left out.
///
/// `SQLiteMetaModel.getTableDDL` asks `readMasterDefinition` for the table and
/// for its indexes and stops. Triggers are reachable only through
/// `getTriggerDDL`, for the navigator's own trigger node, so anything that
/// appended them to a table's DDL — as PostgreSQL's renderer must — would be
/// adding a section upstream does not have.
const PARTS_TOUCH: &str = "CREATE TRIGGER parts_touch AFTER UPDATE ON parts BEGIN
    UPDATE shipments SET shipped_on = NULL WHERE id = NEW.shipment_id;
END";

/// A second table, with an index of its own, which also exists to be left out.
///
/// The failure it catches is a query that filters on the object type and forgets
/// `tbl_name`: `shipments_shipped_on` would then arrive in every table's DDL.
const CREATE_SHIPMENTS: &str = "CREATE TABLE shipments (
    id INTEGER PRIMARY KEY,
    shipped_on TEXT
)";
const SHIPMENTS_SHIPPED_ON: &str = "CREATE INDEX shipments_shipped_on ON shipments (shipped_on)";

/// A table whose only index is the one SQLite built for the `UNIQUE` clause.
///
/// That index is a `sqlite_master` row with a NULL `sql`, so there is an index
/// row and no index text — the case that decides whether the blank line before
/// the index section is written from the rows or from what they rendered into.
const CREATE_BARE: &str = "CREATE TABLE bare (n INTEGER UNIQUE)";

const CREATE_OPEN_PARTS: &str = "CREATE VIEW open_parts AS
SELECT id, sku FROM parts WHERE shipped_at IS NULL";

/// A virtual table, which upstream renders as a table without knowing it is one:
/// `sqlite_master` files it under `type='table'` like any other.
const CREATE_DOCS: &str = "CREATE VIRTUAL TABLE docs USING fts5(title, body)";

// ---------------------------------------------------------------------------
// What upstream emits
// ---------------------------------------------------------------------------

/// `main.parts`, from `SQLiteMetaModel.getTableDDL`.
///
/// The statement the table was created from, a blank line, then every index on it
/// in the order they were created — `tableDDL + "\n" + indexesDDL`, where
/// `readMasterDefinition` has already put a semicolon and a newline after each
/// statement it collected.
///
/// Three things this does not contain, each of them an assertion. No
/// commented-out `DROP TABLE`, because that header comes from
/// `SQLTableManager.getTableDDL` and `SQLiteMetaModel` overrides that method
/// away. No `parts_touch`, because a table's DDL never reaches `getTriggerDDL`.
/// And no `sqlite_autoindex_parts_1`, the index behind `sku TEXT NOT NULL
/// UNIQUE`, because its row has no statement and `readMasterDefinition` skips a
/// NULL one.
///
/// No deliberate differences from upstream anywhere in this file. There is
/// nothing here for this crate to decide: every line is text SQLite handed back,
/// and the only choices are which rows to ask for and how to join them.
const PARTS: &str = "CREATE TABLE parts (
    id INTEGER PRIMARY KEY,
    sku TEXT NOT NULL UNIQUE,
    qty INTEGER NOT NULL DEFAULT 1 CHECK (qty > 0),
    email TEXT,
    shipment_id INTEGER REFERENCES shipments (id),
    shipped_at TEXT
);

CREATE INDEX parts_email_lower ON parts (lower(email));
CREATE UNIQUE INDEX parts_sku_key ON parts (sku);
CREATE INDEX parts_pending ON parts (id) WHERE shipped_at IS NULL;";

/// `main.bare`, which ends where its own statement ends.
///
/// `getTableDDL` returns the table's text alone when the index text is empty, so
/// there is no trailing blank line and nothing after it.
const BARE: &str = "CREATE TABLE bare (n INTEGER UNIQUE);";

/// `main.open_parts`, from `SQLiteMetaModel.getViewDDL`.
///
/// One `readMasterDefinition` call and nothing around it. Where PostgreSQL's
/// renderer writes `CREATE OR REPLACE VIEW` and wraps a body, this is the
/// `CREATE VIEW` the user typed, terminated.
const OPEN_PARTS: &str = "CREATE VIEW open_parts AS
SELECT id, sku FROM parts WHERE shipped_at IS NULL;";

/// `main.docs`, a virtual table rendered by the same path as a table.
const DOCS: &str = "CREATE VIRTUAL TABLE docs USING fts5(title, body);";

// ---------------------------------------------------------------------------
// A database that answers from a list
// ---------------------------------------------------------------------------

/// One batch of `sqlite_master.sql` values, handed over the way a driver would.
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

/// A database that answers queries from a list and remembers what it was asked.
///
/// The answers are consumed in order rather than matched against the statement,
/// because the order is itself part of what upstream specifies — the table's own
/// statement, then its indexes' — and a renderer that asked the other way round
/// would print them the other way round. What the statements actually said is
/// checked separately, from `asked`.
#[derive(Default)]
struct Fixture {
    answers: Mutex<VecDeque<Vec<Option<String>>>>,
    asked: Mutex<Vec<String>>,
    definition: Option<String>,
}

impl Fixture {
    fn answering(answers: &[&[Option<&str>]]) -> Self {
        Self {
            answers: Mutex::new(
                answers
                    .iter()
                    .map(|rows| rows.iter().map(|sql| sql.map(str::to_string)).collect())
                    .collect(),
            ),
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
    // of this renderer: SQLite's DDL is the statement the database kept, so
    // reassembling one out of columns and keys would be describing the table
    // twice and agreeing with itself only by luck.
    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        unreachable!("SQLite's DDL is stored text, not columns put back together")
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        Ok(self.definition.clone())
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("an index reaches the script as the statement that made it")
    }

    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("a unique key is inside the CREATE TABLE SQLite kept")
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
        unreachable!("upstream leaves triggers out of a SQLite table's DDL")
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
            .answers
            .lock()
            .expect("no test panics holding this")
            .pop_front()
            .expect("the renderer asked one more question than the fixture has answers for");
        Ok(Box::new(Rows::holding(answer)))
    }

    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("a schema lookup is a handful of rows, read in one go")
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
            reports_variables: false,
            // DDL is rendered from metadata; this double is never asked to
            // write a row.
            writes_rows: false,
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
    dbddl::definition(driver, &dbsql::SQLITE, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The whole of a table, from answers that never touched a database.
///
/// One assertion over the whole string, because everything this renderer can get
/// wrong is arrangement: whether the blank line is there, whether each statement
/// is terminated, whether the index with no statement of its own left a gap where
/// it was skipped.
#[tokio::test]
async fn a_table_is_its_own_statement_and_then_its_indexes() {
    let fixture = Fixture::answering(&[
        &[Some(CREATE_PARTS)],
        // The NULL is `sqlite_autoindex_parts_1`, which SQLite lists first
        // because it made it while the table was being created.
        &[
            None,
            Some(PARTS_EMAIL_LOWER),
            Some(PARTS_SKU_KEY),
            Some(PARTS_PENDING),
        ],
    ]);
    let ddl = rendered(&fixture, &relation("main", "parts", RelationKind::Table)).await;
    assert_eq!(ddl, PARTS);
}

/// A table whose every index row is one SQLite wrote for itself stops at the
/// table.
///
/// The naive reading of `getTableDDL` tests the rows for emptiness rather than
/// the text they produced, and this is the input that tells the two apart: one
/// index row, no index text. It would leave a blank line and a truncated script
/// behind it.
#[tokio::test]
async fn an_index_sqlite_built_for_itself_leaves_nothing_behind_it() {
    let fixture = Fixture::answering(&[&[Some(CREATE_BARE)], &[None]]);
    let ddl = rendered(&fixture, &relation("main", "bare", RelationKind::Table)).await;
    assert_eq!(ddl, BARE);
}

/// The rows are asked for the way `readMasterDefinition` asks for them.
///
/// The fake cannot check this by answering, so it checks it by remembering: the
/// table query names the object type, the table and the object; the index query
/// names the type and the table and deliberately not a name, because upstream
/// passes null there to collect every index at once.
#[tokio::test]
async fn the_rows_are_asked_for_the_way_upstream_asks_for_them() {
    let fixture = Fixture::answering(&[&[Some(CREATE_PARTS)], &[Some(PARTS_SKU_KEY)]]);
    rendered(&fixture, &relation("main", "parts", RelationKind::Table)).await;
    assert_eq!(
        fixture.asked(),
        [
            "SELECT sql FROM main.sqlite_master \
             WHERE type = 'table' AND tbl_name = 'parts' AND name = 'parts'",
            "SELECT sql FROM main.sqlite_master WHERE type = 'index' AND tbl_name = 'parts'",
        ]
    );
}

/// A name holding a quote stays inside the literal it was written into.
///
/// Upstream binds every one of these as a parameter and `Driver::query` has no
/// parameters, so the name is pasted; a name that could close the literal early
/// is how one query becomes two.
#[tokio::test]
async fn a_name_holding_a_quote_cannot_end_the_statement_early() {
    let fixture = Fixture::answering(&[&[Some("CREATE TABLE \"o'clock\" (n)")], &[]]);
    rendered(&fixture, &relation("main", "o'clock", RelationKind::Table)).await;
    assert!(
        fixture.asked()[0].ends_with("tbl_name = 'o''clock' AND name = 'o''clock'"),
        "the name was pasted unescaped: {}",
        fixture.asked()[0]
    );
}

/// A view is the statement SQLite kept, and nothing is wrapped around it.
#[tokio::test]
async fn a_view_is_the_statement_sqlite_kept() {
    let fixture = Fixture::defining(CREATE_OPEN_PARTS);
    let ddl = rendered(
        &fixture,
        &relation("main", "open_parts", RelationKind::View),
    )
    .await;
    assert_eq!(ddl, OPEN_PARTS);
}

/// A relation the catalog has no statement for is refused by name.
///
/// Upstream returns an empty string and shows a blank DDL tab. This is the one
/// place the two behave differently, and the reason is that a relation the
/// navigator listed and `sqlite_master` has never heard of means the tree has
/// gone stale — which a blank pane does not tell anybody.
#[tokio::test]
async fn a_relation_with_no_stored_statement_is_refused_rather_than_left_blank() {
    let fixture = Fixture::answering(&[&[]]);
    let error = dbddl::definition(
        &fixture,
        &dbsql::SQLITE,
        &relation("main", "vanished", RelationKind::Table),
    )
    .await
    .expect_err("a table with no statement rendered as something");
    assert!(
        error.to_string().contains("vanished"),
        "the refusal does not say which object it is about: {error}"
    );
}

/// A kind SQLite does not have is refused rather than rendered.
///
/// There is no materialized view in SQLite, so a relation arriving as one came
/// from somewhere this renderer cannot describe, and asking `sqlite_master` for
/// it would answer nothing in a more confusing way.
#[tokio::test]
async fn a_kind_sqlite_does_not_have_is_refused() {
    let error = dbddl::definition(
        &Fixture::default(),
        &dbsql::SQLITE,
        &relation("main", "totals", RelationKind::MaterializedView),
    )
    .await
    .expect_err("SQLite rendered a materialized view");
    assert!(error.to_string().contains("totals"), "{error}");
}

// ---------------------------------------------------------------------------
// Against a database
// ---------------------------------------------------------------------------

/// A database file, and the directory it dies with.
struct Fixed {
    source: SqliteSource,
    _dir: tempfile::TempDir,
}

/// Every statement above, in a file of its own.
///
/// A file rather than `:memory:`, because an in-memory database belongs to one
/// connection and this driver opens one per call. Built with `rusqlite` directly,
/// so that the fixture does not depend on the code under test being right.
async fn database() -> Fixed {
    let dir = tempfile::tempdir().expect("no temporary directory");
    let path = dir.path().join("ddl.db");
    let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
    let script: String = [
        CREATE_SHIPMENTS,
        SHIPMENTS_SHIPPED_ON,
        CREATE_PARTS,
        PARTS_EMAIL_LOWER,
        PARTS_SKU_KEY,
        PARTS_PENDING,
        PARTS_TOUCH,
        CREATE_BARE,
        CREATE_OPEN_PARTS,
        CREATE_DOCS,
    ]
    .iter()
    .map(|statement| format!("{statement};\n"))
    .collect();
    conn.execute_batch(&script).expect("fixture setup failed");
    drop(conn);

    let source = SqliteSource::connect(path.to_str().expect("a temporary path is UTF-8"))
        .await
        .expect("fixture database unreachable");
    Fixed { source, _dir: dir }
}

/// The DDL of one relation, found the way the navigator finds it.
///
/// Listed rather than constructed, so that the kind under test is the kind the
/// driver reports — a virtual table rendered from a hand-written
/// `RelationKind::Table` would prove nothing about what the navigator hands over.
async fn from_file(fixed: &Fixed, name: &str) -> String {
    let relation = fixed
        .source
        .relations("main")
        .await
        .expect("listing relations failed")
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the fixture database"));
    rendered(&fixed.source, &relation).await
}

/// The rich table, rendered out of `sqlite_master` rather than out of a fake.
///
/// This is the test the phase-4 criterion is about: the string it compares
/// against was written from the Java, so a difference here is a difference from
/// upstream. It is also the only place that proves the query finds the right rows
/// — `parts_touch` and `shipments_shipped_on` are both in this database, both
/// would be found by a query missing one of its conditions, and neither belongs
/// in this script.
#[tokio::test]
async fn the_parts_table_renders_from_the_file_exactly_as_upstream_writes_it() {
    let fixed = database().await;
    assert_eq!(from_file(&fixed, "parts").await, PARTS);
}

/// A table whose only index is SQLite's own, from the file.
///
/// Hand-fed answers can only prove the NULL row is skipped once something says it
/// is NULL; this proves `sqlite_autoindex_bare_1` really is a row with no
/// statement, which is why the skip has to exist.
#[tokio::test]
async fn the_bare_table_stops_at_its_own_statement() {
    let fixed = database().await;
    assert_eq!(from_file(&fixed, "bare").await, BARE);
}

/// The view, from the statement the file holds.
#[tokio::test]
async fn the_view_renders_from_the_statement_the_file_holds() {
    let fixed = database().await;
    assert_eq!(from_file(&fixed, "open_parts").await, OPEN_PARTS);
}

/// A virtual table renders, and the kind it arrives as is the reason it has to.
///
/// The driver reports `RelationKind::Virtual` for it, which upstream has no
/// equivalent of — `sqlite_master` says `table` and `SQLiteMetaModel` builds a
/// `SQLiteTable`. Refusing the kind would refuse an object upstream renders
/// perfectly well.
#[tokio::test]
async fn a_virtual_table_renders_as_the_table_upstream_takes_it_for() {
    let fixed = database().await;
    let docs = fixed
        .source
        .relations("main")
        .await
        .expect("listing relations failed")
        .into_iter()
        .find(|r| r.name == "docs")
        .expect("docs is not in the fixture database");
    assert_eq!(docs.kind, RelationKind::Virtual);
    assert_eq!(rendered(&fixed.source, &docs).await, DOCS);
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

/// The statement written for a file's columns is one SQLite runs.
///
/// The golden strings in the crate's own tests say what each database is *told*;
/// only the database says whether it understood. SQLite is a file, so this one
/// can be asked for real, and what it is asked for is the column list read back
/// out of the table it made — a type word SQLite did not recognise would arrive
/// here as something other than what was written.
#[tokio::test]
async fn a_table_made_for_a_files_columns_is_one_sqlite_runs() {
    let statement = dbddl::create_table(&dbsql::SQLITE, "landed", &a_files_columns())
        .expect("SQLite would not write a table for a file's columns");

    let dir = tempfile::tempdir().expect("no temporary directory");
    let path = dir.path().join("made.db");
    let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
    conn.execute_batch(&statement)
        .unwrap_or_else(|e| panic!("SQLite refused the statement: {e}\n{statement}"));
    drop(conn);

    let source = SqliteSource::connect(path.to_str().expect("a temporary path is UTF-8"))
        .await
        .expect("the database that was just made is unreachable");
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
            ("id".to_string(), "INTEGER".to_string()),
            ("Order Date".to_string(), "TEXT".to_string()),
            ("amount".to_string(), "NUMERIC(12, 2)".to_string()),
            ("ratio".to_string(), "REAL".to_string()),
            ("paid".to_string(), "BOOLEAN".to_string()),
            ("note".to_string(), "TEXT".to_string()),
            ("seen_at".to_string(), "TEXT".to_string()),
        ]
    );
}

/// The rename and the drop are statements SQLite runs, and they do what they say.
///
/// The half the golden strings cannot reach. `ALTER TABLE … RENAME TO` and
/// `DROP TABLE` are short enough to look obviously right and are exactly the two
/// where "looks right" is not worth much: a rename that quietly moved the table
/// somewhere, or a drop that named the wrong object, reads the same on the page.
/// So both are run and the result is read back out of the catalog.
///
/// Read back through `SqliteSource::relations` rather than through the
/// connection that ran them, so what is checked is the state the navigator would
/// draw afterwards.
#[tokio::test]
async fn a_rename_and_a_drop_are_statements_sqlite_runs() {
    let dir = tempfile::tempdir().expect("no temporary directory");
    let path = dir.path().join("changed.db");
    let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
    conn.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, note TEXT);\nINSERT INTO orders VALUES (1, 'one');")
        .expect("could not seed the fixture");

    let orders = RelationInfo {
        schema: "main".to_string(),
        name: "orders".to_string(),
        kind: RelationKind::Table,
        estimated_rows: None,
    };

    let rename = dbddl::table_change(
        &dbsql::SQLITE,
        &orders,
        dbddl::TableChange::Rename { to: "orders_old" },
    )
    .expect("SQLite would not write a rename");
    conn.execute_batch(&rename)
        .unwrap_or_else(|e| panic!("SQLite refused the rename: {e}\n{rename}"));

    let renamed = RelationInfo {
        name: "orders_old".to_string(),
        ..orders.clone()
    };
    let listed = names(&path).await;
    assert_eq!(
        listed,
        vec!["orders_old".to_string()],
        "after the rename the table is listed under the new name and nothing is listed under \
         the old one"
    );

    // The rows are still there, which is the whole difference between a rename
    // and the drop-and-recreate SQLite needs for a view.
    let kept: i64 = conn
        .query_row("SELECT count(*) FROM orders_old", [], |row| row.get(0))
        .expect("the renamed table could not be read");
    assert_eq!(kept, 1, "a rename keeps the rows");

    let removal = dbddl::table_change(&dbsql::SQLITE, &renamed, dbddl::TableChange::Drop)
        .expect("SQLite would not write a drop");
    conn.execute_batch(&removal)
        .unwrap_or_else(|e| panic!("SQLite refused the drop: {e}\n{removal}"));
    drop(conn);

    assert!(
        names(&path).await.is_empty(),
        "after the drop there is nothing left to list"
    );
}

// ---------------------------------------------------------------------------
// The two capabilities SQLite answers differently
// ---------------------------------------------------------------------------

/// SQLite makes and drops an index, and takes one constraint out of three —
/// which is why the two capabilities are separate and why this one is off.
///
/// The reason `changes_constraints` exists apart from `changes_indexes`, and
/// the reason the answer is what it is, are both here in one file. The index
/// statement this build writes is sent and accepted; the three
/// `ADD CONSTRAINT`s upstream's shared managers write are sent to the same
/// table, and what comes back is not what reading the Java would have led
/// anybody to expect.
///
/// Upstream's position is that SQLite has none of this:
/// `SQLiteSQLDialect.supportsAlterTableStatement` returns false, which is what
/// `GenericUtils.canAlterTable` reads and `GenericPrimaryKeyManager` refuses on,
/// and `SQLiteTableForeignKeyManager` throws "Forein key creation needs table
/// recreation" from create, modify and delete alike. That was true of every
/// SQLite when it was written and is no longer true of the one this build links:
/// the amalgamation `rusqlite` bundles has `ALTER TABLE … ADD CONSTRAINT` for a
/// CHECK and a `DROP CONSTRAINT` that parses, and still refuses a unique
/// constraint and a foreign key at the keyword. This test is where that is
/// written down, because it is the kind of fact that is only ever true of the
/// library in the binary and can change under the next bump.
///
/// So the capability stays off, and not out of deference: one flag stands for
/// three sorts, and two of the three cannot be written here. Drawing the section
/// would give a SQLite table an Add Unique and an Add Foreign Key that refuse
/// whichever is clicked, which is the thing `alters_columns` was split out to
/// stop. What is lost is the check constraint, and that is a limitation to
/// record rather than a menu to lie with.
///
/// The pair matters more than either half. A build that had folded the two
/// capabilities into one would pass whichever half it kept and fail this.
#[tokio::test]
async fn sqlite_takes_an_index_statement_and_two_constraints_out_of_three_it_cannot() {
    let dir = tempfile::tempdir().expect("no temporary directory");
    let path = dir.path().join("constraints.db");
    let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
    conn.execute_batch(
        "CREATE TABLE orders (sku TEXT NOT NULL, qty INTEGER NOT NULL, customer_id INTEGER);",
    )
    .expect("could not seed the fixture");

    let orders = relation("main", "orders", RelationKind::Table);

    // The half that works. Written by this build, run by SQLite, and read back
    // out of the catalog so that the index is a fact rather than an absence of
    // errors.
    let index = dbddl::NewIndex {
        name: "orders_sku_idx".into(),
        columns: vec!["sku".into()],
        unique: true,
        method: None,
    };
    let created = dbddl::index_change(&dbsql::SQLITE, &orders, dbddl::IndexChange::Create(&index))
        .expect("SQLite would not write a CREATE INDEX");
    conn.execute_batch(&created)
        .unwrap_or_else(|e| panic!("SQLite refused the index: {e}\n{created}"));
    let indexed: String = conn
        .query_row(
            "SELECT name FROM main.sqlite_master WHERE type = 'index' AND tbl_name = 'orders'",
            [],
            |row| row.get(0),
        )
        .expect("the index this build wrote is not in the catalog");
    assert_eq!(indexed, "orders_sku_idx");

    // The two that cannot be written, in the words upstream's shared managers
    // write them — `SQLConstraintManager.addObjectCreateActions` and
    // `SQLForeignKeyManager.getNestedDeclarationScript` — aimed at the table
    // that is right there. Both stop at the keyword after the constraint's
    // name, which is as far as SQLite's grammar goes.
    for (statement, keyword) in [
        (
            "ALTER TABLE main.orders ADD CONSTRAINT orders_sku_key UNIQUE (sku)",
            "UNIQUE",
        ),
        (
            "ALTER TABLE main.orders ADD CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) \
             REFERENCES customers(id)",
            "FOREIGN",
        ),
    ] {
        let refused = match conn.execute_batch(statement) {
            Ok(()) => panic!("SQLite accepted a constraint it has no syntax for: {statement}"),
            Err(e) => e.to_string(),
        };
        assert!(
            refused.contains("syntax error") && refused.contains(keyword),
            "SQLite refused {statement} somewhere other than at {keyword}: {refused}"
        );
    }

    // And the one that is taken, which is the fact upstream does not have and
    // the reason this test asserts what the library does rather than what the
    // Java says it does. The clause lands in the stored `CREATE TABLE` as a
    // table constraint, which is where a reader of the DDL tab would find it.
    conn.execute_batch("ALTER TABLE main.orders ADD CONSTRAINT orders_qty_check CHECK (qty > 0)")
        .expect("the bundled SQLite no longer takes a CHECK constraint; see this test's note");
    let stored: String = conn
        .query_row(
            "SELECT sql FROM main.sqlite_master WHERE name = 'orders'",
            [],
            |row| row.get(0),
        )
        .expect("the table went away");
    assert!(
        stored.ends_with("CONSTRAINT orders_qty_check CHECK (qty > 0))"),
        "the check went somewhere other than into the table's own text: {stored}"
    );
    drop(conn);

    // And what this build says about the same table. Two sorts out of three
    // cannot be written, so nothing is offered — and the refusal says which two,
    // rather than claiming SQLite has no constraint syntax at all.
    assert!(dbddl::changes_indexes(&dbsql::SQLITE));
    assert!(!dbddl::changes_constraints(&dbsql::SQLITE));
    let key = dbddl::NewConstraint::Unique {
        name: "orders_sku_key".into(),
        columns: vec!["sku".into()],
    };
    let said = dbddl::constraint_change(
        &dbsql::SQLITE,
        &orders,
        dbddl::ConstraintChange::Create(&key),
    )
    .expect_err("SQLite wrote an ADD CONSTRAINT")
    .to_string();
    assert!(
        said.contains("unique constraint or a foreign key"),
        "the refusal should say which sorts SQLite has no syntax for: {said}"
    );
    assert!(
        said.contains("building the table again"),
        "the refusal should say what it would take instead: {said}"
    );
    assert!(
        !said.contains("yet"),
        "a refusal that will never change by waiting: {said}"
    );
}

/// The relations `main` holds, as the navigator would list them.
async fn names(path: &std::path::Path) -> Vec<String> {
    let source = SqliteSource::connect(path.to_str().expect("a temporary path is UTF-8"))
        .await
        .expect("the fixture is unreachable");
    source
        .relations("main")
        .await
        .expect("listing failed")
        .into_iter()
        .map(|relation| relation.name)
        .collect()
}
