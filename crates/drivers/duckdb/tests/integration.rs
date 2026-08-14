//! End-to-end correctness against a database built for each test.
//!
//! There is no `docker run` line here and nothing is `#[ignore]`d, because
//! DuckDB is a library in this process: the fixture is a file in a temporary
//! directory, written with the `duckdb` crate directly so that it does not
//! depend on the code under test being right. `driver-sqlite` is in the same
//! position and says why it matters — the live read path of a driver, connect
//! through execute and page and cancel, is covered on every `cargo test` rather
//! than on the runs somebody remembered to start a container for.
//!
//! One consequence of DuckDB's file locking shapes the fixture: a read-write
//! database has one instance per file, so the connection that writes the fixture
//! is dropped before the driver opens it, and a test that needs to write behind
//! a reader writes through the driver.

use arrow::array::{Array, Decimal128Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow::datatypes::{DataType, TimeUnit};
use dbconn::{Driver, RelationKind};
use driver_duckdb::{DuckError, DuckSource};
use std::path::PathBuf;
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

/// A schema name as this driver spells one.
///
/// `fixture` is the database name DuckDB derives from `fixture.duckdb`, and
/// every metadata call here takes the two levels joined, because the trait has
/// room for one.
fn schema(name: &str) -> String {
    format!("fixture.{name}")
}

/// The failure `sql` produces, insisting there is one.
///
/// A helper rather than `expect_err`, which wants the success type to be
/// printable — and a live result is a connection and a thread, not something
/// with a useful `Debug`. It looks in both places because a DuckDB statement can
/// fail at execution or at the first chunk, and which one depends on whether the
/// plan produces anything before it reaches the trouble.
async fn failure(src: &DuckSource, sql: &str) -> DuckError {
    match src.query(sql, 100).await {
        Err(e) => e,
        Ok(mut stream) => match stream.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {sql}"),
        },
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

fn field_type(src: &arrow::datatypes::SchemaRef, name: &str) -> DataType {
    src.field_with_name(name)
        .unwrap_or_else(|_| panic!("column {name} missing"))
        .data_type()
        .clone()
}

/// `n` rows of ascending integers, which is enough shape for the paging tests.
fn counted(n: u32) -> String {
    format!("CREATE TABLE nums AS SELECT i AS id, 'row-' || i AS label FROM range({n}) t(i);")
}

/// Everything the catalog calls have to answer for, and one table that has none
/// of it.
const CATALOG: &str = "
    CREATE SCHEMA app;

    CREATE TABLE app.customers (
        id     INTEGER PRIMARY KEY,
        email  VARCHAR NOT NULL,
        tier   INTEGER DEFAULT 1,
        CONSTRAINT customers_email_unique UNIQUE (email),
        CONSTRAINT customers_tier_range   CHECK (tier BETWEEN 1 AND 5)
    );

    CREATE TABLE app.regions (
        country VARCHAR,
        zone    VARCHAR,
        label   VARCHAR,
        PRIMARY KEY (country, zone)
    );

    CREATE TABLE app.orders (
        id           INTEGER PRIMARY KEY,
        customer_id  INTEGER,
        ship_country VARCHAR,
        ship_zone    VARCHAR,
        total        DECIMAL(12, 2) NOT NULL,
        placed_at    TIMESTAMP,
        CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id)
            REFERENCES app.customers (id),
        CONSTRAINT orders_region_fk FOREIGN KEY (ship_country, ship_zone)
            REFERENCES app.regions (country, zone),
        CONSTRAINT orders_total_positive CHECK (total > 0)
    );

    CREATE INDEX orders_placed_idx     ON app.orders (placed_at);
    CREATE UNIQUE INDEX orders_cust_ux ON app.orders (customer_id, placed_at);
    CREATE INDEX orders_lower_country  ON app.orders (lower(ship_country));

    CREATE VIEW app.recent_orders AS
        SELECT id, customer_id, total FROM app.orders WHERE total > 100;

    -- Nothing declared on it: no key, no index, no constraint, no reference.
    CREATE TABLE app.notes (body VARCHAR);

    INSERT INTO app.customers VALUES (1, 'a@example.com', 1), (2, 'b@example.com', 3);
    INSERT INTO app.regions VALUES ('TW', 'north', 'Taipei'), ('JP', 'kanto', 'Tokyo');
    INSERT INTO app.orders VALUES
        (1, 1, 'TW', 'north', 100.00, TIMESTAMP '2024-01-01 00:00:00'),
        (2, 2, 'JP', 'kanto', 250.50, TIMESTAMP '2024-02-01 00:00:00');
";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The seed for the type tests. Values are boundaries rather than `1`, so a
/// wrong mapping is visible rather than plausible.
const TYPES: &str = "
    CREATE TYPE status AS ENUM ('draft', 'live', 'archived');
    CREATE TABLE types_all AS SELECT
        18446744073709551615::UBIGINT   AS v_ubigint,
        340282366920938463463374607431768211455::UHUGEINT AS v_uhugeint,
        (-170141183460469231731687303715884105728)::HUGEINT AS v_hugeint,
        999999999999.999999::DECIMAL(18, 6) AS v_dec,
        'ünïcödé ✓ 漢字'::VARCHAR       AS v_varchar,
        'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::UUID AS v_uuid,
        '{\"nested\": [1, 2]}'::JSON      AS v_json,
        TIME '23:59:59.999999'          AS v_time,
        TIMETZ '23:59:59.999999+08'     AS v_timetz,
        TIMESTAMP '9999-12-31 23:59:59.999999' AS v_timestamp,
        INTERVAL '1 year 2 months 3 days' AS v_interval,
        'archived'::status              AS v_enum,
        [1, -1, 2147483647]             AS v_list,
        [[1], [2, 3]]                   AS v_list_nested,
        [1, 2, 3]::INTEGER[3]           AS v_array,
        {'qty': 2147483647, 'unit': 'kg'} AS v_struct,
        MAP {'a': 1, 'b': 2}            AS v_map;
";

#[tokio::test]
async fn the_types_a_duckdb_user_has_arrive_as_themselves() {
    let fixture = Fixture::new(TYPES);
    let src = fixture.connect().await;
    let mut stream = src.query("SELECT * FROM types_all", 10).await.unwrap();
    let schema = stream.schema();

    // DuckDB produces the Arrow, so the declared precision and scale are on the
    // field already. The PostgreSQL driver reaches the same answer by unpacking
    // a packed type modifier; here there is nothing to unpack and nothing to get
    // wrong.
    assert_eq!(
        field_type(&schema, "v_dec"),
        DataType::Decimal128(18, 6),
        "a decimal keeps its declared scale"
    );
    // Not squeezed into Int64, which would render the largest UBIGINT as -1.
    assert_eq!(field_type(&schema, "v_ubigint"), DataType::UInt64);
    // A UUID is a string here, not FixedSizeBinary(16). Surprising, and it is
    // what DuckDB's Arrow appender does: it casts to a plain string. The
    // PostgreSQL driver renders `Type::UUID` the same way, so the grid needs no
    // new case for it.
    assert_eq!(field_type(&schema, "v_uuid"), DataType::Utf8);
    // JSON is not a logical type of its own — it is a VARCHAR with an alias — so
    // Arrow cannot tell it from one. `columns()` is where that survives.
    assert_eq!(field_type(&schema, "v_json"), DataType::Utf8);
    assert_eq!(
        field_type(&schema, "v_time"),
        DataType::Time64(TimeUnit::Microsecond)
    );
    // The zone is dropped: a TIMETZ is indistinguishable from a TIME in Arrow.
    assert_eq!(
        field_type(&schema, "v_timetz"),
        DataType::Time64(TimeUnit::Microsecond)
    );

    let batch = stream.next_batch().await.unwrap().expect("one row");
    assert_eq!(
        col::<UInt64Array>(&batch, "v_ubigint").value(0),
        u64::MAX,
        "a UBIGINT that does not fit a BIGINT survives"
    );
    // The one lossy mapping in DuckDB's own table: UHUGEINT is handed over as a
    // *signed* Decimal128(38, 0), so everything above 2^127-1 comes back
    // negative. Asserted rather than hidden, because it is DuckDB's conversion
    // and not this driver's to fix.
    assert!(
        col::<Decimal128Array>(&batch, "v_uhugeint").value(0) < 0,
        "UHUGEINT above the signed range is DuckDB's own loss, and it is here"
    );
    // A HUGEINT is a Decimal128(38, 0) whose range is wider than 38 digits, so
    // the smallest one there is arrives intact.
    assert_eq!(
        col::<Decimal128Array>(&batch, "v_hugeint").value(0),
        i128::MIN
    );
    assert_eq!(
        col::<StringArray>(&batch, "v_varchar").value(0),
        "ünïcödé ✓ 漢字"
    );
}

#[tokio::test]
async fn a_nested_column_arrives_as_text_rather_than_as_a_format_string() {
    // The failure this exists to prevent: the Swift reader maps a closed set of
    // Arrow format strings and never follows `children`, so a STRUCT column
    // passed through reaches the grid as `<+s>` in every cell. DuckDB is the
    // driver most likely to produce one — none of these types is an extension.
    let fixture = Fixture::new(TYPES);
    let src = fixture.connect().await;
    let mut stream = src.query("SELECT * FROM types_all", 10).await.unwrap();
    let schema = stream.schema();

    for nested in [
        "v_list",
        "v_list_nested",
        "v_array",
        "v_struct",
        "v_map",
        "v_enum",
    ] {
        assert_eq!(
            field_type(&schema, nested),
            DataType::Utf8,
            "{nested} would otherwise be a format string in every cell"
        );
    }

    let batch = stream.next_batch().await.unwrap().expect("one row");
    assert_eq!(
        col::<StringArray>(&batch, "v_list").value(0),
        "[1, -1, 2147483647]"
    );
    assert_eq!(
        col::<StringArray>(&batch, "v_list_nested").value(0),
        "[[1], [2, 3]]"
    );
    assert_eq!(col::<StringArray>(&batch, "v_array").value(0), "[1, 2, 3]");
    assert_eq!(
        col::<StringArray>(&batch, "v_struct").value(0),
        "{qty: 2147483647, unit: kg}"
    );
    assert_eq!(col::<StringArray>(&batch, "v_map").value(0), "{a: 1, b: 2}");
    // An ENUM arrives as Dictionary(UInt8, Utf8), whose format string is the
    // index type — the reader would show `<C>` over a column of small integers.
    // Rendered, the labels come back.
    assert_eq!(col::<StringArray>(&batch, "v_enum").value(0), "archived");
}

#[tokio::test]
async fn a_rendered_column_still_says_what_the_database_declared() {
    // Arrow is lossy about DuckDB's type system in exactly the places the
    // structure pane notices, so the declared type comes from the catalog rather
    // than from the schema of the result.
    let fixture = Fixture::new(TYPES);
    let src = fixture.connect().await;
    let columns = src
        .columns(&schema("main"), "types_all")
        .await
        .expect("columns failed");
    let declared = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .data_type
            .clone()
    };

    assert_eq!(declared("v_dec"), "DECIMAL(18,6)");
    assert_eq!(declared("v_uuid"), "UUID");
    assert_eq!(declared("v_json"), "JSON");
    assert_eq!(declared("v_list"), "INTEGER[]");
    assert_eq!(declared("v_array"), "INTEGER[3]");
    assert_eq!(declared("v_struct"), "STRUCT(qty INTEGER, unit VARCHAR)");
    assert_eq!(declared("v_map"), "MAP(VARCHAR, INTEGER)");
    // The pair Arrow cannot tell apart at all.
    assert_eq!(declared("v_timetz"), "TIME WITH TIME ZONE");
    assert_eq!(declared("v_time"), "TIME");
}

