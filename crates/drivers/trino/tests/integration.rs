//! The Trino driver against a live coordinator.
//!
//! Marked `ignore`, so `cargo test` passes with nothing installed. To run them:
//!
//! ```text
//! make db-up-trino
//! cargo test -p driver-trino -- --ignored
//! make db-down-trino
//! ```
//!
//! The fixture is applied through this driver, which is the ClickHouse suite's
//! arrangement rather than the Cassandra one's, and for the ClickHouse reason: a
//! driver that cannot run `CREATE TABLE` has not been exercised by anything that
//! only reads, so seeding is the first check in the file. The contract suite's
//! Trino subject is seeded the other way — over a plain HTTP client — because
//! there the fixture must not be vouched for by the code under test.
//!
//! Two of these tests reach past the driver and speak the protocol directly, and
//! both are pinning a measurement the driver's own API cannot express:
//! `a_write_inside_a_transaction_is_refused_by_the_only_writable_catalog` needs
//! the `X-Trino-Transaction-Id` header this driver deliberately does not send,
//! and it is the evidence `Driver::transactional` rests on.
//!
//! **What this suite cannot cover.** There is no materialized view anywhere in
//! it, because no connector a stock coordinator ships can create one — Iceberg
//! and Hive can, and both want a metastore. `relation_kind` therefore reports
//! `Unknown` for whatever string one turns out to have, and says so rather than
//! guessing `Table`.

use arrow::array::{
    Array, BinaryArray, Decimal128Array, Float64Array, Int64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use arrow::datatypes::DataType;
use bytes::Bytes;
use dbconn::{Browse, DbError, Driver, RelationKind, TxStep};
use driver_trino::TrinoSource;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

const ORIGIN: &str = "http://127.0.0.1:58080";
/// A schema of this suite's own. The contract suite uses another one, for the
/// reason `mysql()` gives there: `cargo test --workspace -- --ignored` runs both
/// binaries at once, and a shared fixture would turn a scheduling accident into
/// a contract violation.
const SCHEMA: &str = "memory.dbclient_trino";
const URL: &str = "http://127.0.0.1:58080/memory/dbclient_trino";

/// How many rows the shared fixture table holds.
const ROWS: usize = 500;

/// A statement that takes long enough to still be running when a cancel lands.
///
/// `sf1000` is generated on demand rather than stored, so this is six billion
/// rows the coordinator has to produce before it can count them. Nothing is
/// written and nothing is kept.
const SLOW: &str = "SELECT count(*) FROM tpch.sf1000.lineitem";

/// Applied once per test binary, however many tests want it.
static FIXTURE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn source() -> TrinoSource {
    FIXTURE.get_or_init(seed).await;
    TrinoSource::connect(URL)
        .await
        .expect("Trino unreachable; see the header of this file")
}

/// Builds the fixture, through the driver, one statement at a time.
async fn seed() {
    let admin = TrinoSource::connect("http://127.0.0.1:58080/memory")
        .await
        .expect("Trino unreachable; see the header of this file");
    for statement in [
        format!("CREATE SCHEMA IF NOT EXISTS {SCHEMA}"),
        // The view first: it is the one that depends on the table.
        format!("DROP VIEW IF EXISTS {SCHEMA}.nums_view"),
        format!("DROP TABLE IF EXISTS {SCHEMA}.nums"),
        format!(
            "CREATE TABLE {SCHEMA}.nums AS \
             SELECT id, 'row-' || CAST(id AS varchar) AS label \
             FROM UNNEST(sequence(1, {ROWS})) AS t(id)"
        ),
        format!("CREATE VIEW {SCHEMA}.nums_view AS SELECT id, label FROM {SCHEMA}.nums"),
    ] {
        run(&admin, &statement).await;
    }
}

/// Runs a statement for its effect, reading it to the end.
async fn run(src: &TrinoSource, sql: &str) {
    let mut rows = src
        .query(sql, 1)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    while rows
        .next_page()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .is_some()
    {}
}

/// Reads `nums` in ascending order.
fn read() -> String {
    format!("SELECT id FROM {SCHEMA}.nums ORDER BY id")
}

/// Everything one statement produces, in one batch.
async fn read_all(src: &TrinoSource, sql: &str) -> RecordBatch {
    let mut rows = src
        .query(sql, 4096)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let schema = rows.schema();
    let mut batches = Vec::new();
    while let Some(batch) = rows
        .next_page()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        batches.push(batch);
    }
    arrow::compute::concat_batches(&schema, &batches).expect("concat failed")
}

