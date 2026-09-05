//! What this driver claims, checked against a live ClickHouse.
//!
//! Every test here is `#[ignore]`d, so `cargo test` passes with nothing
//! installed. To run them:
//!
//! ```text
//! docker run -d --name clickhouse-test \
//!   -p 58123:8123 -p 59000:9000 \
//!   -e CLICKHOUSE_PASSWORD=test \
//!   --ulimit nofile=262144:262144 \
//!   clickhouse/clickhouse-server:24
//!
//! # ready when this answers:
//! curl -fsS http://127.0.0.1:58123/ping
//!
//! cargo test -p driver-clickhouse -- --ignored
//! ```
//!
//! The fixture in `tests/seed.sql` is applied by the tests themselves, through
//! this driver, so there is nothing to remember to run first — and applying it
//! that way is the first check in the file, since a driver that cannot run
//! `CREATE TABLE` has not been exercised by anything that only reads.
//!
//! `--ulimit nofile` is ClickHouse's documented requirement; without it the
//! server logs a warning and behaves oddly under load. Port 58123 rather than
//! 8123 because a developer machine may already have a ClickHouse on the
//! default, and a fixture that silently attaches to the wrong server is a worse
//! failure than one that cannot connect.

use arrow::array::{Array, Date32Array, Decimal128Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, TimeUnit};
use dbconn::RelationKind;
use driver_clickhouse::ChSource;
use std::time::Duration;

const URL: &str = "http://default:test@127.0.0.1:58123/bench";
const ADMIN_URL: &str = "http://default:test@127.0.0.1:58123/default";

/// Applied once per test process; `seed` is what keeps that from meaning once
/// per test.
static FIXTURE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// The fixture as text: what applies it, and what recognises it on the server.
const SEED: &str = include_str!("seed.sql");

/// A connection to the seeded fixture database.
async fn source() -> ChSource {
    FIXTURE.get_or_init(seed).await;
    connect(URL).await
}

