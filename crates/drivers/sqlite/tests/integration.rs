//! End-to-end correctness against a database built for each test.
//!
//! Unlike the PostgreSQL suite these need no server and no `make db-seed`, so
//! they run under plain `make test`. That is not a convenience: it means the
//! live read path of a driver — connect, execute, stream, page, cancel — is
//! covered on every run rather than on the runs somebody remembered to start a
//! container for.
//!
//! Each test builds its own database with `rusqlite` directly, so the fixture
//! does not depend on the code under test being correct.

use arrow::array::{Array, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::DataType;
use driver_sqlite::{SqliteError, SqliteSource};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// A database file that lives as long as the test does.
struct Fixture {
    _dir: TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new(setup: &str) -> Self {
        let dir = tempfile::tempdir().expect("no temporary directory");
        let path = dir.path().join("fixture.db");
        let conn = rusqlite::Connection::open(&path).expect("could not create the fixture");
        conn.execute_batch(setup).expect("fixture setup failed");
        Self { _dir: dir, path }
    }

    async fn connect(&self) -> SqliteSource {
        SqliteSource::connect(self.path.to_str().unwrap())
            .await
            .expect("fixture database unreachable")
    }

    /// Opens a second connection of the test's own, for writing behind a reader.
    fn writer(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).expect("could not reopen the fixture")
    }
}

/// The failure `sql` produces, insisting there is one.
///
/// A helper rather than `expect_err`, which wants the success type to be
/// printable — and a live result is a connection and a thread, not something
/// with a useful `Debug`.
async fn query_error(src: &SqliteSource, sql: &str) -> SqliteError {
    match src.query(sql, 10).await {
        Ok(_) => panic!("expected this to fail: {sql}"),
        Err(e) => e,
    }
}

fn col<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
    let idx = batch.schema().index_of(name).expect("column missing");
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column {name} has unexpected array type"))
}

/// `n` rows of ascending integers, which is enough shape for the paging tests.
fn counted(n: u32) -> String {
    format!(
        "CREATE TABLE nums (id INTEGER PRIMARY KEY, label TEXT);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n})
         INSERT INTO nums (id, label) SELECT x, 'row-' || x FROM c;"
    )
}

#[tokio::test]
async fn a_declared_column_is_typed_by_its_declaration_and_the_rest_by_their_values() {
    // The whole of the type-resolution rule in one result. `price` and `moment`
    // are declared, but into NUMERIC affinity, which decides nothing about what
    // a row holds; `loose` is declared with no type at all.
    let fixture = Fixture::new(
        "CREATE TABLE typed (
             id INTEGER PRIMARY KEY,
             name TEXT,
             amount REAL,
             blob_val BLOB,
             price NUMERIC,
             moment DATETIME,
             loose
         );
         INSERT INTO typed VALUES (1, 'first', 2.5, x'0102', 12.5, '2024-01-01', 7);",
    );
    let src = fixture.connect().await;
    let mut stream = src.query("SELECT * FROM typed", 100).await.unwrap();

    let expected = [
        ("id", DataType::Int64),
        ("name", DataType::Utf8),
        ("amount", DataType::Float64),
        ("blob_val", DataType::Binary),
        // Decided by the value, because NUMERIC affinity does not decide.
        ("price", DataType::Float64),
        // The case this rule exists for: a DATETIME column holding text. Read as
        // a number it would be an error on a database SQLite is happy with.
        ("moment", DataType::Utf8),
        ("loose", DataType::Int64),
    ];
    let schema = stream.schema();
    assert_eq!(schema.fields().len(), expected.len());
    for (name, data_type) in expected {
        let field = schema.field_with_name(name).expect("column missing");
        assert_eq!(field.data_type(), &data_type, "type of {name}");
    }

    let batch = stream.next_batch().await.unwrap().expect("one row");
    assert_eq!(col::<Int64Array>(&batch, "id").value(0), 1);
    assert_eq!(col::<StringArray>(&batch, "name").value(0), "first");
    assert_eq!(col::<Float64Array>(&batch, "amount").value(0), 2.5);
    assert_eq!(col::<BinaryArray>(&batch, "blob_val").value(0), &[1, 2]);
    assert_eq!(col::<StringArray>(&batch, "moment").value(0), "2024-01-01");
    assert_eq!(col::<Int64Array>(&batch, "loose").value(0), 7);
}