/// Every `id` a result produced, in the order it produced them.
fn ids(batch: &RecordBatch) -> Vec<i64> {
    let column = batch
        .column_by_name("id")
        .expect("id")
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("id is an Int64");
    (0..batch.num_rows()).map(|row| column.value(row)).collect()
}

fn kind_of(batch: &RecordBatch, column: &str) -> DataType {
    batch
        .schema()
        .field_with_name(column)
        .unwrap_or_else(|_| panic!("no column called {column}"))
        .data_type()
        .clone()
}

fn text(batch: &RecordBatch, column: &str) -> String {
    batch
        .column_by_name(column)
        .expect(column)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| panic!("{column} is not text"))
        .value(0)
        .to_string()
}

/// The failure `sql` produces, insisting there is one.
///
/// Read to the end rather than off the first page, because Trino has two kinds
/// of failure and only one of them is ready when the call returns: a statement
/// it will not plan fails immediately, and a statement that breaks while it is
/// running fails on whichever page the split that broke was feeding.
async fn failure(src: &TrinoSource, sql: &str) -> DbError {
    let mut stream = match Driver::query(src, sql, 10).await {
        Err(e) => return e,
        Ok(stream) => stream,
    };
    loop {
        match stream.next_batch().await {
            Err(e) => return e,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("expected this to fail: {sql}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn reads_a_result_in_batches_of_the_size_asked_for() {
    let src = source().await;
    let mut stream = src.query(&read(), 100).await.expect("query");
    // Before a single row has been read: a front end lays out a grid first and
    // asks for rows afterwards, and the protocol has no DESCRIBE to ask.
    assert_eq!(stream.schema().fields().len(), 1);
    assert_eq!(stream.rows_affected(), None, "zero is a real answer");

    let mut seen = 0;
    while let Some(batch) = stream.next_page().await.expect("batch") {
        assert!(batch.num_rows() <= 100);
        seen += batch.num_rows();
    }
    assert_eq!(seen, ROWS);
    assert_eq!(stream.rows_affected(), Some(ROWS as u64));
}

/// The property that separates this driver's carry from the other two.
///
/// Trino chunks a result by bytes, so the page size is never the caller's:
/// `SELECT orderkey FROM tpch.tiny.orders` arrives as *one* chunk of 15000 rows.
/// The carry has to cut that up, where the ClickHouse and Cassandra carries only
/// ever had to join pieces together. Asked for 137 — deliberately not a divisor
/// of 15000 — every batch but the last has to be exactly that.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_chunk_the_coordinator_sized_by_bytes_is_cut_to_the_size_that_was_asked_for() {
    let src = source().await;
    let mut stream = src
        .query(
            "SELECT orderkey FROM tpch.tiny.orders ORDER BY orderkey",
            137,
        )
        .await
        .expect("query");
    let mut sizes = Vec::new();
    while let Some(batch) = stream.next_page().await.expect("batch") {
        sizes.push(batch.num_rows());
    }
    assert_eq!(sizes.iter().sum::<usize>(), 15_000);
    assert_eq!(sizes.last(), Some(&(15_000 % 137)), "the remainder is last");
    assert!(
        sizes[..sizes.len() - 1].iter().all(|n| *n == 137),
        "every page but the last is the size that was asked for, got {sizes:?}"
    );
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Every type mapped in `arrow_map`, against the wire encoding it actually
/// arrives in.
///
/// Literals rather than a table, because the memory connector's own type support
/// is not what is under test here — the mapping from Trino's JSON to Arrow is.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn each_kind_of_value_arrives_as_the_type_that_was_decided_for_it() {
    use arrow::datatypes::TimeUnit;
    let src = source().await;
    let batch = read_all(&src, KINDS).await;

    assert_eq!(kind_of(&batch, "c_boolean"), DataType::Boolean);
    // Arrow has an Int8 and the reader on the far side of the FFI does not.
    assert_eq!(kind_of(&batch, "c_tinyint"), DataType::Int16);
    assert_eq!(kind_of(&batch, "c_smallint"), DataType::Int16);
    assert_eq!(kind_of(&batch, "c_integer"), DataType::Int32);
    assert_eq!(kind_of(&batch, "c_bigint"), DataType::Int64);
    assert_eq!(kind_of(&batch, "c_real"), DataType::Float32);
    assert_eq!(kind_of(&batch, "c_double"), DataType::Float64);
    assert_eq!(kind_of(&batch, "c_decimal"), DataType::Decimal128(18, 2));
    assert_eq!(kind_of(&batch, "c_decimal38"), DataType::Decimal128(38, 3));
    assert_eq!(kind_of(&batch, "c_varchar"), DataType::Utf8);
    assert_eq!(kind_of(&batch, "c_varbinary"), DataType::Binary);
    assert_eq!(kind_of(&batch, "c_date"), DataType::Date32);
    assert_eq!(
        kind_of(&batch, "c_time6"),
        DataType::Time64(TimeUnit::Microsecond)
    );
    assert_eq!(
        kind_of(&batch, "c_ts6"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    // The deliberate flattenings, each for a reason recorded in `arrow_map`.
    for wider_than_arrow in ["c_time9", "c_ts9", "c_tstz", "c_timetz"] {
        assert_eq!(
            kind_of(&batch, wider_than_arrow),
            DataType::Utf8,
            "{wider_than_arrow} should be text"
        );
    }
    for no_arrow_home in [
        "c_uuid", "c_ip", "c_json", "c_int_ym", "c_int_ds", "c_array", "c_map", "c_row",
    ] {
        assert_eq!(
            kind_of(&batch, no_arrow_home),
            DataType::Utf8,
            "{no_arrow_home} should be text"
        );
    }
}

/// The values themselves, for the ones whose rendering had to be decided rather
/// than borrowed.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn the_values_that_had_to_be_rendered_by_hand_come_back_exact() {
    let src = source().await;
    let batch = read_all(&src, KINDS).await;

    // A JSON string is taken as it is; a JSON structure is rendered as JSON.
    // Quoting the first group would put `"8e14e760-…"` in a grid cell.
    assert_eq!(
        text(&batch, "c_uuid"),
        "8e14e760-7fa8-11eb-bc66-000000000001"
    );
    assert_eq!(text(&batch, "c_ip"), "2001:db8::1");
    assert_eq!(text(&batch, "c_json"), r#"{"a":[1,2]}"#);
    assert_eq!(text(&batch, "c_array"), "[1,2,3]");
    assert_eq!(text(&batch, "c_map"), r#"{"a":1,"b":2}"#);
    assert_eq!(text(&batch, "c_row"), r#"[7,"seven"]"#);
    assert_eq!(text(&batch, "c_int_ym"), "0-3");
    assert_eq!(text(&batch, "c_int_ds"), "2 00:00:00.000");
    // The zone travels with the value in Trino and with the column in Arrow,
    // which is why this is text: two rows of one column can be in two zones.
    assert_eq!(
        text(&batch, "c_tstz"),
        "2024-01-15 12:34:56.123 Asia/Taipei"
    );

    // `char(n)` arrives padded, and the padding is part of the value rather than
    // something to trim: it is what `char` comparison is built on.
    assert_eq!(text(&batch, "c_char"), "abc  ");

    // Thirty-eight digits, exact, in a type Arrow can hold — Trino's maximum
    // decimal precision is 38 and so is Decimal128's, which is why the mapping
    // never has to fall back to text for a number.
    let wide = batch
        .column_by_name("c_decimal38")
        .expect("c_decimal38")
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .expect("a decimal");
    assert_eq!(wide.value(0), 12345678901234567890123456789012345678i128);

    // Trino sends varbinary base64-encoded, so this is the one type whose value
    // is not simply what the JSON said.
    let raw = batch
        .column_by_name("c_varbinary")
        .expect("c_varbinary")
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("binary");
    assert_eq!(raw.value(0), &[0x00, 0xff]);

    // A NULL is a null and not an empty string.
    let null = batch.column_by_name("c_null").expect("c_null");
    assert!(null.is_null(0));
}

/// The header that decides whether a stored value survives the round trip.
///
/// Without `X-Trino-Client-Capabilities: PARAMETRIC_DATETIME` the coordinator
/// rewrites every `timestamp(p)` and `time(p)` to precision 3 *and reports the
/// type without its precision* — so `timestamp(9)` comes back as `timestamp`
/// holding three digits, and nothing on this side can tell that from a column
/// that was stored with three. This is what that header buys, asserted rather
/// than trusted: nine digits arrive, and the type says nine.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_datetime_keeps_every_digit_it_was_stored_with() {
    let src = source().await;
    let batch = read_all(&src, KINDS).await;
    assert_eq!(text(&batch, "c_ts9"), "2024-01-15 12:34:56.123456789");
    assert_eq!(text(&batch, "c_time9"), "12:34:56.123456789");

    // And the microsecond half, which does fit and therefore is not text: the
    // same value read back out of Arrow.
    let ts = batch
        .column_by_name("c_ts6")
        .expect("c_ts6")
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .expect("a timestamp");
    assert_eq!(ts.value(0), 1_705_322_096_123_456);
}

/// The two values a `double` can hold that JSON has no spelling for. Trino sends
/// them as the strings `"NaN"` and `"Infinity"`, so a driver reading `as_f64()`
/// alone would refuse an ordinary aggregate over an empty group.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_double_that_is_not_a_number_still_arrives_as_a_double() {
    let src = source().await;
    let batch = read_all(
        &src,
        "SELECT nan() AS c_nan, infinity() AS c_inf, -infinity() AS c_ninf",
    )
    .await;
    let value = |name: &str| {
        batch
            .column_by_name(name)
            .expect(name)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("a double")
            .value(0)
    };
    assert!(value("c_nan").is_nan());
    assert_eq!(value("c_inf"), f64::INFINITY);
    assert_eq!(value("c_ninf"), f64::NEG_INFINITY);
}

/// One statement holding every type this driver maps, so that the three tests
/// above read the same result rather than three different ones.
const KINDS: &str = "SELECT \
  true AS c_boolean, \
  CAST(7 AS tinyint) AS c_tinyint, \
  CAST(300 AS smallint) AS c_smallint, \
  CAST(70000 AS integer) AS c_integer, \
  CAST(9223372036854775807 AS bigint) AS c_bigint, \
  CAST(1.5 AS real) AS c_real, \
  CAST(2.5 AS double) AS c_double, \
  CAST('1234.56' AS decimal(18,2)) AS c_decimal, \
  CAST('12345678901234567890123456789012345.678' AS decimal(38,3)) AS c_decimal38, \
  CAST('hello' AS varchar) AS c_varchar, \
  CAST('abc' AS char(5)) AS c_char, \
  X'00ff' AS c_varbinary, \
  DATE '2024-01-15' AS c_date, \
  TIME '12:34:56.123456' AS c_time6, \
  TIME '12:34:56.123456789' AS c_time9, \
  TIME '12:34:56.123456+08:00' AS c_timetz, \
  TIMESTAMP '2024-01-15 12:34:56.123456' AS c_ts6, \
  TIMESTAMP '2024-01-15 12:34:56.123456789' AS c_ts9, \
  TIMESTAMP '2024-01-15 12:34:56.123 Asia/Taipei' AS c_tstz, \
  INTERVAL '3' MONTH AS c_int_ym, \
  INTERVAL '2' DAY AS c_int_ds, \
  ARRAY[1, 2, 3] AS c_array, \
  MAP(ARRAY['a','b'], ARRAY[1,2]) AS c_map, \
  CAST(ROW(7, 'seven') AS row(n integer, w varchar)) AS c_row, \
  JSON '{\"a\": [1,2]}' AS c_json, \
  UUID '8e14e760-7fa8-11eb-bc66-000000000001' AS c_uuid, \
  IPADDRESS '2001:db8::1' AS c_ip, \
  CAST(NULL AS varchar) AS c_null";

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Three answers, because Trino gives three: a read counts what it produced, a
/// write reports what it changed, and a DDL statement carries neither.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_write_reports_the_rows_it_changed_and_a_ddl_reports_none() {
    let src = source().await;
    let table = format!("{SCHEMA}.written");
    run(&src, &format!("DROP TABLE IF EXISTS {table}")).await;

    let mut ddl = src
        .query(&format!("CREATE TABLE {table} (n integer)"), 10)
        .await
        .expect("create");
    assert_eq!(ddl.schema().fields().len(), 0, "a DDL has no columns");
    assert!(ddl.next_page().await.expect("drain").is_none());
    assert_eq!(
        ddl.rows_affected(),
        None,
        "it did something and how much is not a number Trino has"
    );

    let mut write = src
        .query(&format!("INSERT INTO {table} VALUES (1), (2), (3)"), 10)
        .await
        .expect("insert");
    while write.next_page().await.expect("drain").is_some() {}
    assert_eq!(
        write.rows_affected(),
        Some(3),
        "rows changed, which is a better number than a counted result"
    );

    let batch = read_all(&src, &format!("SELECT n FROM {table} ORDER BY n")).await;
    assert_eq!(batch.num_rows(), 3);
    run(&src, &format!("DROP TABLE {table}")).await;
}

// ---------------------------------------------------------------------------
// Cursors and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn pages_a_cursor_without_repeating_or_skipping() {
    let src = source().await;
    let mut cursor = src.cursor(&read(), 50).await.expect("cursor");
    assert!(cursor.schema().field_with_name("id").is_ok());

    let mut seen: Vec<i64> = Vec::new();
    while let Some(batch) = cursor.next_page().await.expect("fetch") {
        seen.extend(ids(&batch));
    }
    assert_eq!(seen.len(), ROWS, "every row once");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "in order, with nothing repeated"
    );
    cursor.close().await.expect("close");
}

/// A cancel arriving while a fetch is parked on the socket.
///
/// The coordinator answers the parked `GET` as soon as the `DELETE` lands rather
/// than making it wait out its poll, which is what makes half a second enough
/// here — measured at 0.55s from cancel to the reader seeing `USER_CANCELED`.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_cancel_during_a_fetch_stops_it_where_it_is() {
    let src = source().await;
    let mut cursor = src.cursor(SLOW, 10).await.expect("cursor");
    let canceller = cursor.canceller();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await.expect("cancel");
    });

    let error = loop {
        match cursor.next_page().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("six billion rows were counted before the cancel landed"),
            Err(e) => break e,
        }
    };
    assert!(
        error.is_cancelled(),
        "a cancelled query should say so, got: {error}"
    );
}