/// A connection to `url`, retried rather than attempted once.
///
/// The first network syscall a freshly built test binary makes takes most of a
/// minute on a development Mac — long enough that ClickHouse closes the socket
/// before the request finishes arriving, and the client reports
/// `connection closed before message completed`. A fixture that gave up there
/// would be reporting the laptop rather than the server. Every attempt after
/// the first is milliseconds, and `cargo nextest` makes it matter more rather
/// than less: each test is a process of its own, so each one arrives cold.
async fn connect(url: &str) -> ChSource {
    let mut last = None;
    for _ in 0..30 {
        match ChSource::connect(url).await {
            Ok(source) => return source,
            // Slept between attempts because this client fails a closed port in
            // microseconds, and a bare loop would turn a server that is simply
            // not running into a spin rather than into a message.
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    panic!(
        "ClickHouse unreachable; see the command at the top of this file: {}",
        last.expect("at least one attempt")
    )
}

/// Applies `SEED`, one statement per request, unless the server already holds
/// exactly it.
///
/// ClickHouse's HTTP interface takes one statement per request, so the file is
/// split on a line of `;;` rather than on `;` — splitting on the semicolon would
/// cut the multi-row `INSERT`s apart at every value.
///
/// The page size is a real number and not the 1 that "these statements answer
/// with nothing" invites, because in this driver the page size is
/// `max_block_size`: a setting on the server's own pipeline rather than a
/// request for one row at a time. At 1, `INSERT … SELECT … FROM
/// numbers(1000000)` is processed as a million blocks. Measured against
/// ClickHouse 24.10: 2.1s and 1.17GiB at a page size of 1, 171ms and 119MiB at
/// this one — which is the difference between a fixture that applies and one
/// that is stopped by the server's memory tracker and reported as a broken
/// driver.
const SEED_PAGE: usize = 65536;

async fn seed() {
    // `cargo nextest` gives every test its own process, so `FIXTURE` holds
    // nothing back across them: without this lock thirty-four processes apply
    // `SEED` to the same database at once and knock each other over with
    // `Table bench.types_all already exists`.
    let _turn = dbfixture::exclusive("clickhouse").await;
    let admin = connect(ADMIN_URL).await;

    let want = dbfixture::fingerprint([SEED]);
    if stamp(&admin).await.as_deref() == Some(want.as_str()) {
        return;
    }
    // Dropped rather than applied over, which also takes the stamp with it: a
    // run that stops partway leaves nothing that a later one would mistake for
    // a finished fixture.
    run(&admin, "DROP DATABASE IF EXISTS bench").await;
    for statement in SEED.split("\n;;") {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        run(&admin, statement).await;
    }
    // Last, so that it means "all of the above ran".
    run(
        &admin,
        "CREATE TABLE bench.__fixture (stamp String) ENGINE = TinyLog",
    )
    .await;
    run(
        &admin,
        &format!("INSERT INTO bench.__fixture VALUES ('{want}')"),
    )
    .await;
}

/// Runs one seed statement for its effect.
async fn run(admin: &ChSource, statement: &str) {
    admin
        .query(statement, SEED_PAGE)
        .await
        .unwrap_or_else(|e| panic!("seeding failed on `{}`: {e}", head(statement)));
}

/// What the fixture on the server was built from, or `None` when there is
/// nothing to read it from: no database, no stamp table, or a rebuild that
/// stopped before it wrote one.
///
/// A stamp rather than "do the tables exist", because the container outlives
/// every run and `SEED` does not — a table built by an older version of the file
/// beside it answers yes, and the test added alongside a new column then fails
/// naming the column, which reads exactly like the driver losing it.
async fn stamp(admin: &ChSource) -> Option<String> {
    let mut rows = admin
        .query("SELECT stamp FROM bench.__fixture", 1)
        .await
        .ok()?;
    let batch = rows.next_page().await.ok()??;
    let column = batch.column(0).as_any().downcast_ref::<StringArray>()?;
    (!column.is_empty()).then(|| column.value(0).to_string())
}

/// Enough of a statement to recognise it by, for the message when seeding
/// fails. Counted in characters, because slicing a string by bytes is how a
/// failure report becomes a second failure.
fn head(statement: &str) -> String {
    statement
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}

/// Everything one statement produces, in one batch.
async fn read_all(source: &ChSource, sql: &str) -> RecordBatch {
    let mut rows = source.query(sql, 4096).await.expect("query failed");
    let schema = rows.schema();
    let mut batches = Vec::new();
    while let Some(batch) = rows.next_page().await.expect("batch failed") {
        batches.push(batch);
    }
    arrow::compute::concat_batches(&schema, &batches).expect("concat failed")
}

fn text(batch: &RecordBatch, column: &str, row: usize) -> String {
    let at = batch.schema().index_of(column).expect("no such column");
    let array = batch
        .column(at)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| {
            panic!(
                "{column} arrived as {:?}, not text",
                batch.column(at).data_type()
            )
        });
    array.value(row).to_string()
}

fn kind_of(batch: &RecordBatch, column: &str) -> DataType {
    let at = batch.schema().index_of(column).expect("no such column");
    batch.schema().field(at).data_type().clone()
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The finding that decided this driver's shape.
///
/// ClickHouse 24.10 does not flatten `UUID` on the way into Arrow — it refuses
/// it, with `Code: 50 UNKNOWN_TYPE`, and the refusal takes the whole statement
/// with it. So `SELECT *` from any table with a UUID column cannot be answered
/// by repairing the batch afterwards, because there is no batch. The spec this
/// was written from expected `FixedSizeBinary(16)` and marked it unverified; it
/// is worse than that, and it is why the conversion happens in the SELECT list.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_uuid_does_not_refuse_the_whole_statement() {
    let source = source().await;
    let batch = read_all(&source, "SELECT id, uid FROM bench.types_all ORDER BY id").await;
    assert_eq!(kind_of(&batch, "uid"), DataType::Utf8);
    assert_eq!(
        text(&batch, "uid", 0),
        "00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        text(&batch, "uid", 1),
        "ffffffff-ffff-ffff-ffff-ffffffffffff"
    );
    assert_eq!(
        text(&batch, "uid", 2),
        "01890a5d-ac96-774b-bcce-b302099a8057"
    );
}

/// The same refusal, for two more types, so that a server version which starts
/// accepting them does not quietly change what this driver sends.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_types_the_arrow_writer_refuses_still_arrive() {
    let source = source().await;
    let batch = read_all(&source, "SELECT id, iv FROM bench.types_all ORDER BY id").await;
    assert_eq!(kind_of(&batch, "iv"), DataType::Utf8);
    assert_eq!(text(&batch, "iv", 1), "365");
}

/// `18446744073709551615` is the value that separates a driver which thought
/// about `UInt64` from one that did not.
///
/// Arrow has an unsigned 64-bit type and ClickHouse sends it, so nothing is lost
/// on the wire — but the reader on the far side of the FFI has no case for `L`
/// and would draw the column as `<L>` in every cell. `Decimal128(20, 0)` is the
/// narrowest type it can read that still holds twenty digits.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_uint64_that_does_not_fit_an_int64_survives() {
    let source = source().await;
    let batch = read_all(&source, "SELECT id, u64 FROM bench.types_all ORDER BY id").await;
    assert_eq!(kind_of(&batch, "u64"), DataType::Decimal128(20, 0));
    let at = batch.schema().index_of("u64").unwrap();
    let values = batch
        .column(at)
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .expect("u64 should be a decimal");
    assert_eq!(values.value(0), u64::MAX as i128);
    assert_eq!(values.value(1), 0);
}

/// The labels are the column. `Enum8` arrives as the ordinal with them gone, so
/// a driver that passed the Arrow buffer through would show `-1`, `0` and `1`
/// where the table says `draft`, `live` and `archived`.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn an_enum_keeps_its_labels() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, e8, e16 FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(kind_of(&batch, "e8"), DataType::Utf8);
    assert_eq!(text(&batch, "e8", 0), "draft");
    assert_eq!(text(&batch, "e8", 1), "archived");
    assert_eq!(text(&batch, "e8", 2), "live");
    assert_eq!(text(&batch, "e16", 1), "beta");
}