#[tokio::test]
async fn an_expression_column_is_typed_from_its_first_value() {
    // A prepared statement can describe a table column's declaration and nothing
    // about an expression, so `count(*)` has only its value to go on. Reading it
    // as text — the safe answer — would right-align nothing and sort wrongly.
    let fixture = Fixture::new(&counted(3));
    let src = fixture.connect().await;

    let mut counted = src
        .query("SELECT count(*) AS n FROM nums", 10)
        .await
        .unwrap();
    assert_eq!(counted.schema().field(0).data_type(), &DataType::Int64);
    let batch = counted.next_batch().await.unwrap().expect("one row");
    assert_eq!(col::<Int64Array>(&batch, "n").value(0), 3);

    let literal = src.query("SELECT 'x' AS s", 10).await.unwrap();
    assert_eq!(literal.schema().field(0).data_type(), &DataType::Utf8);
}

#[tokio::test]
async fn an_empty_result_still_reports_the_columns_it_would_have_had() {
    // The front end lays out a grid before a row arrives, so a result with none
    // still has to answer for its shape.
    let fixture = Fixture::new(&counted(3));
    let src = fixture.connect().await;
    let mut stream = src
        .query("SELECT id, label FROM nums WHERE id < 0", 10)
        .await
        .unwrap();
    let schema = stream.schema();
    assert_eq!(schema.fields().len(), 2);
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    assert!(stream.next_batch().await.unwrap().is_none());
}

#[tokio::test]
async fn a_result_arrives_in_batches_of_the_size_asked_for() {
    let fixture = Fixture::new(&counted(2500));
    let src = fixture.connect().await;
    let mut stream = src
        .query("SELECT id FROM nums ORDER BY id", 1000)
        .await
        .unwrap();

    let mut sizes = Vec::new();
    let mut seen = 0i64;
    while let Some(batch) = stream.next_batch().await.unwrap() {
        let ids = col::<Int64Array>(&batch, "id");
        for i in 0..ids.len() {
            seen += 1;
            assert_eq!(ids.value(i), seen, "rows must arrive in order, once each");
        }
        sizes.push(batch.num_rows());
    }
    assert_eq!(sizes, vec![1000, 1000, 500]);
}

#[tokio::test]
async fn a_result_stops_growing_while_nobody_reads_it() {
    // The bound Phase 1 asked for. The reader is one batch ahead at most, so a
    // result nobody is draining holds two batches, not the table.
    let fixture = Fixture::new(&counted(100_000));
    let src = fixture.connect().await;
    let mut stream = src
        .query("SELECT id FROM nums ORDER BY id", 10)
        .await
        .unwrap();

    // Long enough that a reader with nothing holding it back would have run the
    // whole table out and reported its count.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        stream.rows_affected(),
        None,
        "the reader should be parked one batch ahead, not finished"
    );

    // And nothing was lost while it waited: the first row is still the first row.
    let batch = stream.next_batch().await.unwrap().expect("a first batch");
    assert_eq!(col::<Int64Array>(&batch, "id").value(0), 1);
}

#[tokio::test]
async fn a_cursor_pages_a_result_without_repeating_or_skipping_a_row() {
    let fixture = Fixture::new(&counted(500));
    let src = fixture.connect().await;
    let mut cursor = src
        .cursor("SELECT id FROM nums ORDER BY id", 100)
        .await
        .unwrap();
    assert_eq!(cursor.schema().field(0).data_type(), &DataType::Int64);

    let mut seen = 0i64;
    while let Some(batch) = cursor.fetch().await.unwrap() {
        let ids = col::<Int64Array>(&batch, "id");
        for i in 0..ids.len() {
            seen += 1;
            assert_eq!(ids.value(i), seen);
        }
    }
    assert_eq!(seen, 500);
}

#[tokio::test]
async fn a_write_landing_mid_read_does_not_change_what_the_later_pages_say() {
    // What a cursor is for. SQLite gives it without a `DECLARE`: the read
    // transaction a statement opens on its first step lasts until its last, so
    // pages of one statement are pages of one snapshot.
    //
    // WAL, because the point is a writer that succeeds. In the default journal
    // mode the reader's lock would refuse the write and the test would prove
    // only that SQLite can say "busy".
    let fixture = Fixture::new(&format!("PRAGMA journal_mode=WAL; {}", counted(5_000)));
    let src = fixture.connect().await;
    let mut cursor = src
        .cursor("SELECT id FROM nums ORDER BY id", 100)
        .await
        .unwrap();

    // One page taken, so the statement is open and mid-result.
    cursor.fetch().await.unwrap().expect("a first page");

    fixture
        .writer()
        .execute_batch(
            "INSERT INTO nums (id, label) SELECT id + 100000, label FROM nums;
             DELETE FROM nums WHERE id > 4000 AND id <= 5000;",
        )
        .expect("the writer should not be blocked");

    // The page already taken, plus whatever is left.
    let mut seen = 100i64;
    while let Some(batch) = cursor.fetch().await.unwrap() {
        seen += batch.num_rows() as i64;
    }
    assert_eq!(
        seen, 5_000,
        "the cursor should report the table as it was when it started reading"
    );
}