/// The other half of the same claim: a statement that broke on its own must not
/// be reported as somebody's button working.
///
/// A fault raised while rows are being produced rather than while the statement
/// is being parsed, which is the case that arrives through the same field as a
/// cancellation and is told apart from one only by the code.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_query_that_fails_while_it_runs_is_not_a_cancellation() {
    let src = source().await;
    let error = failure(
        &src,
        "SELECT if(orderkey > 0, fail('deliberate'), 1) FROM tpch.tiny.orders",
    )
    .await;
    assert!(!error.is_cancelled());
    assert!(error.to_string().contains("deliberate"), "got: {error}");
}

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn cancelling_an_idle_cursor_is_not_a_failure() {
    // Delivery is not interruption: pressing Cancel when nothing is running has
    // to succeed, or a front end reports a failure for pressing a button at the
    // wrong moment. Trino answers `204` for a query it has already forgotten,
    // which is what makes this true rather than merely tolerated.
    let src = source().await;
    let cursor = src.cursor(&read(), 10).await.expect("cursor");
    cursor.canceller().cancel().await.expect("idle cursor");
    src.cancel().await.expect("idle session");
}

/// The trait says a session cancel does not reach a cursor, and here that is a
/// fact about which statements the session registered rather than something
/// remembered.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_session_cancel_does_not_reach_a_cursor() {
    let src = source().await;
    let mut cursor = src.cursor(&read(), 10).await.expect("cursor");
    src.cancel().await.expect("session cancel");
    let batch = cursor
        .next_page()
        .await
        .expect("the cursor is not the session's to cancel")
        .expect("a page");
    assert_eq!(batch.num_rows(), 10);
}