/// `Date` arrives as the `UInt16` day count it is stored as. `Date32` arrives
/// correctly, which the documented type-matching table denies — it claims
/// `Date32 → UINT16`, a type that cannot hold a range reaching 1900.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_date_is_a_date_and_not_a_number() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, dt_date, dt_date32 FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(kind_of(&batch, "dt_date"), DataType::Date32);
    assert_eq!(kind_of(&batch, "dt_date32"), DataType::Date32);

    let at = batch.schema().index_of("dt_date32").unwrap();
    let days = batch
        .column(at)
        .as_any()
        .downcast_ref::<Date32Array>()
        .expect("a date column");
    // 1900-01-01, which is 25567 days before the epoch and does not fit the
    // UInt16 the documentation says this arrives as.
    assert_eq!(days.value(0), -25567);
}

/// The zone is in the declared type and nowhere in the Arrow field ClickHouse
/// sends for a `DateTime`, which arrives as a bare `UInt32`. Reading it back out
/// of the declaration is the only way to keep it.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_datetime_keeps_its_zone() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, dt_datetime, dt_datetime_tz, dt_dt64_3, dt_dt64_9_tz \
         FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(
        kind_of(&batch, "dt_datetime_tz"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("Asia/Taipei".into()))
    );
    // No zone of its own means the server's, which this driver reads at connect
    // rather than assuming.
    assert_eq!(
        kind_of(&batch, "dt_datetime"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    assert_eq!(
        kind_of(&batch, "dt_dt64_3"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    // Nine digits do not fit in a microsecond timestamp, and dropping three of
    // them quietly is worse than handing over all nine.
    assert_eq!(kind_of(&batch, "dt_dt64_9_tz"), DataType::Utf8);
    assert_eq!(
        text(&batch, "dt_dt64_9_tz", 2),
        "2024-01-15 12:34:56.123456789"
    );
}

/// A `UInt32` in the grid is not an address, and neither is sixteen bytes of
/// binary.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn an_ip_address_is_not_an_integer() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, ip4, ip6 FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(kind_of(&batch, "ip4"), DataType::Utf8);
    assert_eq!(text(&batch, "ip4", 1), "255.255.255.255");
    assert_eq!(text(&batch, "ip4", 2), "10.0.0.1");
    assert_eq!(
        text(&batch, "ip6", 1),
        "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
    );
    assert_eq!(text(&batch, "ip6", 2), "2001:db8::1");
}

/// Arrow can express all four of these and the reader at the other end of the
/// FFI maps a closed set of format strings, so `List`, `Struct` and `Map` reach
/// the grid as `<+l>`, `<+s>` and `<+m>` in every cell. ClickHouse's own
/// rendering is the form the user would have typed.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_composite_value_arrives_as_something_the_grid_can_draw() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, arr, arr_nested, tup, tup_named, map_ss, map_si \
         FROM bench.types_all ORDER BY id",
    )
    .await;
    for column in ["arr", "arr_nested", "tup", "tup_named", "map_ss", "map_si"] {
        assert_eq!(kind_of(&batch, column), DataType::Utf8, "{column}");
    }
    assert_eq!(text(&batch, "arr", 1), "[1,-1,2147483647]");
    assert_eq!(text(&batch, "arr_nested", 1), "[['a','b'],['c']]");
    assert_eq!(text(&batch, "tup", 1), "(2147483647,'z')");
    assert_eq!(text(&batch, "map_ss", 1), "{'a':'1','b':'2'}");
    assert_eq!(text(&batch, "map_si", 1), "{'a':[1],'b':[2,3]}");
}

/// Wide integers have no Arrow home at all past 128 bits, and the 128-bit ones
/// arrive as fixed-size binary the reader cannot open.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_number_too_wide_for_arrow_keeps_all_its_digits() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, i128, i256, u128, u256, d256 FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(
        text(&batch, "i128", 0),
        "-170141183460469231731687303715884105728"
    );
    assert_eq!(
        text(&batch, "u256", 0),
        "115792089237316195423570985008687907853269984665640564039457584007913129639935"
    );
    // Decimal256 is a real Arrow type, and the reader would parse its format
    // string as a Decimal128 and read half the bytes — visibly wrong digits,
    // which is worse than a column it says it cannot draw.
    assert_eq!(kind_of(&batch, "d256"), DataType::Utf8);
    assert_eq!(
        text(&batch, "d256", 1),
        "0.0000000000000000000000000000000000000001"
    );
}