#[tokio::test]
async fn a_variant_column_fails_with_something_to_do_about_it() {
    // Refused by the binding rather than by DuckDB, before a row moves, with a
    // message naming a Rust type. Restated so the user is told what to write
    // instead.
    //
    // In memory because a VARIANT column needs storage version v1.5.0, and a
    // file DuckDB creates for itself is written at v1.0.0 for compatibility.
    let src = DuckSource::connect(":memory:").await.unwrap();
    for statement in [
        "CREATE TABLE v (id INTEGER, payload VARIANT)",
        "INSERT INTO v VALUES (1, 42::VARIANT)",
    ] {
        let mut done = src.query(statement, 10).await.unwrap();
        while done.next_batch().await.unwrap().is_some() {}
    }

    let err = failure(&src, "SELECT * FROM v").await;
    let message = err.to_string();
    assert!(
        message.contains("CAST") && message.contains("VARCHAR"),
        "the message should say what to write instead, got: {message}"
    );
    assert!(!err.is_cancelled());

    // The rest of the table is readable, which is the point of saying which
    // column it was.
    let mut ok = src.query("SELECT id FROM v", 10).await.unwrap();
    assert_eq!(ok.next_batch().await.unwrap().expect("a row").num_rows(), 1);
}

// ---------------------------------------------------------------------------
// Streaming and paging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_batch_owns_its_buffers_after_the_database_is_gone() {
    // The claim "duckdb-rs is zero-copy to Arrow" is repeated widely and is
    // wrong as stated: `duckdb_data_chunk_to_arrow` converts a data chunk into
    // Arrow-layout buffers in C++, and the binding destroys the chunk before it
    // imports the array — a string column could not alias one anyway, since
    // DuckDB's `string_t` is a sixteen-byte value with an inlined prefix and
    // Arrow's is an offset into a shared buffer. What *is* zero-copy is
    // everything after that, and this is the property that matters: the batch
    // that comes out of the channel is self-contained, so the FFI layer can hand
    // it to Swift after the result and the connection are both gone.
    let fixture = Fixture::new(&counted(4_000));
    let src = fixture.connect().await;
    let mut stream = src.query("SELECT id, label FROM nums", 500).await.unwrap();
    let batch = stream.next_batch().await.unwrap().expect("a batch");
    drop(stream);
    drop(src);
    drop(fixture);

    assert_eq!(col::<Int64Array>(&batch, "id").value(0), 0);
    assert_eq!(col::<StringArray>(&batch, "label").value(499), "row-499");
}