/// A cancelled session takes the statements it started with it, and leaves the
/// next one alone.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_session_cancel_stops_its_own_statements_and_not_the_next_one() {
    let src = source().await;
    let mut stream = src.query(SLOW, 10).await.expect("query");
    src.cancel().await.expect("cancel");
    let error = loop {
        match stream.next_page().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the statement finished before the cancel landed"),
            Err(e) => break e,
        }
    };
    assert!(error.is_cancelled(), "got: {error}");

    let mut after = src.query(&read(), 10).await.expect("a new statement");
    assert!(
        after.next_page().await.expect("not cancelled").is_some(),
        "a statement started after Cancel is not the one that was cancelled"
    );
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

/// Trino counts the column in code points, which is the one place this driver is
/// simpler than the ClickHouse and Cassandra ones rather than more complicated.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_broken_statement_says_where_it_stopped() {
    let src = source().await;
    for broken in [
        format!("SELECT id FROM {SCHEMA}.nums WHERE ORDER BY id"),
        // The same fault with six three-byte characters ahead of it. A byte
        // offset applied as a character one would land twelve characters late.
        format!("SELECT \"漢字漢字漢字\" FROM {SCHEMA}.nums WHERE ORDER BY id"),
        // And with seven characters outside the basic plane, where a UTF-16
        // count would part company from a code-point one.
        format!("SELECT \"𝔘𝔫𝔦𝔠𝔬𝔡𝔢\" FROM {SCHEMA}.nums WHERE ORDER BY id"),
    ] {
        let error = failure(&src, &broken).await;
        let at = error
            .statement_position()
            .unwrap_or_else(|| panic!("Trino reports a position: {error}"))
            as usize;
        assert!(at >= 1, "positions count from one, got {at}");
        assert!(at <= broken.chars().count() + 1, "past the end: {at}");
        assert_eq!(
            broken.chars().nth(at - 1),
            Some('O'),
            "the caret should land on ORDER: {broken}"
        );
        assert!(!error.is_cancelled(), "a broken statement is not a Cancel");
    }
}