/// The types that already arrive in a shape both ends can read must be left
/// alone, or the projection is doing work for nothing on every query.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn an_ordinary_column_is_left_as_it_is() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT i16, i32, i64, f32, f64, b, s, n_str, d32, d64, d128, fs \
         FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(kind_of(&batch, "i16"), DataType::Int16);
    assert_eq!(kind_of(&batch, "i32"), DataType::Int32);
    assert_eq!(kind_of(&batch, "i64"), DataType::Int64);
    assert_eq!(kind_of(&batch, "f32"), DataType::Float32);
    assert_eq!(kind_of(&batch, "f64"), DataType::Float64);
    assert_eq!(kind_of(&batch, "b"), DataType::Boolean);
    assert_eq!(kind_of(&batch, "s"), DataType::Utf8);
    assert_eq!(kind_of(&batch, "d32"), DataType::Decimal128(9, 4));
    assert_eq!(kind_of(&batch, "d64"), DataType::Decimal128(18, 8));
    assert_eq!(kind_of(&batch, "d128"), DataType::Decimal128(38, 18));
    // A FixedString would arrive as FixedSizeBinary, which the reader has no
    // case for; the setting that makes it an ordinary string is pinned by the
    // driver, padding NULs and all.
    assert_eq!(kind_of(&batch, "fs"), DataType::Utf8);
    assert_eq!(text(&batch, "fs", 0), "fixed\0\0\0");
    assert_eq!(text(&batch, "s", 1), "ordinary");
    // A nullable string, which is the same Arrow type with a validity buffer —
    // ClickHouse's `Nullable(T)` costs nothing to carry and nothing to map.
    assert_eq!(text(&batch, "n_str", 1), "ünïcödé ✓ 漢字");
    let at = batch.schema().index_of("n_str").unwrap();
    assert!(batch.column(at).is_null(0), "NULL is not an empty string");
    assert!(batch.schema().field(at).is_nullable());
}

/// The narrow and unsigned integers Arrow has and the reader does not, widened
/// into signed types that hold them exactly.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_narrow_integer_is_widened_rather_than_left_undrawable() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT id, i8, u8, u16, u32 FROM bench.types_all ORDER BY id",
    )
    .await;
    assert_eq!(kind_of(&batch, "i8"), DataType::Int16);
    assert_eq!(kind_of(&batch, "u8"), DataType::Int16);
    assert_eq!(kind_of(&batch, "u16"), DataType::Int32);
    assert_eq!(kind_of(&batch, "u32"), DataType::Int64);
}

/// One bad row must not make a table unbrowsable.
///
/// ClickHouse's `String` is arbitrary bytes and the server does not check them,
/// so an invalid sequence fails inside the Arrow IPC decoder and takes the whole
/// result with it. The retry asks again through `toValidUTF8` and arrives as the
/// same Arrow type the schema already promised, so the caller never sees that
/// anything happened.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn invalid_utf8_in_a_string_column_does_not_lose_the_table() {
    let source = source().await;
    let batch = read_all(&source, "SELECT id, s FROM bench.dirty_strings ORDER BY id").await;
    assert_eq!(batch.num_rows(), 3, "the clean rows must survive too");
    assert_eq!(text(&batch, "s", 1), "clean");
    assert!(
        text(&batch, "s", 0).contains('\u{fffd}'),
        "bytes that are not text should arrive as replacement characters, got {:?}",
        text(&batch, "s", 0)
    );
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The shared contract, driven through `&dyn Driver` and nothing else.
///
/// `crates/conn/tests/contract.rs` is wired centrally after this branch merges,
/// so this is the same walk done here: the statements it would use, in the shape
/// it would use them, against the fixture as this crate leaves it. A driver that
/// only ever ran through its own inherent API has not been checked against the
/// thing every driver has to satisfy.
///
/// The relation is named `bench_wide` and reached unqualified on purpose — that
/// is what the contract's other subjects look like, and it is what proves the
/// database in the connection URL is the one unqualified names resolve in.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_shared_contract_holds_through_the_trait() {
    let source = source().await;
    let driver: &dyn dbconn::Driver = &source;

    let mut stream = driver
        .query("SELECT id FROM bench_wide ORDER BY id", 100)
        .await
        .expect("query failed");
    assert_eq!(stream.schema().fields().len(), 1);
    assert_eq!(stream.rows_affected(), None);
    assert_eq!(stream.next_batch().await.unwrap().unwrap().num_rows(), 100);
    assert_eq!(stream.next_batch().await.unwrap().unwrap().num_rows(), 100);

    let mut cursor = driver
        .cursor("SELECT id FROM bench_wide ORDER BY id", 50)
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
    cursor.canceller().cancel().await.expect("idle cancel");
    cursor.close().await.expect("close failed");
    driver.cancel().await.expect("session cancel");

    // A caret a front end could place: counted from one, and no further than one
    // past the end of what was sent. Zero is the trap — it is what a driver
    // produces by forgetting to convert an offset, and it looks like a position.
    for sql in [
        "SELECT id FROM bench_wide WHERE ORDER BY id",
        "SELECT * FROM no_such_relation_anywhere",
    ] {
        let error = match driver.query(sql, 10).await {
            Err(e) => e,
            Ok(mut stream) => stream
                .next_batch()
                .await
                .err()
                .unwrap_or_else(|| panic!("expected this to fail: {sql}")),
        };
        assert!(!error.is_cancelled(), "{sql}");
        if let Some(at) = error.statement_position() {
            assert!(at >= 1, "positions count from one, got {at}");
            assert!(at as usize <= sql.chars().count() + 1, "{sql}: {at}");
        }
    }

    // Every metadata call answers for a relation that exists, and the answers
    // agree with each other.
    assert!(
        driver
            .schemas()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "bench")
    );
    let relations = driver.relations("bench").await.unwrap();
    let found = relations
        .iter()
        .find(|r| r.name == "bench_wide")
        .expect("bench_wide should be listed");
    assert_eq!(found.schema, "bench");
    let columns = driver.columns("bench", "bench_wide").await.unwrap();
    assert!(columns.iter().any(|c| c.name == "id"));
    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(column.position, offset as i32 + 1);
        assert!(!column.data_type.is_empty());
    }
    assert_eq!(
        driver.definition("bench", "bench_wide").await.unwrap(),
        None
    );
    driver.indexes("bench", "bench_wide").await.unwrap();
    driver.foreign_keys("bench", "bench_wide").await.unwrap();
    driver.referenced_by("bench", "bench_wide").await.unwrap();
    driver.constraints("bench", "bench_wide").await.unwrap();
    driver.triggers("bench", "bench_wide").await.unwrap();
}