#[tokio::test]
async fn a_statement_says_what_it_affected_only_once_it_has_finished() {
    let fixture = Fixture::new(&counted(10));
    let src = fixture.connect().await;

    let mut select = src.query("SELECT id FROM nums", 4).await.unwrap();
    // Zero is a real answer — an UPDATE that matched nothing — so "not yet" has
    // to be something else.
    assert_eq!(select.rows_affected(), None);
    while select.next_batch().await.unwrap().is_some() {}
    assert_eq!(select.rows_affected(), Some(10));

    let mut update = src
        .query("UPDATE nums SET label = 'x' WHERE id <= 3", 4)
        .await
        .unwrap();
    assert!(update.next_batch().await.unwrap().is_none());
    // Not the count SQLite would answer for the SELECT above: `changes()` keeps
    // reporting the last writing statement, so a reading one must not believe it.
    assert_eq!(update.rows_affected(), Some(3));
}

#[tokio::test]
async fn a_statement_that_cannot_be_parsed_says_where_it_went_wrong() {
    let fixture = Fixture::new(&counted(1));
    let src = fixture.connect().await;

    let err = query_error(&src, "SELECT id FROM nums WHERE ORDER BY id").await;
    let position = err
        .statement_position()
        .expect("a syntax error should carry a position");
    // 1-based and counted in characters, as the FFI contract states and as
    // PostgreSQL reports. `ORDER` is the 27th character of that statement.
    assert_eq!(position, 27, "message was: {err}");

    // A failure that is not about a place in the statement must not invent one,
    // or the caret lands wherever the arithmetic happened to point.
    let missing = query_error(&src, "SELECT * FROM no_such_table").await;
    assert_eq!(missing.statement_position(), None, "message was: {missing}");
}

#[tokio::test]
async fn a_position_is_counted_in_characters_rather_than_bytes() {
    // SQLite counts bytes; the contract is characters. The two agree on every
    // statement written in English, which is how this stays broken until
    // somebody names a table in their own language.
    let fixture = Fixture::new("CREATE TABLE 訂單 (id INTEGER);");
    let src = fixture.connect().await;
    let err = query_error(&src, "SELECT * FROM 訂單 WHERE ORDER BY id").await;
    // `ORDER` is the 24th character and the 28th byte, so a position that
    // stayed in bytes would put the caret four characters past the word.
    assert_eq!(err.statement_position(), Some(24), "message was: {err}");
}

#[tokio::test]
async fn a_running_statement_stops_when_asked_and_says_that_is_why() {
    let fixture = Fixture::new(&counted(1));
    let src = Arc::new(fixture.connect().await);

    // Scheduled before the statement is sent. `query` does not come back while
    // the statement is running — SQLite has to produce a first row before the
    // column types are settled — so a cancel issued after that call would be
    // cancelling something that already finished.
    let canceller = Arc::clone(&src);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });

    // Counting to a hundred million rather than a large scan of the fixture:
    // work SQLite could finish early would let this pass on a build where
    // cancellation does nothing at all.
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000000)
               SELECT count(*) FROM c";
    let err = tokio::time::timeout(Duration::from_secs(20), query_error(&src, sql))
        .await
        .expect("the statement was still running 20s after being cancelled");

    assert!(err.is_cancelled(), "expected a cancellation, got: {err}");

    // Every other failure has to stay distinguishable from this one, or the
    // front end labels real faults as something the user did on purpose.
    let ordinary = query_error(&src, "SELECT * FROM no_such_table").await;
    assert!(!ordinary.is_cancelled(), "got: {ordinary}");

    // The session is usable afterwards: an interrupt stops a statement, not the
    // connection it ran on.
    let mut after = src
        .query("SELECT count(*) AS n FROM nums", 10)
        .await
        .unwrap();
    let batch = after.next_batch().await.unwrap().expect("one row");
    assert_eq!(col::<Int64Array>(&batch, "n").value(0), 1);
}