/// A fault on a later line, which is where the line arithmetic earns its place.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_fault_on_the_second_line_is_reported_on_the_second_line() {
    let src = source().await;
    let broken = format!("SELECT id FROM {SCHEMA}.nums\nWHERE ORDER BY id");
    let error = failure(&src, &broken).await;
    let at = error.statement_position().expect("a position") as usize;
    assert_eq!(broken.chars().nth(at - 1), Some('O'), "got {at}");
}

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn reading_a_table_that_is_not_there_is_a_failure_with_the_coordinators_own_words() {
    let src = source().await;
    let error = failure(
        &src,
        &format!("SELECT * FROM {SCHEMA}.no_such_relation_anywhere"),
    )
    .await;
    assert!(error.to_string().contains("does not exist"), "got: {error}");
    // Trino points at the name rather than declining, which is PostgreSQL's
    // behaviour and not SQLite's; the contract allows either.
    assert!(error.statement_position().is_some(), "got: {error}");
}

/// A statement typed against a session with a catalog and schema behind it
/// resolves an unqualified name — which is the whole point of sending
/// `X-Trino-Catalog` and `X-Trino-Schema` on every request.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn an_unqualified_name_resolves_in_the_catalog_the_url_named() {
    let src = source().await;
    assert_eq!(src.catalog(), "memory");
    assert_eq!(src.schema(), "dbclient_trino");
    let batch = read_all(&src, "SELECT count(*) AS n FROM nums").await;
    assert_eq!(batch.num_rows(), 1);

    // And a connection that names no schema cannot: the coordinator says so
    // rather than guessing.
    let bare = TrinoSource::connect("http://127.0.0.1:58080/memory")
        .await
        .expect("connect");
    let error = failure(&bare, "SELECT count(*) FROM nums").await;
    assert!(
        error.to_string().contains("Schema must be specified"),
        "got: {error}"
    );
}