#[tokio::test]
async fn a_batch_cut_down_to_size_still_crosses_the_ffi_boundary() {
    // `batch_rows` is honoured by slicing, and a slice is a view: its columns
    // keep the buffers of the whole chunk and carry an offset into them. The
    // Arrow C data interface says how to export that, and this is the check that
    // what this driver emits survives the trip — the Swift reader indexes each
    // column by its own offset, so a slice whose offset sat on a wrapper instead
    // of on the columns would show the first rows of the chunk on every page.
    let fixture = Fixture::new(&counted(4_000));
    let src = fixture.connect().await;
    let mut cursor = src.cursor("SELECT id, label FROM nums", 100).await.unwrap();
    cursor.fetch().await.unwrap().expect("the first page");
    let second = cursor.fetch().await.unwrap().expect("the second page");
    assert_eq!(col::<Int64Array>(&second, "id").value(0), 100);

    let exported = arrow::array::StructArray::from(second).into_data();
    let array = arrow::ffi::FFI_ArrowArray::new(&exported);
    let schema = arrow::ffi::FFI_ArrowSchema::try_from(exported.data_type()).unwrap();
    let imported = arrow::array::StructArray::from(unsafe {
        arrow::ffi::from_ffi(array, &schema).expect("a well-formed export")
    });
    let round_tripped = RecordBatch::from(&imported);
    assert_eq!(round_tripped.num_rows(), 100);
    assert_eq!(col::<Int64Array>(&round_tripped, "id").value(0), 100);
    assert_eq!(
        col::<StringArray>(&round_tripped, "label").value(99),
        "row-199"
    );
}