/// `max_block_size` is a hint to the server's pipeline, not a promise, so the
/// page size has to be the caller's number and not whatever arrived.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn batches_arrive_in_the_size_that_was_asked_for() {
    let source = source().await;
    let mut rows = source
        .query("SELECT id FROM bench.bench_wide ORDER BY id", 100)
        .await
        .expect("query failed");
    // Before a single row: the whole reason `DESCRIBE` runs first.
    assert_eq!(rows.schema().fields().len(), 1);
    assert_eq!(rows.rows_affected(), None);

    for _ in 0..3 {
        let batch = rows.next_page().await.unwrap().expect("a batch");
        assert_eq!(batch.num_rows(), 100);
    }
    assert_eq!(rows.rows_affected(), None, "there is more to come");
}

/// A million rows read forward: every page must continue where the last one
/// stopped, with nothing repeated and nothing skipped.
///
/// The wrapper this driver puts around a statement is what makes this worth
/// checking rather than assuming — a projection over a sorted subquery is only
/// useful if the sort survives it.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn paging_does_not_re_read_what_it_returned() {
    let source = source().await;
    let mut cursor = source
        .cursor(
            "SELECT id, v_uuid FROM bench.bench_wide ORDER BY id",
            25_000,
        )
        .await
        .expect("cursor failed");

    let mut seen = 0i128;
    let mut previous: Option<i128> = None;
    let mut pages = 0;
    while let Some(batch) = cursor.next_page().await.expect("fetch failed") {
        pages += 1;
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .expect("id should be a decimal");
        for row in 0..batch.num_rows() {
            let id = ids.value(row);
            if let Some(last) = previous {
                assert_eq!(id, last + 1, "row {seen} broke the sequence");
            }
            previous = Some(id);
            seen += 1;
        }
    }
    assert_eq!(seen, 1_000_000);
    assert_eq!(pages, 40);
    assert_eq!(cursor.rows_affected(), Some(1_000_000));
}

/// A statement with no result set is not a statement this driver refuses.
///
/// `DESCRIBE (INSERT …)` is a syntax error, so the driver falls through to
/// running the caller's own text — which is also what gives a broken statement
/// an offset into what they actually wrote.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_statement_with_no_result_set_still_runs() {
    let source = source().await;
    for statement in [
        "DROP TABLE IF EXISTS bench.scratch",
        "CREATE TABLE bench.scratch (a Int32) ENGINE = Memory",
        "INSERT INTO bench.scratch VALUES (1), (2), (3)",
    ] {
        let mut rows = source.query(statement, 10).await.expect(statement);
        assert!(rows.next_page().await.unwrap().is_none());
        // Not `Some(0)`. The insert affected three rows and this driver has no
        // way to learn the number, so it declines rather than states a false
        // one — the count rides in a response header the crate does not expose.
        assert_eq!(rows.rows_affected(), None, "{statement}");
    }
    let batch = read_all(&source, "SELECT sum(a) AS total FROM bench.scratch").await;
    assert_eq!(batch.num_rows(), 1);
}