/// A catalog that is not there is caught by `connect` rather than by the first
/// table somebody clicks. Trino will not do this for free — a `SELECT 1` sent
/// with a nonexistent catalog header succeeds, because nothing in it resolves a
/// name.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn connecting_to_a_catalog_that_is_not_there_fails_at_connect() {
    let error = TrinoSource::connect(&format!("{ORIGIN}/no_such_catalog"))
        .await
        .err()
        .expect("there is no such catalog");
    assert!(
        error.to_string().contains("no_such_catalog"),
        "got: {error}"
    );
}

/// The statement a navigator writes for a table is one this coordinator runs,
/// with all three levels of the name in it.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_browse_names_all_three_levels_and_runs() {
    let src = source().await;
    let keys = ["id".to_string()];
    let statement = src.browse(&Browse {
        schema: SCHEMA,
        relation: "nums",
        filter: None,
        order: None,
        keys: &keys,
        limit: Some(3),
    });
    assert_eq!(
        statement,
        "SELECT * FROM memory.dbclient_trino.nums ORDER BY id LIMIT 3"
    );
    let batch = read_all(&src, &statement).await;
    assert_eq!(batch.num_rows(), 3);
    assert!(
        batch.schema().field_with_name("label").is_ok(),
        "every column"
    );
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// The navigator root, which is `catalog.schema` because Trino has a level the
/// trait does not.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn the_navigator_root_carries_both_levels_trino_has() {
    let src = source().await;
    let schemas = src.schemas().await.expect("schemas");
    let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&SCHEMA), "got: {names:?}");
    assert!(names.contains(&"tpch.tiny"));
    // The catalogs that answer every other question on this page are listed
    // rather than hidden — see `metadata.rs`.
    assert!(names.contains(&"system.runtime"));
    assert!(names.contains(&"tpch.information_schema"));
    // A schema name that carried no catalog would be a name nothing can open.
    assert!(names.iter().all(|name| name.contains('.')), "{names:?}");
}

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_view_is_not_a_table_and_carries_the_body_it_was_defined_by() {
    let src = source().await;
    let relations = src.relations(SCHEMA).await.expect("relations");
    let kind = |name: &str| {
        relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed, got {relations:?}"))
            .kind
    };
    assert_eq!(kind("nums"), RelationKind::Table);
    assert_eq!(kind("nums_view"), RelationKind::View);
    assert_eq!(
        relations.iter().find(|r| r.name == "nums").unwrap().schema,
        SCHEMA,
        "a relation knows where it lives"
    );
    // Nothing has measured this, and saying so is not the same as saying zero.
    assert_eq!(
        relations
            .iter()
            .find(|r| r.name == "nums")
            .unwrap()
            .estimated_rows,
        None
    );

    // A table is not a view, which is what the structure pane hangs a section on.
    assert_eq!(src.definition(SCHEMA, "nums").await.unwrap(), None);
    let body = src
        .definition(SCHEMA, "nums_view")
        .await
        .unwrap()
        .expect("a view has a definition");
    assert!(body.contains("nums"), "got: {body}");
    assert!(
        !body.to_uppercase().contains("CREATE VIEW"),
        "the body, not the whole statement: {body}"
    );
}