#[tokio::test]
async fn a_cursor_stops_the_page_it_is_fetching() {
    // A cursor carries its own canceller for the same reason the PostgreSQL one
    // does: cancelling means reaching the connection at the moment a fetch has
    // it, so the handle has to be something taken out beforehand and held.
    let fixture = Fixture::new(&counted(200));
    let src = fixture.connect().await;

    // Cheap for the first hundred rows and unbounded after them. The subquery
    // names `id`, which is what makes it run once per row rather than once.
    let sql = "SELECT id FROM nums
               WHERE id <= 100
                  OR (WITH RECURSIVE c(x) AS (
                          SELECT id UNION ALL SELECT x + 1 FROM c WHERE x < 100000000
                      ) SELECT count(*) FROM c) > 0";
    let mut cursor = src.cursor(sql, 100).await.unwrap();
    let canceller = cursor.canceller();

    let first = cursor.fetch().await.unwrap().expect("a first page");
    assert_eq!(
        first.num_rows(),
        100,
        "the cheap rows should come back at once"
    );

    tokio::spawn(async move {
        // Long enough for the second page to be under way. Cancelling before it
        // starts would find nothing to stop, which is the one outcome that looks
        // the same as a canceller that does not work.
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });

    let err = tokio::time::timeout(Duration::from_secs(20), cursor.fetch())
        .await
        .expect("the fetch was still running 20s after being cancelled")
        .expect_err("the fetch should have been interrupted");
    assert!(err.is_cancelled(), "expected a cancellation, got: {err}");
}

#[tokio::test]
async fn a_value_that_does_not_fit_its_column_is_reported_rather_than_rounded() {
    // SQLite lets a REAL sit in a column declared INTEGER. Showing 1 where the
    // database holds 1.5 is the one outcome worse than an error.
    let fixture = Fixture::new(
        "CREATE TABLE loose (n INTEGER);
         INSERT INTO loose VALUES (1), (2);
         UPDATE loose SET n = 1.5 WHERE n = 2;",
    );
    let src = fixture.connect().await;
    let mut stream = src
        .query("SELECT n FROM loose ORDER BY rowid", 10)
        .await
        .unwrap();
    let err = stream
        .next_batch()
        .await
        .expect_err("a fractional value cannot be an integer");
    match err {
        SqliteError::TypeMismatch {
            column,
            found,
            expected,
        } => {
            assert_eq!(column, "n");
            assert_eq!(found, "REAL");
            assert_eq!(expected, "INTEGER");
        }
        other => panic!("expected a mismatch naming the column, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// A schema with one of everything the sidebar has a section for.
const CATALOG: &str = "
    CREATE TABLE authors (
        id      INTEGER PRIMARY KEY,
        name    TEXT NOT NULL,
        email   TEXT,
        country TEXT DEFAULT 'TW'
    );
    CREATE UNIQUE INDEX authors_email ON authors (lower(email)) WHERE email IS NOT NULL;

    CREATE TABLE books (
        isbn      TEXT NOT NULL,
        region    TEXT NOT NULL,
        author_id INTEGER REFERENCES authors,
        editor_id INTEGER,
        title     TEXT CHECK (length(title) > 0),
        UNIQUE (title),
        FOREIGN KEY (editor_id) REFERENCES authors (id)
            ON DELETE SET NULL ON UPDATE CASCADE,
        PRIMARY KEY (isbn, region)
    );

    CREATE VIEW in_print AS SELECT title FROM books WHERE title IS NOT NULL;

    CREATE TRIGGER books_audit AFTER UPDATE ON books BEGIN SELECT 1; END;
";

#[tokio::test]
async fn the_navigator_root_is_the_databases_attached_to_this_connection() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let names: Vec<String> = src
        .schemas()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    // `temp` is deliberately absent: it holds one connection's temporary tables,
    // and every call here gets a connection of its own, so it could never have
    // anything under it.
    assert_eq!(names, ["main"]);
}

#[tokio::test]
async fn relations_report_what_kind_they_are_and_leave_sqlites_own_out() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let relations = src.relations("main").await.unwrap();

    let listed: Vec<(&str, driver_sqlite::RelationKind)> = relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    assert_eq!(
        listed,
        [
            ("authors", driver_sqlite::RelationKind::Table),
            ("books", driver_sqlite::RelationKind::Table),
            ("in_print", driver_sqlite::RelationKind::View),
        ],
        "sqlite_autoindex and friends are SQLite's bookkeeping, not the user's"
    );
    assert!(relations.iter().all(|r| r.schema == "main"));
}

#[tokio::test]
async fn a_row_estimate_is_absent_until_something_has_measured_it() {
    let fixture = Fixture::new(&format!(
        "{CATALOG}
         INSERT INTO authors (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');"
    ));
    let src = fixture.connect().await;

    let before = src.relations("main").await.unwrap();
    let authors = before.iter().find(|r| r.name == "authors").unwrap();
    // Not zero. A sidebar that says a table has no rows when nobody has counted
    // is stating something false rather than declining to answer.
    assert_eq!(authors.estimated_rows, None);

    fixture.writer().execute_batch("ANALYZE").unwrap();
    let after = src.relations("main").await.unwrap();
    let authors = after.iter().find(|r| r.name == "authors").unwrap();
    assert_eq!(authors.estimated_rows, Some(3));
}

#[tokio::test]
async fn columns_report_their_declaration_key_and_default() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let columns = src.columns("main", "authors").await.unwrap();

    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "email", "country"]);

    let id = &columns[0];
    assert_eq!(id.data_type, "INTEGER");
    assert!(id.is_primary_key);
    // One-based, as PostgreSQL's attnum is. SQLite counts from zero, and a front
    // end showing both would call the same column first and zeroth depending on
    // which database it came from.
    assert_eq!(id.position, 1);

    assert!(!columns[1].nullable, "name is declared NOT NULL");
    assert!(columns[2].nullable);
    assert_eq!(columns[3].default_value.as_deref(), Some("'TW'"));
}