/// A statement with a result set the planner will not describe still has one.
///
/// `DESCRIBE (SHOW …)` is the same syntax error as `DESCRIBE (INSERT …)`, and
/// for a while both were treated as the same thing: run it, hand back nothing.
/// That is right for the write and silently wrong for the read — `SHOW
/// DATABASES` typed into the editor drew an empty grid, and a table's DDL, which
/// is `SHOW CREATE TABLE` and nothing else, could not be read at all.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_show_statement_answers_with_the_rows_it_has() {
    let source = source().await;
    let batch = read_all(&source, "SHOW CREATE TABLE bench.types_all").await;
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(batch.schema().field(0).name(), "statement");
    let statement = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("the statement arrives as text");
    assert!(
        statement
            .value(0)
            .starts_with("CREATE TABLE bench.types_all"),
        "{}",
        statement.value(0)
    );

    // Rows produced, where the `INSERT` above declines to answer: this one is a
    // read, and the count of what a read produced is a number the driver has.
    let mut rows = source
        .query("SHOW DATABASES", 100)
        .await
        .expect("running SHOW DATABASES");
    let mut produced = 0;
    while let Some(page) = rows.next_page().await.expect("reading a page") {
        produced += page.num_rows();
    }
    assert!(produced >= 2, "at least bench and system are there");
    assert_eq!(rows.rows_affected(), Some(produced as u64));
}

/// A statement that cannot be described has to report its own failure and not
/// the one `DESCRIBE` had with it.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_statement_that_cannot_be_described_reports_its_own_error() {
    let source = source().await;
    let error = failure(&source, "INSERT INTO bench.no_such_table_at_all VALUES (1)").await;
    assert!(
        error.to_string().contains("no_such_table_at_all"),
        "the failure should name what was wrong, got: {error}"
    );
    assert!(!error.is_cancelled());
}

// ---------------------------------------------------------------------------
// Failure and cancellation
// ---------------------------------------------------------------------------

async fn failure(source: &ChSource, sql: &str) -> driver_clickhouse::ChError {
    match source.query(sql, 10).await {
        Err(e) => e,
        Ok(mut rows) => match rows.next_page().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {sql}"),
        },
    }
}

/// ClickHouse counts a syntax error's offset in bytes from one; the trait counts
/// it in characters from one. The two agree on every statement written in
/// English, which is why the second half of this test exists.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_syntax_error_says_where() {
    let source = source().await;

    let ascii = "SELECT id FROM bench.bench_wide WHERE ORDER BY id";
    let error = failure(&source, ascii).await;
    let at = error.statement_position().expect("a position");
    assert_eq!(ascii.chars().nth(at as usize - 1), Some('B'));
    assert!(!error.is_cancelled());

    let unicode = "SELECT \"漢字漢字漢字\" FROM bench.bench_wide WHERE ORDER BY id";
    let error = failure(&source, unicode).await;
    let at = error.statement_position().expect("a position");
    assert_eq!(
        unicode.chars().nth(at as usize - 1),
        Some('B'),
        "a byte offset applied as a character one lands {} characters late",
        at as usize - 51
    );
}

/// A cancelled statement has to say so, because the front end draws one
/// differently from a fault.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_cancelled_query_says_it_was_cancelled() {
    let source = source().await;
    // One row per block, a hundredth of a second each: long enough to still be
    // running when the KILL lands, short enough not to hold the suite up.
    let mut rows = source
        .query("SELECT sleepEachRow(0.01) AS naptime FROM numbers(3000)", 1)
        .await
        .expect("query failed");

    let canceller = rows.canceller();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        canceller.cancel().await.expect("cancel failed");
    });

    let error = loop {
        match rows.next_page().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the statement finished before the cancel landed"),
            Err(e) => break e,
        }
    };
    assert!(
        error.is_cancelled(),
        "a killed statement should report itself as cancelled, got: {error}"
    );
}

/// The other half of the same claim: a statement that broke on its own must not
/// be reported as somebody's button working.
///
/// This is what keeps `is_cancelled` from being "did this side call cancel" —
/// which would hide a real fault behind the Cancel button whenever the two
/// happened at once.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_query_that_fails_on_its_own_merits_is_not_a_cancellation() {
    let source = source().await;
    let error = failure(
        &source,
        "SELECT throwIf(number = 400, 'deliberate') FROM numbers(10000)",
    )
    .await;
    assert!(!error.is_cancelled());
    assert!(error.to_string().contains("deliberate"), "got: {error}");
}

/// Cancelling a session with nothing running is a no-op, and so is cancelling a
/// reader that has not been read from.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn cancelling_nothing_is_not_a_failure() {
    let source = source().await;
    source.cancel().await.expect("an idle session");

    let rows = source
        .query("SELECT id FROM bench.bench_wide ORDER BY id", 10)
        .await
        .expect("query failed");
    rows.canceller().cancel().await.expect("an idle reader");
    // And once the reader is gone, the session has nothing to name again.
    drop(rows);
    source.cancel().await.expect("a session with nothing left");
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// The catalog is reported and marked rather than left out.
///
/// This test used to assert the opposite, and it was right when it was written:
/// the driver kept the system databases out of its answer entirely. That became
/// wrong when the marking went in — a name missing from the answer is one no
/// setting can put back, so "show me `system`" was a thing this client could not
/// do at all. What the driver owes now is the mark and the order.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_navigator_root_marks_the_catalog_without_hiding_it() {
    let source = source().await;
    let schemas = source.schemas().await.expect("schemas failed");
    let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
    let system = |name: &str| {
        schemas
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed, got {names:?}"))
            .is_system
    };
    for own in ["system", "INFORMATION_SCHEMA", "information_schema"] {
        assert!(system(own), "{own} is the engine's own");
    }
    assert!(!system("bench"), "and the seeded database is not");

    // And the engine's own sort last, so a tree that opens on a fresh server
    // does not open on `INFORMATION_SCHEMA`. The ordering is the half of this a
    // marking driver can still get wrong.
    let first_system = schemas
        .iter()
        .position(|s| s.is_system)
        .expect("the engine's own are in the list");
    assert!(
        schemas[first_system..].iter().all(|s| s.is_system),
        "the engine's own belong after everybody else's, got {names:?}"
    );
}