/// Columns are one-based and carry Trino's own spelling of the type, which is
/// not the Arrow type the values arrive as.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn columns_are_numbered_from_one_and_state_the_declared_type() {
    let src = source().await;
    let columns = src.columns("tpch.tiny", "orders").await.expect("columns");
    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(column.position, offset as i32 + 1, "{}", column.name);
        assert!(!column.data_type.is_empty(), "a column states its own type");
        // False for every column of every Trino table: `PRIMARY KEY` is not in
        // the `CREATE TABLE` grammar.
        assert!(!column.is_primary_key, "{}", column.name);
    }
    let declared = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .data_type
            .clone()
    };
    assert_eq!(declared("orderkey"), "bigint");
    assert_eq!(declared("orderpriority"), "varchar(15)");
    assert_eq!(declared("orderdate"), "date");
    assert_eq!(declared("totalprice"), "double");
}

/// Five calls that answer without asking, because Trino has nothing to ask
/// about. `metadata.rs` quotes the grammar and the catalog refusing each.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn the_calls_with_no_answer_are_empty_rather_than_broken() {
    let src = source().await;
    assert!(src.indexes(SCHEMA, "nums").await.unwrap().is_empty());
    assert!(src.foreign_keys(SCHEMA, "nums").await.unwrap().is_empty());
    assert!(src.referenced_by(SCHEMA, "nums").await.unwrap().is_empty());
    assert!(src.constraints(SCHEMA, "nums").await.unwrap().is_empty());
    assert!(src.triggers(SCHEMA, "nums").await.unwrap().is_empty());
}

/// The evidence those five rest on, run against the coordinator so that a Trino
/// which grows any of them turns this into a failing test rather than five
/// comments nobody rereads.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn trino_has_nowhere_to_read_a_key_an_index_or_a_trigger_from() {
    let src = source().await;
    for absent in [
        "table_constraints",
        "key_column_usage",
        "referential_constraints",
        "check_constraints",
        "statistics",
        "triggers",
    ] {
        let error = failure(
            &src,
            &format!("SELECT * FROM tpch.information_schema.{absent}"),
        )
        .await;
        assert!(
            error.to_string().contains("does not exist"),
            "information_schema.{absent} should not exist, got: {error}"
        );
    }
    for refused in [
        format!("CREATE INDEX i ON {SCHEMA}.nums (id)"),
        format!("CREATE TRIGGER t BEFORE INSERT ON {SCHEMA}.nums EXECUTE f"),
        format!("CREATE TABLE {SCHEMA}.keyed (n integer PRIMARY KEY)"),
    ] {
        let error = failure(&src, &refused).await;
        assert!(
            error.to_string().contains("mismatched input"),
            "{refused} should be a syntax error, got: {error}"
        );
    }
}