#[tokio::test]
async fn the_columns_are_known_before_the_first_row() {
    // PostgreSQL's contract, restored. DuckDB settles every column's type at
    // execution, so `query` resolves without a row having been read — where
    // SQLite has to wait for one, because a column's type there is not decided
    // until a value turns up. "Schema known before rows" is a fact about each
    // database and not about embedded versus server.
    let fixture = Fixture::new(&counted(5_000));
    let src = fixture.connect().await;
    let stream = src.query("SELECT id, label FROM nums", 100).await.unwrap();

    assert_eq!(stream.schema().fields().len(), 2);
    assert_eq!(field_type(&stream.schema(), "id"), DataType::Int64);
    // Zero is a real answer for a statement that affected nothing, so "not
    // finished" has to be something else.
    assert_eq!(stream.rows_affected(), None);
}

#[tokio::test]
async fn a_result_arrives_in_batches_of_the_size_that_was_asked_for() {
    let fixture = Fixture::new(&counted(5_000));
    let src = fixture.connect().await;
    let mut stream = src
        .query("SELECT id FROM nums ORDER BY id", 100)
        .await
        .unwrap();

    let mut seen = 0i64;
    let mut first_two = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        let ids = col::<Int64Array>(&batch, "id");
        for row in 0..batch.num_rows() {
            assert_eq!(ids.value(row), seen, "rows arrive once each, in order");
            seen += 1;
        }
        if first_two.len() < 2 {
            first_two.push(batch.num_rows());
        }
    }
    assert_eq!(first_two, [100, 100]);
    assert_eq!(seen, 5_000);
    assert_eq!(stream.rows_affected(), Some(5_000));
}

#[tokio::test]
async fn a_batch_never_grows_past_one_duckdb_chunk() {
    // The one place the shared `batch_rows` cannot be honoured. A DuckDB data
    // chunk is at most STANDARD_VECTOR_SIZE rows, which is a compile-time
    // constant the C API only reads back, so asking for more than that gets less
    // — reaching the asked-for size means concatenating, which copies every
    // buffer this path exists to avoid copying.
    let fixture = Fixture::new(&counted(10_000));
    let src = fixture.connect().await;
    let mut stream = src.query("SELECT id FROM nums", 1_000_000).await.unwrap();

    let mut largest = 0;
    let mut total = 0;
    while let Some(batch) = stream.next_batch().await.unwrap() {
        largest = largest.max(batch.num_rows());
        total += batch.num_rows();
    }
    assert_eq!(total, 10_000);
    assert!(
        largest <= 2048,
        "a batch is one data chunk, and got {largest} rows"
    );
    assert_eq!(largest, 2048, "and the chunk is full while there are rows");
}

#[tokio::test]
async fn paging_does_not_re_read_what_it_returned() {
    let fixture = Fixture::new(&counted(300_000));
    let src = fixture.connect().await;
    let mut cursor = src
        .cursor("SELECT id FROM nums ORDER BY id", 500)
        .await
        .unwrap();
    assert_eq!(cursor.schema().fields().len(), 1);

    let mut expected = 0i64;
    let mut pages = 0;
    while let Some(page) = cursor.fetch().await.unwrap() {
        let ids = col::<Int64Array>(&page, "id");
        for row in 0..page.num_rows() {
            assert_eq!(ids.value(row), expected, "no repeats and no gaps");
            expected += 1;
        }
        pages += 1;
    }
    assert_eq!(expected, 300_000);
    assert!(pages > 100, "the whole result did not arrive in one page");
}

#[tokio::test]
async fn a_page_agrees_with_the_page_before_it_across_a_write() {
    // What Phase 1 wanted from a cursor. DuckDB gets it from MVCC — a statement
    // reads the snapshot fixed when it began — rather than from a DECLARE or
    // from a read transaction held open, which is why there is no BEGIN here.
    let fixture = Fixture::new(&counted(50_000));
    let src = fixture.connect().await;
    let mut cursor = src
        .cursor("SELECT id FROM nums ORDER BY id", 1_000)
        .await
        .unwrap();
    let first = cursor.fetch().await.unwrap().expect("a first page");
    assert_eq!(first.num_rows(), 1_000);

    // Written through the driver, because a read-write DuckDB database has one
    // instance per file and a second connection of the test's own could not open
    // it.
    let mut write = src
        .query("INSERT INTO nums SELECT -1, 'late'", 1)
        .await
        .unwrap();
    while write.next_batch().await.unwrap().is_some() {}

    let mut seen = first.num_rows();
    while let Some(page) = cursor.fetch().await.unwrap() {
        let ids = col::<Int64Array>(&page, "id");
        for row in 0..page.num_rows() {
            assert!(ids.value(row) >= 0, "a row written after the cursor opened");
        }
        seen += page.num_rows();
    }
    assert_eq!(seen, 50_000, "the page count is the one the snapshot had");
}