#[tokio::test]
async fn a_column_declared_without_a_type_says_so_rather_than_leaving_a_blank() {
    let fixture = Fixture::new("CREATE TABLE loose (a, b INTEGER);");
    let src = fixture.connect().await;
    let columns = src.columns("main", "loose").await.unwrap();
    // SQLite reports an empty string, which renders as a gap in the structure
    // pane and reads as a defect in the client rather than as the truth about the
    // column. `ANY` is the word SQLite's own STRICT tables use for it.
    assert_eq!(columns[0].data_type, "ANY");
    assert_eq!(columns[1].data_type, "INTEGER");
}

#[tokio::test]
async fn a_view_reports_the_statement_it_was_created_from() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;

    let definition = src
        .definition("main", "in_print")
        .await
        .unwrap()
        .expect("a view has a definition");
    assert!(definition.contains("CREATE VIEW"), "got: {definition}");
    assert!(definition.contains("WHERE title IS NOT NULL"));

    // Absent rather than empty for a table, which is the distinction the
    // structure pane hangs a section on.
    assert_eq!(src.definition("main", "books").await.unwrap(), None);
}

#[tokio::test]
async fn an_index_reports_the_keys_the_planner_can_actually_use() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let indexes = src.indexes("main", "authors").await.unwrap();

    let email = indexes
        .iter()
        .find(|i| i.name == "authors_email")
        .expect("the declared index is missing");
    // An index on lower(email) is not an index on email. No pragma will name an
    // expression key, so this comes out of the statement that declared it.
    assert_eq!(email.columns, ["lower(email)"]);
    assert!(email.is_unique);
    assert!(!email.is_primary);
    assert_eq!(email.method, "btree");
    // No pragma reports a partial index's predicate either.
    assert_eq!(email.predicate.as_deref(), Some("email IS NOT NULL"));

    // `authors.id` is INTEGER PRIMARY KEY, which SQLite implements as the rowid
    // rather than as an index — so there is nothing here to list, and
    // ColumnInfo::is_primary_key is where a front end should read the key from.
    assert!(!indexes.iter().any(|i| i.is_primary));
}

#[tokio::test]
async fn an_index_sqlite_made_for_itself_still_reports_its_columns() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let indexes = src.indexes("main", "books").await.unwrap();

    let primary = indexes
        .iter()
        .find(|i| i.is_primary)
        .expect("a composite primary key is an index");
    assert_eq!(primary.columns, ["isbn", "region"]);
    // No CREATE INDEX statement exists for it, so the key list has to come from
    // the pragma rather than from DDL text there is none of.
    assert!(primary.is_unique);
}