/// A materialized view holds data and a plain view does not, and upstream's
/// `tableType.contains("VIEW")` cannot tell them apart.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_materialized_view_is_not_reported_as_a_view() {
    let source = source().await;
    let relations = source.relations("bench").await.expect("relations failed");
    let kind = |name: &str| {
        relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .kind
    };
    assert_eq!(kind("plain_view"), RelationKind::View);
    assert_eq!(kind("mat_view"), RelationKind::MaterializedView);
    assert_eq!(kind("meta_rich"), RelationKind::Table);
    assert_eq!(kind("no_stats"), RelationKind::Table);
}

/// Declining to answer is not the same as answering zero, and a sidebar that
/// reports a view as empty is stating something false.
///
/// The spec this was written from expected the `Log` engine to be the case that
/// declines; on 24.10 it reports a real count, and the engines that answer NULL
/// are the views.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_view_declines_to_estimate_its_rows() {
    let source = source().await;
    let relations = source.relations("bench").await.expect("relations failed");
    let rows = |name: &str| {
        relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .estimated_rows
    };
    assert_eq!(rows("plain_view"), None);
    assert_eq!(rows("mat_view"), None);
    assert_eq!(rows("meta_rich"), Some(1000));
    assert_eq!(rows("no_stats"), Some(3));
}

/// Upstream runs a second query over `system.parts` to get this number, because
/// its JDBC path did not surface `total_rows`. This driver reads it in the same
/// statement as everything else, on the assumption that the two agree — which is
/// worth one assertion rather than a comment.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_row_estimate_agrees_with_the_parts_it_sums() {
    let source = source().await;
    let batch = read_all(
        &source,
        "SELECT toInt64(sum(rows)) AS n FROM system.parts \
         WHERE database = 'bench' AND table = 'meta_rich' AND active",
    )
    .await;
    let summed = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("a count")
        .value(0);

    let relations = source.relations("bench").await.expect("relations failed");
    let reported = relations
        .iter()
        .find(|r| r.name == "meta_rich")
        .unwrap()
        .estimated_rows;
    assert_eq!(reported, Some(summed));
}

/// `system.columns` has no `is_nullable`: ClickHouse spells nullability inside
/// the type, and `LowCardinality` may wrap it.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn nullability_is_read_out_of_the_type() {
    let source = source().await;
    let columns = source
        .columns("bench", "types_all")
        .await
        .expect("columns failed");
    let nullable = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .nullable
    };
    assert!(nullable("lc_nullable"), "LowCardinality(Nullable(String))");
    assert!(!nullable("lc"), "LowCardinality(String)");
    assert!(nullable("n_i32"));
    assert!(!nullable("i32"));

    // And the declared type is carried through verbatim, because it is what the
    // structure pane shows and what the type mapping reads.
    let declared = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .data_type
            .clone()
    };
    assert_eq!(declared("lc_nullable"), "LowCardinality(Nullable(String))");
    assert_eq!(
        declared("e8"),
        "Enum8('draft' = -1, 'live' = 0, 'archived' = 1)"
    );
    assert_eq!(declared("d64"), "Decimal(18, 8)");
}

/// Positions are one-based and contiguous, and the key flag is what the user
/// wrote after `ORDER BY` — not a uniqueness claim.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn columns_are_numbered_from_one() {
    let source = source().await;
    let columns = source
        .columns("bench", "meta_rich")
        .await
        .expect("columns failed");
    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(column.position, offset as i32 + 1, "{}", column.name);
        assert!(!column.data_type.is_empty());
    }
    assert!(
        columns
            .iter()
            .find(|c| c.name == "id")
            .unwrap()
            .is_primary_key
    );
    assert!(
        !columns
            .iter()
            .find(|c| c.name == "payload")
            .unwrap()
            .is_primary_key
    );
}

/// A MATERIALIZED expression is not a default. `ColumnInfo` has one field for
/// four kinds, so the kind travels in front of the expression rather than being
/// dropped or silently rendered as something the user may write to.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_materialized_expression_is_not_reported_as_a_plain_default() {
    let source = source().await;
    let columns = source
        .columns("bench", "meta_rich")
        .await
        .expect("columns failed");
    let default = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .default_value
            .clone()
    };
    assert_eq!(default("tag").as_deref(), Some("'none'"));
    assert_eq!(default("derived").as_deref(), Some("MATERIALIZED id * 2"));
    assert_eq!(default("alias_col").as_deref(), Some("ALIAS id + 1"));
    assert_eq!(default("eph").as_deref(), Some("EPHEMERAL 0"));
    assert_eq!(default("payload"), None);
}