#[tokio::test]
async fn a_write_reports_its_count_as_a_row_rather_than_as_a_number() {
    // The shape that differs from both other drivers, pinned rather than
    // described. DuckDB answers an INSERT with an ordinary one-row result whose
    // single `Count` column holds the number of rows written, so the count a
    // user wants is in the grid — and `rows_affected` is the rows this result
    // produced, which is one.
    //
    // Deliberately not guessed at from the shape: `SELECT count(*) AS "Count"`
    // produces exactly the same result, and a number that is right for writes
    // and silently wrong for that would be worse than one that always means the
    // same thing.
    let fixture = Fixture::new(&counted(10));
    let src = fixture.connect().await;
    let mut written = src
        .query("INSERT INTO nums SELECT i, 'late' FROM range(3) t(i)", 100)
        .await
        .unwrap();

    let batch = written.next_batch().await.unwrap().expect("the count row");
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(col::<Int64Array>(&batch, "Count").value(0), 3);
    assert!(written.next_batch().await.unwrap().is_none());
    assert_eq!(written.rows_affected(), Some(1));
}

#[tokio::test]
async fn closing_a_cursor_with_pages_left_in_it_is_allowed() {
    let fixture = Fixture::new(&counted(50_000));
    let src = fixture.connect().await;
    let mut cursor = src.cursor("SELECT id FROM nums", 100).await.unwrap();
    cursor.fetch().await.unwrap().expect("a first page");
    cursor.close().await.expect("close failed");
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// A statement that runs long enough to be interrupted, and produces nothing
/// until it finishes so that the interrupt lands during execution.
const SLOW_AGGREGATE: &str = "SELECT count(*) FROM range(20000000000) t(i) WHERE i % 7 = 3";

#[tokio::test]
async fn a_cancelled_statement_says_it_was_cancelled() {
    let fixture = Fixture::new(&counted(10));
    let src = fixture.connect().await;

    let (result, ()) = tokio::join!(src.query(SLOW_AGGREGATE, 100), async {
        // Long enough that the statement is certainly running: cancelling
        // something that has not started is a no-op, and this test is about the
        // other case.
        tokio::time::sleep(Duration::from_millis(300)).await;
        src.cancel();
    });

    let err = result
        .err()
        .expect("the statement should have been stopped");
    assert!(
        err.is_cancelled(),
        "a cancelled statement reads differently from a broken one: {err}"
    );

    // The interrupt belongs to the statement, not to the session: the next one
    // runs.
    let mut after = src.query("SELECT count(*) FROM nums", 10).await.unwrap();
    assert_eq!(
        col::<Int64Array>(&after.next_batch().await.unwrap().unwrap(), "count_star()").value(0),
        10
    );
}

#[tokio::test]
async fn a_cancelled_page_stops_the_fetch_rather_than_the_process() {
    // Also the test that would catch someone simplifying the pump back to the
    // crate's own `Arrow` iterator, which turns this exact failure into a
    // `panic!` — under `panic = "abort"` that takes the application with it, and
    // here it would end the reader thread silently and look like the end of the
    // result.
    let fixture = Fixture::new(&counted(2_000_000));
    let src = fixture.connect().await;
    let mut cursor = src
        .cursor("SELECT id, label FROM nums ORDER BY id", 100)
        .await
        .unwrap();
    cursor.fetch().await.unwrap().expect("a first page");

    let canceller = cursor.canceller();
    canceller.cancel();

    let mut pages = 0;
    let stopped = loop {
        match cursor.fetch().await {
            Ok(Some(_)) => {
                pages += 1;
                assert!(pages < 5_000, "the interrupt never landed");
            }
            Ok(None) => panic!("the result ended instead of being interrupted"),
            Err(e) => break e,
        }
    };
    assert!(stopped.is_cancelled(), "got: {stopped}");
}

#[tokio::test]
async fn cancelling_something_idle_is_not_a_failure() {
    // A front end's Cancel button must not report an error for being pressed at
    // a moment when nothing was running.
    let fixture = Fixture::new(&counted(10));
    let src = fixture.connect().await;
    src.cancel();

    let cursor = src.cursor("SELECT id FROM nums", 10).await.unwrap();
    cursor.canceller().cancel();
    src.cancel();
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_statement_broken_in_the_middle_says_where() {
    let fixture = Fixture::new("CREATE TABLE ünïcödé (id INTEGER);");
    let src = fixture.connect().await;

    // A non-ASCII identifier before the fault, because DuckDB's caret counts
    // single-byte characters and a driver that believed the column it printed
    // would put the caret four places early — inside the table name.
    let sql = "SELECT id FROM ünïcödé WHERE ORDER BY id";
    let err = failure(&src, sql).await;
    let position = err
        .statement_position()
        .expect("DuckDB prints a caret for a parse error");
    assert!(position >= 1, "positions count from one, got {position}");
    assert!(position as usize <= sql.chars().count() + 1);

    let at = sql
        .char_indices()
        .nth(position as usize - 1)
        .expect("inside the statement")
        .0;
    assert!(
        sql[at..].starts_with("ORDER"),
        "the caret should land on the word DuckDB named, and landed on {:?}",
        &sql[at..]
    );
    assert!(
        !err.is_cancelled(),
        "a broken statement is not a cancellation"
    );
}

#[tokio::test]
async fn a_query_that_fails_on_its_own_merits_is_not_a_cancellation() {
    let fixture = Fixture::new(&counted(10));
    let src = fixture.connect().await;
    let err = failure(&src, "SELECT no_such_column FROM nums").await;
    assert!(!err.is_cancelled());
    assert!(err.to_string().contains("no_such_column"));
}

#[tokio::test]
async fn a_fault_in_a_row_a_quarter_of_a_million_in_still_fails_at_query() {
    // Worth pinning because it is not what "streaming execution" suggests.
    // `duckdb_execute_prepared_streaming` does not stop at the first chunk: the
    // pipeline runs across DuckDB's own threads, so a row 250,000 in that will
    // not cast is found before `query` has returned. The trait allows either —
    // it says only that a successful `query` has not established that the
    // statement worked — and this driver turns out to be the strict one. The
    // path where a failure does arrive at a batch instead is cancellation, and
    // `a_cancelled_page_stops_the_fetch_rather_than_the_process` covers it.
    let fixture = Fixture::new(
        "CREATE TABLE mixed AS
             SELECT i AS id,
                    CASE WHEN i = 250000 THEN 'not a number' ELSE i::VARCHAR END AS label
             FROM range(300000) t(i);",
    );
    let src = fixture.connect().await;
    let err = failure(&src, "SELECT CAST(label AS INTEGER) AS n FROM mixed").await;
    assert!(
        !err.is_cancelled(),
        "a conversion error is not a cancellation"
    );
    assert!(err.to_string().contains("not a number"), "got {err}");
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_navigator_root_holds_the_users_own_schemas() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let names: Vec<String> = src
        .schemas()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();

    // `main` is the trap. `duckdb_schemas()` marks the user's own `main` as
    // internal — the same flag it uses for `pg_catalog` — so filtering on it
    // would return a navigator with everything the user made missing.
    assert!(names.contains(&schema("main")), "got {names:?}");
    assert!(names.contains(&schema("app")));
    // `system` holds information_schema and pg_catalog; `temp` holds one
    // connection's temporary objects, and every call here has a connection of
    // its own, so nothing could ever be under it.
    assert!(!names.iter().any(|n| n.starts_with("system.")), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("temp.")), "{names:?}");
}

#[tokio::test]
async fn a_view_is_not_reported_as_a_table() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let relations = src.relations(&schema("app")).await.unwrap();

    let orders = relations
        .iter()
        .find(|r| r.name == "orders")
        .expect("orders");
    assert_eq!(orders.kind, RelationKind::Table);
    assert_eq!(
        orders.schema,
        schema("app"),
        "a relation knows where it lives"
    );
    assert_eq!(orders.estimated_rows, Some(2));

    let view = relations
        .iter()
        .find(|r| r.name == "recent_orders")
        .expect("recent_orders");
    assert_eq!(view.kind, RelationKind::View);
    // A view has no row estimate. Reporting 0 would state something false, so it
    // declines to answer instead.
    assert_eq!(view.estimated_rows, None);
}