#[tokio::test]
async fn a_foreign_key_reports_both_sides_and_what_happens_on_delete() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let keys = src.foreign_keys("main", "books").await.unwrap();
    assert_eq!(keys.len(), 2);

    let editor = keys
        .iter()
        .find(|k| k.local_columns == ["editor_id"])
        .expect("the editor key is missing");
    assert_eq!(editor.other_table, "authors");
    assert_eq!(editor.other_columns, ["id"]);
    assert_eq!(editor.on_delete, "SET NULL");
    assert_eq!(editor.on_update, "CASCADE");
    assert_eq!(editor.other_schema, "main");

    // `author_id INTEGER REFERENCES authors` names no column on the far side,
    // and SQLite leaves it out rather than filling it in. A key rendered with one
    // side blank reads as though the database were missing something.
    let author = keys
        .iter()
        .find(|k| k.local_columns == ["author_id"])
        .expect("the author key is missing");
    assert_eq!(author.other_columns, ["id"]);
}

#[tokio::test]
async fn an_inbound_reference_is_named_for_the_table_that_declared_it() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let inbound = src.referenced_by("main", "authors").await.unwrap();
    assert_eq!(inbound.len(), 2);

    for key in &inbound {
        assert_eq!(key.other_table, "books");
        // Named for the vantage point: looked at from authors, the local columns
        // are authors', and both keys point at its primary key.
        assert_eq!(key.local_columns, ["id"]);
        // The key lives on books even though books is not what was asked about,
        // and a made-up name that said `authors` would misplace it.
        assert!(key.name.starts_with("fk_books_"), "got: {}", key.name);
    }

    let referencing: Vec<&str> = inbound
        .iter()
        .map(|k| k.other_columns[0].as_str())
        .collect();
    assert!(referencing.contains(&"author_id"));
    assert!(referencing.contains(&"editor_id"));

    assert!(
        src.referenced_by("main", "books").await.unwrap().is_empty(),
        "nothing references books"
    );
}

#[tokio::test]
async fn a_unique_constraint_is_reported_and_a_check_constraint_honestly_is_not() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let constraints = src.constraints("main", "books").await.unwrap();

    assert_eq!(constraints.len(), 1, "got: {constraints:?}");
    assert_eq!(constraints[0].kind, driver_sqlite::ConstraintKind::Unique);
    assert_eq!(constraints[0].definition, "UNIQUE (title)");

    // books also has `CHECK (length(title) > 0)`, and it is not here. SQLite
    // records a CHECK only inside the CREATE TABLE text, and reading it out of
    // there is parsing SQL — Phase 3's job, not something to half-do here.
}

#[tokio::test]
async fn a_trigger_reports_the_statement_it_was_created_from() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let triggers = src.triggers("main", "books").await.unwrap();

    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].name, "books_audit");
    // Not the timing and events in separate fields, as PostgreSQL reports them.
    // SQLite's catalog holds the text and nothing else, and picking `AFTER` out
    // of it is guessing at what the reader can see for themselves.
    let definition = triggers[0].definition.as_deref().expect("the DDL");
    assert!(
        definition.contains("AFTER UPDATE ON books"),
        "got: {definition}"
    );

    assert!(src.triggers("main", "authors").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_schema_that_is_not_open_says_so_in_those_words() {
    // Every schema-scoped call answers the same way, which is the point. Left to
    // themselves the pragma-backed ones return an empty list and the
    // sqlite_schema-backed ones fail with "no such table: nowhere.sqlite_schema"
    // — an internal detail about a table nobody mentioned.
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;

    for message in [
        src.relations("nowhere").await.unwrap_err().to_string(),
        src.columns("nowhere", "authors")
            .await
            .unwrap_err()
            .to_string(),
        src.indexes("nowhere", "authors")
            .await
            .unwrap_err()
            .to_string(),
        src.triggers("nowhere", "authors")
            .await
            .unwrap_err()
            .to_string(),
        src.definition("nowhere", "in_print")
            .await
            .unwrap_err()
            .to_string(),
        src.constraints("nowhere", "books")
            .await
            .unwrap_err()
            .to_string(),
        src.foreign_keys("nowhere", "books")
            .await
            .unwrap_err()
            .to_string(),
        src.referenced_by("nowhere", "authors")
            .await
            .unwrap_err()
            .to_string(),
    ] {
        assert!(message.contains("nowhere"), "got: {message}");
        assert!(!message.contains("sqlite_schema"), "got: {message}");
    }

    // And the schema that is open still reads.
    assert_eq!(src.relations("main").await.unwrap().len(), 3);
}