/// Skip indexes are real, and the sorting key is not one of them.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_sorting_key_is_on_the_relation_and_not_in_the_index_list() {
    let source = source().await;
    let indexes = source
        .indexes("bench", "meta_rich")
        .await
        .expect("indexes failed");
    let names: Vec<&str> = indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "idx_expr",
            "idx_payload_bloom",
            "idx_tag_set",
            "idx_ts_minmax"
        ],
        "exactly the four skip indexes, and nothing synthesised"
    );
    assert!(
        indexes.iter().all(|i| !i.is_primary && !i.is_unique),
        "a skip index identifies nothing; it lets the planner discard granules"
    );

    let storage = source
        .storage("bench", "meta_rich")
        .await
        .expect("storage failed")
        .expect("meta_rich exists");
    assert_eq!(storage.engine, "MergeTree");
    assert_eq!(storage.sorting_key.as_deref(), Some("id, ts"));
    assert_eq!(storage.primary_key.as_deref(), Some("id"));
    assert_eq!(storage.partition_key.as_deref(), Some("toYYYYMM(ts)"));
    assert_eq!(
        storage.comment.as_deref(),
        Some("Everything the Structure tab has to show")
    );
}

/// The arguments are what make an index readable, and the granularity is what
/// makes its selectivity readable. `IndexInfo` has a field for neither, so both
/// travel in `method` as the DDL spells them.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_skip_index_reports_its_granularity_and_kind() {
    let source = source().await;
    let indexes = source
        .indexes("bench", "meta_rich")
        .await
        .expect("indexes failed");
    let index = |name: &str| indexes.iter().find(|i| i.name == name).unwrap();
    assert_eq!(
        index("idx_payload_bloom").method,
        "bloom_filter(0.01) GRANULARITY 4"
    );
    assert_eq!(index("idx_tag_set").method, "set(100) GRANULARITY 2");
    assert_eq!(index("idx_ts_minmax").method, "minmax GRANULARITY 1");
    // An index over an expression is not an index over the column in it.
    assert_eq!(index("idx_expr").columns, ["lower(payload)"]);
    assert_eq!(index("idx_payload_bloom").columns, ["payload"]);
}

/// A table has no definition and a view has one, which is the distinction the
/// structure pane hangs a section on.
///
/// `as_select` turning out to be populated for a plain `View` and not only for a
/// materialized one is what keeps this off the `create_table_query` fallback.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn only_a_view_has_a_definition() {
    let source = source().await;
    assert_eq!(
        source.definition("bench", "meta_rich").await.unwrap(),
        None,
        "a table is not a view"
    );

    let plain = source
        .definition("bench", "plain_view")
        .await
        .unwrap()
        .expect("a view has a definition");
    assert!(plain.contains("meta_rich"), "got: {plain}");
    assert!(
        !plain.to_uppercase().contains("CREATE VIEW"),
        "as_select is the body, not the whole statement: {plain}"
    );

    assert!(
        source
            .definition("bench", "mat_view")
            .await
            .unwrap()
            .is_some()
    );
}

/// Four calls that answer without asking, because ClickHouse has nothing to ask
/// about — and one of them is the only one that could have had something.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn the_calls_with_no_answer_are_empty_rather_than_broken() {
    let source = source().await;
    assert!(
        source
            .foreign_keys("bench", "meta_rich")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .referenced_by("bench", "meta_rich")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .triggers("bench", "meta_rich")
            .await
            .unwrap()
            .is_empty()
    );
    // meta_rich carries `CONSTRAINT payload_not_empty CHECK length(payload) > 0`
    // and there is no catalog to read it back from — it exists only inside the
    // text of `create_table_query`.
    assert!(
        source
            .constraints("bench", "meta_rich")
            .await
            .unwrap()
            .is_empty()
    );
}

/// A navigator works from a tree that can be one refresh out of date, so asking
/// about something that is not there has to be an empty answer and never an
/// error.
#[tokio::test]
#[ignore = "requires a ClickHouse server"]
async fn a_relation_that_is_not_there_is_an_empty_answer() {
    let source = source().await;
    let missing = "no_such_relation_anywhere";
    assert!(source.columns("bench", missing).await.unwrap().is_empty());
    assert!(source.indexes("bench", missing).await.unwrap().is_empty());
    assert!(
        source
            .foreign_keys("bench", missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .constraints("bench", missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(source.triggers("bench", missing).await.unwrap().is_empty());
    assert_eq!(source.definition("bench", missing).await.unwrap(), None);
    assert_eq!(source.storage("bench", missing).await.unwrap(), None);
    // And a schema that is not there is an empty list, not a failure.
    assert!(
        source
            .relations("no_such_database")
            .await
            .unwrap()
            .is_empty()
    );
}