#[tokio::test]
async fn a_column_reports_its_place_its_type_and_whether_it_is_the_key() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let columns = src.columns(&schema("app"), "orders").await.unwrap();

    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(
            column.position,
            offset as i32 + 1,
            "column {} is out of position",
            column.name
        );
        assert!(!column.data_type.is_empty());
    }
    let id = &columns[0];
    assert_eq!(id.name, "id");
    assert!(id.is_primary_key, "the key comes from duckdb_constraints()");
    assert!(!id.nullable);

    let total = columns.iter().find(|c| c.name == "total").unwrap();
    assert_eq!(total.data_type, "DECIMAL(12,2)");
    assert!(!total.nullable);
    assert!(!total.is_primary_key);

    // A composite key marks every column in it.
    let regions = src.columns(&schema("app"), "regions").await.unwrap();
    let keyed: Vec<&str> = regions
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(keyed, ["country", "zone"]);
}

#[tokio::test]
async fn a_view_reports_the_whole_statement_it_was_created_from() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;

    let definition = src
        .definition(&schema("app"), "recent_orders")
        .await
        .unwrap()
        .expect("a view has a definition");
    // The whole CREATE VIEW, not just the body: DuckDB stores the statement. The
    // SQLite driver reports the same shape and PostgreSQL cannot, because
    // `pg_get_viewdef` renders the body back from a parse tree and has no
    // original text to return.
    assert!(definition.starts_with("CREATE VIEW"), "got {definition:?}");
    assert!(definition.contains("total > 100"));

    // A table is not a view, and the distinction is what the structure pane
    // hangs a section on.
    assert_eq!(
        src.definition(&schema("app"), "orders").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_index_list_holds_what_the_planner_can_use_and_not_the_key() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let indexes = src.indexes(&schema("app"), "orders").await.unwrap();

    let names: Vec<&str> = indexes.iter().map(|i| i.name.as_str()).collect();
    // Exactly the three CREATE INDEXes. DuckDB maintains the primary key and the
    // UNIQUE constraint with indexes too, and documents their details as living
    // in `duckdb_constraints()` instead — so this list can never show the key,
    // and `ColumnInfo::is_primary_key` is where a front end reads it.
    assert_eq!(
        names,
        [
            "orders_cust_ux",
            "orders_lower_country",
            "orders_placed_idx"
        ]
    );

    let unique = indexes.iter().find(|i| i.name == "orders_cust_ux").unwrap();
    assert!(unique.is_unique);
    assert_eq!(unique.columns, ["customer_id", "placed_at"]);
    assert!(
        !unique.is_primary,
        "DuckDB documents is_primary as always false"
    );
    assert_eq!(unique.method, "art");
    // DuckDB refuses `CREATE INDEX … WHERE …` outright, so there is no predicate
    // to have rather than one this driver could not find.
    assert_eq!(unique.predicate, None);

    // An index on `lower(ship_country)` is not an index on `ship_country`, and
    // printing it as one would misstate what the planner can use.
    let expression = indexes
        .iter()
        .find(|i| i.name == "orders_lower_country")
        .unwrap();
    assert_eq!(expression.columns.len(), 1);
    assert!(
        expression.columns[0].contains("lower(ship_country)"),
        "got {:?}",
        expression.columns
    );
}