/// A navigator works from a tree that can be one refresh out of date, so asking
/// about something that is not there has to be an empty answer and never an
/// error.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn asking_about_a_relation_that_is_not_there_is_an_empty_answer() {
    let src = source().await;
    let missing = "no_such_relation_anywhere";
    assert!(src.columns(SCHEMA, missing).await.unwrap().is_empty());
    assert!(src.indexes(SCHEMA, missing).await.unwrap().is_empty());
    assert!(src.triggers(SCHEMA, missing).await.unwrap().is_empty());
    assert_eq!(src.definition(SCHEMA, missing).await.unwrap(), None);
    // And a schema that is not there is an empty list rather than a failure.
    assert!(
        src.relations("memory.no_such_schema")
            .await
            .unwrap()
            .is_empty()
    );
    // A schema string with no catalog in it never came from `schemas()`, and is
    // answered with nothing rather than with a statement naming a catalog that
    // does not exist.
    assert!(src.relations("dbclient_trino").await.unwrap().is_empty());
    assert!(
        src.columns("dbclient_trino", "nums")
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Transactions, and the measurement `transactional` rests on
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn transaction_control_is_refused_by_name() {
    let src = source().await;
    assert!(!src.capabilities().transactional);
    assert!(src.capabilities().cancel_stops_the_statement);
    for step in [
        TxStep::Begin,
        TxStep::Commit,
        TxStep::Rollback,
        TxStep::Savepoint("halfway".to_string()),
        TxStep::RollbackTo("halfway".to_string()),
        TxStep::Release("halfway".to_string()),
    ] {
        let error = src
            .transaction(&step)
            .await
            .err()
            .unwrap_or_else(|| panic!("{step:?} should be refused"));
        assert!(error.to_string().contains("connector"), "got: {error}");
    }
}

/// Why `transactional` is false, made falsifiable.
///
/// Trino's protocol has interactive transactions and they work: the three
/// statements below open one, and the coordinator hands back an id that the
/// fourth carries. What does not work is writing anything inside it. The only
/// writable catalog a stock coordinator has is `memory`, which declares
/// single-statement writes, so the write is refused with
/// `AUTOCOMMIT_WRITE_CONFLICT` — and the refusal *aborts the transaction*, so the
/// statement after it is refused too, with `TRANSACTION_ALREADY_ABORTED`.
///
/// Spoken directly to the protocol because this driver deliberately sends no
/// `X-Trino-Transaction-Id`, so there is no way to reach this through its own
/// API. The day a coordinator here has a catalog that takes writes inside a
/// transaction, this test fails and `Driver::transactional` is worth rewriting.
#[tokio::test]
#[ignore = "requires a Trino coordinator"]
async fn a_write_inside_a_transaction_is_refused_by_the_only_writable_catalog() {
    // A table of its own, so that a write which one day succeeds fails this test
    // rather than the row counts in every other one.
    let src = source().await;
    let target = format!("{SCHEMA}.tx_probe");
    run(&src, &format!("DROP TABLE IF EXISTS {target}")).await;
    run(&src, &format!("CREATE TABLE {target} (n integer)")).await;

    let (headers, _) = raw("START TRANSACTION", &[("X-Trino-Transaction-Id", "NONE")]).await;
    let id = headers
        .iter()
        .find(|(name, _)| name == "x-trino-started-transaction-id")
        .map(|(_, value)| value.clone())
        .expect("the coordinator starts a transaction and names it");
    let inside = [("X-Trino-Transaction-Id", id.as_str())];

    // A read inside the transaction is ordinary, which is what makes the next
    // assertion about writes rather than about transactions.
    let (_, reading) = raw("SELECT count(*) FROM tpch.tiny.nation", &inside).await;
    assert_eq!(reading, None, "a read inside a transaction is fine");

    let (_, writing) = raw(&format!("INSERT INTO {target} VALUES (1)"), &inside).await;
    assert_eq!(
        writing.as_deref(),
        Some("AUTOCOMMIT_WRITE_CONFLICT"),
        "the only writable catalog here refuses every write inside a transaction"
    );

    let (_, after) = raw("SELECT 1", &inside).await;
    assert_eq!(
        after.as_deref(),
        Some("TRANSACTION_ALREADY_ABORTED"),
        "and the refusal takes the whole transaction with it"
    );

    raw("ROLLBACK", &inside).await;
    run(&src, &format!("DROP TABLE {target}")).await;
}

/// One statement over the protocol, past this driver.
///
/// Returns the response headers that were seen, lower-cased, and the name of the
/// error if there was one. Only the two tests that need a header this driver
/// does not send use it.
async fn raw(sql: &str, headers: &[(&str, &str)]) -> (Vec<(String, String)>, Option<String>) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let ask = async |method: Method, uri: String, body: Full<Bytes>| {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Trino-User", "integration");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = client
            .request(request.body(body).expect("a request"))
            .await
            .expect("Trino unreachable; see the header of this file");
        let (head, body) = response.into_parts();
        let seen: Vec<(String, String)> = head
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let answer: serde_json::Value =
            serde_json::from_slice(&body.collect().await.expect("a body").to_bytes())
                .expect("Trino answers JSON");
        (seen, answer)
    };

    let (mut seen, mut body) = ask(
        Method::POST,
        format!("{ORIGIN}/v1/statement"),
        Full::new(Bytes::from(sql.to_string())),
    )
    .await;
    let mut failed = None;
    while let Some(next) = body.get("nextUri").and_then(|uri| uri.as_str()) {
        let (more, answer) = ask(Method::GET, next.to_string(), Full::default()).await;
        seen.extend(more);
        body = answer;
        if let Some(name) = body
            .get("error")
            .and_then(|failure| failure.get("errorName"))
            .and_then(|name| name.as_str())
        {
            failed = Some(name.to_string());
        }
    }
    (seen, failed)
}