#[tokio::test]
async fn a_composite_foreign_key_keeps_the_order_of_both_sides() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let keys = src.foreign_keys(&schema("app"), "orders").await.unwrap();
    assert_eq!(keys.len(), 2);

    let region = keys
        .iter()
        .find(|k| k.other_table == "regions")
        .expect("the composite key");
    // The third local column references the third foreign one, and
    // `duckdb_constraints()` hands both sides over as arrays already in
    // declaration order — where SQLite reports one column per row and has to be
    // regrouped.
    assert_eq!(region.local_columns, ["ship_country", "ship_zone"]);
    assert_eq!(region.other_columns, ["country", "zone"]);
    assert_eq!(region.other_schema, schema("app"));
    // Not blank and not invented: DuckDB has no referential actions at all.
    assert_eq!(region.on_update, "NO ACTION");
    assert_eq!(region.on_delete, "NO ACTION");
    // DuckDB records a name for every constraint and it is a name DuckDB made
    // up: `orders_region_fk` as declared is discarded and this comes back.
    assert!(!region.name.is_empty());
}

#[tokio::test]
async fn an_inbound_reference_is_named_for_the_table_that_was_asked_about() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let inbound = src
        .referenced_by(&schema("app"), "customers")
        .await
        .unwrap();

    assert_eq!(inbound.len(), 1);
    let key = &inbound[0];
    // Read swapped, so every field is named for `customers` rather than for the
    // table that declared the key.
    assert_eq!(key.local_columns, ["id"]);
    assert_eq!(key.other_table, "orders");
    assert_eq!(key.other_columns, ["customer_id"]);

    // A foreign key appears once, on the child. DuckDB keeps a mirrored entry on
    // the referenced table and `duckdb_constraints` skips it deliberately, so
    // this scan cannot double-count.
    assert!(
        src.foreign_keys(&schema("app"), "customers")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_check_constraint_reports_the_databases_own_text() {
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let constraints = src.constraints(&schema("app"), "orders").await.unwrap();

    // Where DuckDB beats SQLite outright: SQLite keeps a CHECK only inside the
    // CREATE TABLE text and its driver cannot report one at all.
    let check = constraints
        .iter()
        .find(|c| c.kind == dbconn::ConstraintKind::Check)
        .expect("a check constraint");
    assert!(check.definition.contains("total > 0"), "got {check:?}");

    // Neither key is here. Listing one in two places invites the reader to
    // wonder whether they are two different things.
    let kinds: Vec<_> = constraints.iter().map(|c| c.kind).collect();
    assert!(!kinds.contains(&dbconn::ConstraintKind::Other));

    // NOT NULL is a constraint object in DuckDB's catalog and a per-column
    // property in `columns()`. Reported in both places it would turn one fact
    // into two.
    assert!(
        !constraints
            .iter()
            .any(|c| c.definition.trim() == "NOT NULL"),
        "got {constraints:?}"
    );
    let total = src
        .columns(&schema("app"), "orders")
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.name == "total")
        .unwrap();
    assert!(!total.nullable, "and it is still reported once, here");

    let unique = src
        .constraints(&schema("app"), "customers")
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.kind == dbconn::ConstraintKind::Unique)
        .expect("a unique constraint");
    assert!(unique.definition.contains("email"));
}

#[tokio::test]
async fn a_table_with_none_of_these_features_answers_for_all_of_them() {
    // The case a driver is most likely to get wrong by failing instead of
    // answering empty.
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let bare = "notes";

    assert_eq!(src.columns(&schema("app"), bare).await.unwrap().len(), 1);
    assert!(src.indexes(&schema("app"), bare).await.unwrap().is_empty());
    assert!(
        src.foreign_keys(&schema("app"), bare)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        src.referenced_by(&schema("app"), bare)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        src.constraints(&schema("app"), bare)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(src.triggers(&schema("app"), bare).await.unwrap().is_empty());
    assert_eq!(src.definition(&schema("app"), bare).await.unwrap(), None);
}

#[tokio::test]
async fn a_relation_that_is_not_there_is_an_empty_answer() {
    // A navigator works from a tree that can be one refresh out of date, so this
    // happens in ordinary use and must not put an error on screen.
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    let missing = "no_such_relation_anywhere";

    for schema_name in [schema("app"), schema("nowhere"), "nonsense".to_string()] {
        assert!(src.columns(&schema_name, missing).await.unwrap().is_empty());
        assert!(src.indexes(&schema_name, missing).await.unwrap().is_empty());
        assert!(
            src.foreign_keys(&schema_name, missing)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            src.referenced_by(&schema_name, missing)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            src.constraints(&schema_name, missing)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            src.triggers(&schema_name, missing)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(src.definition(&schema_name, missing).await.unwrap(), None);
        assert!(src.relations(&schema_name).await.is_ok());
    }
}

#[tokio::test]
async fn triggers_are_empty_because_duckdb_has_none() {
    // Not a gap: DuckDB has no CREATE TRIGGER — the parser refuses the word —
    // no `duckdb_triggers()` and no `information_schema.triggers`. No query is
    // issued.
    let fixture = Fixture::new(CATALOG);
    let src = fixture.connect().await;
    assert!(
        src.triggers(&schema("app"), "orders")
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_in_memory_database_is_one_database_for_everything_on_it() {
    // `Connection::try_clone` rather than reopening the path, and this is the
    // test for why: a second `open_in_memory` is a different, empty database, so
    // a driver that reopened would answer every metadata call about a database
    // with nothing in it while the reader saw rows.
    let src = DuckSource::connect(":memory:")
        .await
        .expect("an in-memory database");
    let mut created = src
        .query(
            "CREATE TABLE scratch AS SELECT i AS id FROM range(2048) t(i)",
            10,
        )
        .await
        .unwrap();
    while created.next_batch().await.unwrap().is_some() {}

    let relations = src.relations("memory.main").await.unwrap();
    assert!(
        relations.iter().any(|r| r.name == "scratch"),
        "a metadata connection should see the same database, got {relations:?}"
    );
    let mut read = src
        .query("SELECT count(*) AS n FROM scratch", 10)
        .await
        .unwrap();
    let batch = read.next_batch().await.unwrap().expect("a row");
    assert_eq!(col::<Int64Array>(&batch, "n").value(0), 2048);
}

#[tokio::test]
async fn an_empty_path_is_the_in_memory_database_too() {
    // The registry hands over whatever followed the scheme, so `duckdb://` with
    // nothing after it arrives here as an empty string.
    let src = DuckSource::connect("")
        .await
        .expect("an in-memory database");
    assert!(
        src.schemas()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "memory.main")
    );
}

// ---------------------------------------------------------------------------
// Through the trait
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_trait_sees_everything_the_inherent_api_does() {
    // The shared contract check lives in `crates/conn/tests/contract.rs` and is
    // wired to each driver centrally; this is the same walk done here so the
    // trait impl is not merely compiled.
    let fixture = Fixture::new(&counted(500));
    let source = fixture.connect().await;
    let driver: &dyn Driver = &source;

    let mut stream = driver
        .query("SELECT id FROM nums ORDER BY id", 100)
        .await
        .expect("query failed");
    assert_eq!(stream.schema().fields().len(), 1);
    assert_eq!(stream.rows_affected(), None);
    assert_eq!(
        stream
            .next_batch()
            .await
            .unwrap()
            .expect("a batch")
            .num_rows(),
        100
    );

    let mut cursor = driver
        .cursor("SELECT id FROM nums ORDER BY id", 50)
        .await
        .expect("cursor failed");
    assert_eq!(cursor.schema().fields().len(), 1);
    for page in 1..=3 {
        let batch = cursor
            .fetch()
            .await
            .expect("fetch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        assert_eq!(batch.num_rows(), 50);
    }
    cursor.canceller().cancel().await.expect("cancel failed");
    cursor.close().await.expect("close failed");
    driver.cancel().await.expect("session cancel failed");

    let root = schema("main");
    assert!(
        driver
            .schemas()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == root)
    );
    let relations = driver.relations(&root).await.unwrap();
    let found = relations.iter().find(|r| r.name == "nums").expect("nums");
    assert_eq!(found.schema, root);
    assert!(!driver.columns(&root, "nums").await.unwrap().is_empty());
    assert_eq!(driver.definition(&root, "nums").await.unwrap(), None);
    driver.indexes(&root, "nums").await.expect("indexes failed");
    driver
        .foreign_keys(&root, "nums")
        .await
        .expect("foreign keys failed");
    driver
        .referenced_by(&root, "nums")
        .await
        .expect("inbound references failed");
    driver
        .constraints(&root, "nums")
        .await
        .expect("constraints failed");
    driver
        .triggers(&root, "nums")
        .await
        .expect("triggers failed");

    // A failure through the trait keeps both facts a front end acts on.
    let Err(err) = driver
        .query("SELECT id FROM nums WHERE ORDER BY id", 10)
        .await
    else {
        panic!("a broken statement should not prepare");
    };
    assert!(err.statement_position().is_some(), "got {err}");
    assert!(!err.is_cancelled());
}
