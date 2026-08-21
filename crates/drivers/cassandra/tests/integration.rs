//! The Cassandra driver against a live server.
//!
//! Marked `ignore`, so `cargo test` passes with nothing installed. To run them:
//!
//! ```text
//! make db-up-cassandra
//! cargo test -p driver-cassandra -- --ignored
//! make db-down-cassandra
//! ```
//!
//! The fixture is built with the `scylla` crate directly rather than through the
//! driver, so a fixture never depends on the code under test being right — with
//! one concession that is itself a finding. The seeding session needs the same
//! address translator `CassandraSource::connect` installs, because without it the
//! driver crate cannot reach a Cassandra published on a host port other than
//! 9042 at all. `without_translation_the_driver_crate_cannot_reach_a_published_container`
//! below is that statement made falsifiable, and `OneEndpoint` in the driver is
//! where the reasoning lives.
//!
//! **What this suite cannot cover.** The stock `cassandra:5` image ships
//! `materialized_views_enabled: false`, and it is a `cassandra.yaml` setting
//! with no `nodetool` or CQL equivalent — so `CREATE MATERIALIZED VIEW` is
//! refused here and the branch of `relations`/`definition` that reports one has
//! no fixture. It was verified by hand against a container started with the
//! setting flipped, which reported the view as `MaterializedView` and its
//! definition as `bucket IS NOT NULL AND id IS NOT NULL AND label IS NOT NULL`.
//! `the_stock_image_refuses_a_materialized_view` pins the reason, so that a
//! server which does allow them turns this into a failing test rather than a
//! silent gap.

use async_trait::async_trait;
use dbconn::{Browse, Driver};
use driver_cassandra::CassandraSource;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::TranslationError;
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const NODE: &str = "127.0.0.1:59042";
const KEYSPACE: &str = "dbclient_cassandra";

/// How many rows the shared fixture table holds.
const ROWS: i32 = 500;

/// The translator the driver installs, repeated here for the seeding session.
///
/// Not shared with the driver on purpose: a fixture that imported the code under
/// test would be vouching for it. Ten lines is the price of the fixture staying
/// independent.
struct AtNode(SocketAddr);

#[async_trait]
impl AddressTranslator for AtNode {
    async fn translate_address(
        &self,
        _peer: &UntranslatedPeer,
    ) -> Result<SocketAddr, TranslationError> {
        Ok(self.0)
    }
}

async fn seeding_session() -> Session {
    let at: SocketAddr = NODE.parse().expect("a literal address");
    SessionBuilder::new()
        .known_node_addr(at)
        .address_translator(Arc::new(AtNode(at)))
        .build()
        .await
        .expect("Cassandra unreachable; see the header of this file")
}

async fn run(session: &Session, cql: &str) {
    session
        .query_unpaged(cql, &[])
        .await
        .unwrap_or_else(|e| panic!("seeding failed on {cql}: {e}"));
}

/// Seeds the shared read-only fixture and returns a connected driver.
///
/// One keyspace and one set of tables for every test, rather than the
/// database-per-test the MongoDB suite uses. Two reasons, and both are about
/// Cassandra rather than about taste: a keyspace is a schema change that has to
/// reach agreement across the cluster before the next statement, so one per test
/// would dominate the run; and every statement here is an idempotent upsert of
/// the same values, so tests seeding concurrently write the same rows and cannot
/// interfere. A test that *writes* takes a table of its own — see `scratch`.
async fn fixture() -> CassandraSource {
    let session = seeding_session().await;
    run(
        &session,
        &format!(
            "CREATE KEYSPACE IF NOT EXISTS {KEYSPACE} WITH replication = \
             {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await;

    // A single partition, which is the one table shape in CQL that can be read
    // in a known order: `ORDER BY` is legal on a clustering column once the
    // partition is pinned. A table keyed only on `id` would be perfectly
    // idiomatic Cassandra and there would be no way to ask for its rows in
    // ascending order at all.
    run(
        &session,
        &format!(
            "CREATE TABLE IF NOT EXISTS {KEYSPACE}.nums \
             (bucket int, id int, label text, PRIMARY KEY (bucket, id))"
        ),
    )
    .await;
    run(
        &session,
        &format!("CREATE INDEX IF NOT EXISTS nums_label ON {KEYSPACE}.nums (label)"),
    )
    .await;
    run(
        &session,
        &format!(
            "CREATE TABLE IF NOT EXISTS {KEYSPACE}.kinds (\
                 id int PRIMARY KEY, flag boolean, tiny tinyint, small smallint, medium int, \
                 big bigint, single float, dbl double, words text, plain ascii, raw blob, \
                 uid uuid, tid timeuuid, ip inet, huge varint, money decimal, span duration, \
                 day date, clock time, moment timestamp, tags list<text>, names set<text>, \
                 props map<text, int>, pair frozen<tuple<int, text>>)"
        ),
    )
    .await;

    fill(&session, "nums", ROWS).await;
    run(
        &session,
        &format!(
            "INSERT INTO {KEYSPACE}.kinds (id, flag, tiny, small, medium, big, single, dbl, \
                 words, plain, raw, uid, tid, ip, huge, money, span, day, clock, moment, \
                 tags, names, props, pair) \
             VALUES (1, true, 7, 300, 70000, 5000000000, 1.5, 2.5, 'hello', 'ascii', 0x00ff, \
                 8e14e760-7fa8-11eb-bc66-000000000001, 8e14e760-7fa8-11eb-bc66-000000000001, \
                 '192.168.0.1', 1208925819614629174706176, 1234.56, 1mo4d, '2024-01-15', \
                 '12:00:00.123456789', '2023-11-14T22:13:20.000Z', ['a','b'], {{'x','y'}}, \
                 {{'n': 1}}, (7, 'seven'))"
        ),
    )
    .await;

    CassandraSource::connect(&format!("cassandra://{NODE}/{KEYSPACE}"))
        .await
        .expect("the driver could not connect")
}

/// Writes `rows` rows into one partition of `table`.
///
/// Batched in fifties, which is the one case Cassandra's own documentation
/// endorses batching for: every statement in it hits the same partition, so the
/// coordinator writes it as a single mutation rather than fanning out. Fifty
/// rather than all of them keeps each batch under `batch_size_warn_threshold`.
async fn fill(session: &Session, table: &str, rows: i32) {
    for chunk in (1..=rows).collect::<Vec<i32>>().chunks(50) {
        let mut cql = String::from("BEGIN UNLOGGED BATCH ");
        for id in chunk {
            cql.push_str(&format!(
                "INSERT INTO {KEYSPACE}.{table} (bucket, id, label) \
                 VALUES (0, {id}, 'row-{id}'); "
            ));
        }
        cql.push_str("APPLY BATCH");
        run(session, &cql).await;
    }
}

/// A table of this test's own, dropped and rebuilt, for the checks that write.
async fn scratch(session: &Session, table: &str, rows: i32) {
    run(session, &format!("DROP TABLE IF EXISTS {KEYSPACE}.{table}")).await;
    run(
        session,
        &format!(
            "CREATE TABLE {KEYSPACE}.{table} \
             (bucket int, id int, label text, PRIMARY KEY (bucket, id))"
        ),
    )
    .await;
    fill(session, table, rows).await;
}

/// Reads `nums` in ascending order, which needs the partition pinned.
fn read() -> String {
    format!("SELECT id FROM {KEYSPACE}.nums WHERE bucket = 0 ORDER BY id")
}

/// Every `id` a result produced, in the order it produced them.
fn ids(batch: &arrow::array::RecordBatch) -> Vec<i32> {
    let column = arrow::array::cast::as_primitive_array::<arrow::datatypes::Int32Type>(
        batch.column_by_name("id").expect("id"),
    );
    (0..batch.num_rows()).map(|r| column.value(r)).collect()
}

/// The failure `cql` produces, insisting there is one.
async fn failure(src: &CassandraSource, cql: &str) -> dbconn::DbError {
    match Driver::query(src, cql, 10).await {
        Err(e) => e,
        Ok(mut stream) => match stream.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {cql}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn reads_a_result_in_batches_of_the_size_asked_for() {
    let src = fixture().await;
    let mut stream = src.query(&read(), 100).await.expect("query");
    assert_eq!(
        stream.schema().fields().len(),
        1,
        "the projection is one column"
    );
    assert_eq!(stream.rows_affected(), None, "zero is a real answer");

    let mut seen = 0;
    while let Some(batch) = stream.next_page().await.expect("batch") {
        assert!(batch.num_rows() <= 100);
        seen += batch.num_rows();
    }
    assert_eq!(seen, ROWS as usize);
    assert_eq!(stream.rows_affected(), Some(ROWS as u64));
}

/// The page size is a request and not a promise, and the carry is what turns it
/// into the caller's number. Asked for 137 — deliberately not a divisor of 500 —
/// every batch but the last has to be exactly that.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_page_the_server_cuts_short_is_still_the_size_that_was_asked_for() {
    let src = fixture().await;
    let mut stream = src.query(&read(), 137).await.expect("query");
    let mut sizes = Vec::new();
    while let Some(batch) = stream.next_page().await.expect("batch") {
        sizes.push(batch.num_rows());
    }
    assert_eq!(sizes, vec![137, 137, 137, 89], "500 rows in pages of 137");
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn each_kind_of_value_arrives_as_the_type_that_was_decided_for_it() {
    use arrow::datatypes::{DataType, TimeUnit};
    let src = fixture().await;
    let stream = src
        .query(&format!("SELECT * FROM {KEYSPACE}.kinds"), 10)
        .await
        .expect("query");
    let schema = stream.schema();
    let of = |name: &str| {
        schema
            .field_with_name(name)
            .expect(name)
            .data_type()
            .clone()
    };

    assert_eq!(of("flag"), DataType::Boolean);
    // Arrow has an Int8 and the reader on the far side of the FFI does not.
    assert_eq!(of("tiny"), DataType::Int16);
    assert_eq!(of("small"), DataType::Int16);
    assert_eq!(of("medium"), DataType::Int32);
    assert_eq!(of("big"), DataType::Int64);
    assert_eq!(of("single"), DataType::Float32);
    assert_eq!(of("dbl"), DataType::Float64);
    assert_eq!(of("words"), DataType::Utf8);
    assert_eq!(of("plain"), DataType::Utf8);
    assert_eq!(of("raw"), DataType::Binary);
    assert_eq!(of("day"), DataType::Date32);
    assert_eq!(
        of("moment"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    // The deliberate flattenings, each for a reason recorded in `arrow_map`.
    for text in ["uid", "tid", "ip", "huge", "money", "span", "clock"] {
        assert_eq!(of(text), DataType::Utf8, "{text} should be text");
    }
    for composite in ["tags", "names", "props", "pair"] {
        assert_eq!(of(composite), DataType::Utf8, "{composite} should be JSON");
    }
}

/// The values themselves, for the four whose rendering had to be written rather
/// than borrowed: a bignum, a decimal point, nanoseconds, and JSON.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn the_values_that_had_to_be_rendered_by_hand_come_back_exact() {
    let src = fixture().await;
    let mut stream = src
        .query(
            &format!("SELECT huge, money, clock, tags, props, pair, raw FROM {KEYSPACE}.kinds"),
            10,
        )
        .await
        .expect("query");
    let batch = stream.next_page().await.expect("batch").expect("one row");
    let text = |name: &str| {
        arrow::array::cast::as_string_array(batch.column_by_name(name).expect(name))
            .value(0)
            .to_string()
    };

    // 2^80. No fixed-width integer holds it, which is the whole point of the
    // type and the reason this is not a `Decimal128`.
    assert_eq!(text("huge"), "1208925819614629174706176");
    assert_eq!(text("money"), "1234.56");
    // All nine digits: the reader's only time type is microseconds, so a driver
    // that mapped onto it would have dropped the last three.
    assert_eq!(text("clock"), "12:00:00.123456789");
    assert_eq!(text("tags"), r#"["a","b"]"#);
    assert_eq!(text("props"), r#"{"n":1}"#);
    assert_eq!(text("pair"), r#"[7,"seven"]"#);

    let raw = arrow::array::cast::as_generic_binary_array::<i32>(
        batch.column_by_name("raw").expect("raw"),
    );
    assert_eq!(raw.value(0), &[0x00, 0xff]);
}

/// A write answers with an empty frame and no count at all — not zero, nothing —
/// so saying `None` is the difference between "unknown" and "changed nothing".
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_write_reports_no_row_count_rather_than_claiming_it_changed_none() {
    let src = fixture().await;
    let mut stream = src
        .query(
            &format!("INSERT INTO {KEYSPACE}.nums (bucket, id, label) VALUES (0, 1, 'row-1')"),
            10,
        )
        .await
        .expect("insert");
    assert_eq!(stream.schema().fields().len(), 0, "a write has no columns");
    assert!(stream.next_page().await.expect("drain").is_none());
    assert_eq!(stream.rows_affected(), None);
}

// ---------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn pages_a_cursor_without_repeating_or_skipping() {
    let src = fixture().await;
    let mut cursor = src.cursor(&read(), 50).await.expect("cursor");

    let mut seen: Vec<i32> = Vec::new();
    while let Some(batch) = cursor.next_page().await.expect("fetch") {
        seen.extend(ids(&batch));
    }
    assert_eq!(seen.len(), ROWS as usize, "every row once");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "in order, with nothing repeated"
    );
    cursor.close().await.expect("close");
}

/// The property the trait asks a cursor for, against the write that would break
/// it — and the property it does *not* promise, stated in the same test so
/// nobody has to guess which this is.
///
/// Cassandra's paging state is a position in the partition, not a snapshot.
/// Nothing already read comes back and nothing before the position is skipped,
/// which is the whole of what the trait requires. A row inserted *ahead* of the
/// position afterwards is read, because by the time the cursor reaches it, it is
/// simply a row that is there. PostgreSQL's cursor would not have shown it.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_write_between_two_pages_repeats_nothing_and_skips_nothing() {
    let session = seeding_session().await;
    scratch(&session, "paged", 300).await;
    let src = CassandraSource::connect(&format!("cassandra://{NODE}/{KEYSPACE}"))
        .await
        .expect("connect");

    let mut cursor = src
        .cursor(
            &format!("SELECT id FROM {KEYSPACE}.paged WHERE bucket = 0 ORDER BY id"),
            50,
        )
        .await
        .expect("cursor");
    let first = cursor
        .next_page()
        .await
        .expect("fetch")
        .expect("a first page");
    assert_eq!(ids(&first), (1..=50).collect::<Vec<i32>>());

    // Behind the cursor: rewritten rows it has already handed over.
    run(
        &session,
        &format!(
            "UPDATE {KEYSPACE}.paged SET label = 'rewritten' WHERE bucket = 0 AND id IN (1, 2, 3)"
        ),
    )
    .await;
    // Ahead of it: ids that sort after everything the cursor has left to read.
    run(
        &session,
        &format!(
            "BEGIN UNLOGGED BATCH \
             INSERT INTO {KEYSPACE}.paged (bucket, id, label) VALUES (0, 1001, 'late'); \
             INSERT INTO {KEYSPACE}.paged (bucket, id, label) VALUES (0, 1002, 'late'); \
             APPLY BATCH"
        ),
    )
    .await;

    let mut seen = ids(&first);
    while let Some(batch) = cursor.next_page().await.expect("fetch") {
        seen.extend(ids(&batch));
    }

    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "no row was read twice");
    for id in 1..=300 {
        assert!(seen.contains(&id), "row {id} was skipped");
    }
    // The part that is a position rather than a snapshot, asserted rather than
    // apologised for. If Cassandra ever gives a paging state a read view, this
    // fails and the module comment needs rewriting.
    assert!(
        seen.contains(&1001) && seen.contains(&1002),
        "rows written ahead of the position are read, because by then they are there"
    );
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// The deterministic half: a cancel that lands while nothing is in flight still
/// stops the cursor, because a read that has been stopped stays stopped.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_cancel_between_two_pages_stops_the_next_one() {
    let src = fixture().await;
    let mut cursor = src.cursor(&read(), 10).await.expect("cursor");
    cursor
        .next_page()
        .await
        .expect("fetch")
        .expect("a first page");

    cursor.canceller().cancel().await.expect("cancel");
    let error = cursor
        .next_page()
        .await
        .expect_err("a cancelled cursor should not hand over another page");
    assert!(error.is_cancelled(), "got: {error}");
}

/// The half that exercises the `select!`: a cancel arriving while a fetch is
/// parked on the socket. Pages of one over five hundred rows is five hundred
/// round trips, so the twenty milliseconds is not a race this can lose without
/// something else having gone wrong — and if it does drain first, the assertion
/// below fails loudly rather than passing quietly.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_cancel_during_a_fetch_stops_it_where_it_is() {
    let src = fixture().await;
    let mut cursor = src.cursor(&read(), 1).await.expect("cursor");
    let canceller = cursor.canceller();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        canceller.cancel().await.expect("cancel");
    });

    let mut read_before_the_stop = 0usize;
    let error = loop {
        match cursor.next_page().await {
            Ok(Some(batch)) => read_before_the_stop += batch.num_rows(),
            Ok(None) => panic!(
                "the cursor drained all {ROWS} rows before the cancel landed; \
                 it read {read_before_the_stop}"
            ),
            Err(e) => break e,
        }
    };
    assert!(error.is_cancelled(), "got: {error}");
    assert!(
        read_before_the_stop < ROWS as usize,
        "the read should have stopped short, got {read_before_the_stop}"
    );
    // The honest half of the message: the coordinator is still working.
    assert!(
        error.to_string().contains("no server-side cancel"),
        "got: {error}"
    );
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn cancelling_an_idle_cursor_is_not_a_failure() {
    // Delivery is not interruption: pressing Cancel when nothing is running has
    // to succeed, or a front end reports a failure for pressing a button at the
    // wrong moment.
    let src = fixture().await;
    let cursor = src.cursor(&read(), 10).await.expect("cursor");
    cursor.canceller().cancel().await.expect("cancel");
    src.cancel().await.expect("session cancel");
}

/// The trait says a session cancel does not reach a cursor, and here that is a
/// fact about which `Stop` each of them holds rather than something remembered.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_session_cancel_does_not_reach_a_cursor() {
    let src = fixture().await;
    let mut cursor = src.cursor(&read(), 10).await.expect("cursor");
    src.cancel().await.expect("session cancel");
    let batch = cursor
        .next_page()
        .await
        .expect("the cursor is not the session's to cancel")
        .expect("a page");
    assert_eq!(batch.num_rows(), 10);
}

/// A cancelled session takes the results it handed out with it, and leaves the
/// next statement alone.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_session_cancel_stops_its_own_results_and_not_the_next_one() {
    let src = fixture().await;
    let mut stream = src.query(&read(), 10).await.expect("query");
    src.cancel().await.expect("cancel");
    let error = stream.next_page().await.expect_err("stopped");
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

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_broken_statement_says_where_it_stopped() {
    let src = fixture().await;
    let broken = format!("SELECT id FROM {KEYSPACE}.nums WHERE ORDER BY id");
    let error = failure(&src, &broken).await;
    let at = error
        .statement_position()
        .expect("Cassandra reports a position") as usize;
    assert!(at >= 1, "positions count from one, got {at}");
    assert!(at <= broken.chars().count() + 1, "past the end: {at}");
    assert_eq!(
        broken.chars().nth(at - 1),
        Some('O'),
        "the caret should land on ORDER: {error}"
    );
    assert!(!error.is_cancelled(), "a broken statement is not a Cancel");
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn reading_a_table_that_is_not_there_is_a_failure_with_the_servers_own_words() {
    let src = fixture().await;
    let error = failure(
        &src,
        &format!("SELECT * FROM {KEYSPACE}.no_such_relation_anywhere"),
    )
    .await;
    assert!(error.to_string().contains("does not exist"), "got: {error}");
    assert_eq!(
        error.statement_position(),
        None,
        "the server names the table, not a place in the text"
    );
}

/// The statement this driver refuses to write, run to show why.
///
/// `Browse::sql` would have appended the key to an `ORDER BY` so that a browse
/// looks the same twice. Against Cassandra that is not a worse ordering, it is
/// no result at all.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_browse_that_ordered_by_the_key_would_be_refused_outright() {
    let src = fixture().await;
    let refused = format!(r#"SELECT * FROM "{KEYSPACE}"."nums" ORDER BY "id""#);
    let error = failure(&src, &refused).await;
    assert!(
        error.to_string().contains("ORDER BY"),
        "the server should object to the ordering itself: {error}"
    );

    // And the statement this driver writes instead, run to the end.
    let keys = ["id".to_string()];
    let statement = src.browse(&Browse {
        schema: KEYSPACE,
        relation: "nums",
        filter: None,
        order: None,
        keys: &keys,
        limit: Some(3),
    });
    assert_eq!(
        statement,
        format!(r#"SELECT * FROM "{KEYSPACE}"."nums" LIMIT 3"#)
    );
    let mut stream = Driver::query(&src, &statement, 10).await.expect("browse");
    let mut rows = 0;
    while let Some(batch) = stream.next_batch().await.expect("browse") {
        rows += batch.num_rows();
    }
    assert_eq!(rows, 3);
}

/// A filter on a column that is not part of the key is refused by Cassandra
/// telling the user to add `ALLOW FILTERING`, and that refusal is the useful
/// answer: it says the query would read every partition in the table. A driver
/// that appended the clause itself would turn a warning into a full scan nobody
/// asked for.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn a_filter_on_a_column_that_is_not_a_key_is_refused_by_the_server() {
    let src = fixture().await;
    let statement = src.browse(&Browse {
        schema: KEYSPACE,
        relation: "kinds",
        filter: Some("words = 'hello'"),
        order: None,
        keys: &[],
        limit: None,
    });
    let error = failure(&src, &statement).await;
    assert!(
        error.to_string().contains("ALLOW FILTERING"),
        "the server's own advice should reach the user: {error}"
    );

    // With the clause the user chose to add, it is an ordinary statement.
    let mut stream = Driver::query(&src, &format!("{statement} ALLOW FILTERING"), 10)
        .await
        .expect("with the clause");
    assert!(stream.next_batch().await.expect("rows").is_some());
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn the_navigator_finds_the_keyspace_and_its_tables() {
    let src = fixture().await;
    let schemas = src.schemas().await.expect("schemas");
    assert!(schemas.iter().any(|s| s.name == KEYSPACE));
    // The system keyspaces are listed too, because the catalog every other call
    // reads lives in one of them.
    assert!(schemas.iter().any(|s| s.name == "system_schema"));

    let relations = src.relations(KEYSPACE).await.expect("relations");
    let nums = relations
        .iter()
        .find(|r| r.name == "nums")
        .expect("nums should be listed");
    assert_eq!(nums.kind, dbconn::RelationKind::Table);
    assert_eq!(nums.schema, KEYSPACE);
    assert_eq!(nums.estimated_rows, None, "nothing has measured this");
    // A table is not a view, which is what the structure pane hangs a section on.
    assert_eq!(src.definition(KEYSPACE, "nums").await.unwrap(), None);
}

/// The catalog answers alphabetically and numbers each kind of column from zero
/// on its own; this is the reordering that turns that into what a reader
/// expects.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn the_key_columns_come_first_and_are_numbered_from_one() {
    let src = fixture().await;
    let columns = src.columns(KEYSPACE, "nums").await.expect("columns");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    // Alphabetically this would be bucket, id, label — which happens to be the
    // same. `kinds` below is where the two orders differ.
    assert_eq!(names, vec!["bucket", "id", "label"]);
    for (at, column) in columns.iter().enumerate() {
        assert_eq!(column.position, at as i32 + 1);
        assert!(!column.data_type.is_empty(), "a column states its own type");
    }
    assert!(columns[0].is_primary_key, "bucket is the partition key");
    assert!(columns[1].is_primary_key, "id is the clustering column");
    assert!(!columns[2].is_primary_key);
    assert!(!columns[0].nullable, "a key column cannot hold a null");
    assert!(columns[2].nullable);

    // `id` is the partition key of `kinds` and sorts well after most of its
    // other columns, so a driver that took the catalog's order would put it
    // somewhere in the middle.
    let kinds = src.columns(KEYSPACE, "kinds").await.expect("columns");
    assert_eq!(kinds[0].name, "id");
    assert!(kinds[0].is_primary_key);
    assert!(kinds[1..].iter().all(|c| !c.is_primary_key));
    assert_eq!(
        kinds
            .iter()
            .find(|c| c.name == "props")
            .expect("props")
            .data_type,
        "map<text, int>",
        "the declared type, not the Arrow one"
    );
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn an_index_reports_the_expression_it_is_on() {
    let src = fixture().await;
    let indexes = src.indexes(KEYSPACE, "nums").await.expect("indexes");
    let label = indexes
        .iter()
        .find(|i| i.name == "nums_label")
        .unwrap_or_else(|| panic!("the seeded index should be listed, got {indexes:?}"));
    assert_eq!(label.columns, vec!["label"]);
    assert!(!label.is_unique, "no Cassandra index is unique");
    assert!(!label.is_primary, "the primary key is not an index here");
    assert!(!label.method.is_empty());
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn the_calls_this_database_has_no_answer_for_are_empty_rather_than_broken() {
    let src = fixture().await;
    assert!(src.foreign_keys(KEYSPACE, "nums").await.unwrap().is_empty());
    assert!(
        src.referenced_by(KEYSPACE, "nums")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(src.constraints(KEYSPACE, "nums").await.unwrap().is_empty());
    // Not structurally empty — this one asks the server, and the answer for a
    // table with no triggers is nothing.
    assert!(src.triggers(KEYSPACE, "nums").await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn asking_about_a_table_that_is_not_there_is_an_empty_answer() {
    // A navigator works from a tree that can be one refresh out of date, so this
    // happens in ordinary use and must not put an error on screen.
    let src = fixture().await;
    let missing = "no_such_relation_anywhere";
    assert!(src.columns(KEYSPACE, missing).await.unwrap().is_empty());
    assert!(src.indexes(KEYSPACE, missing).await.unwrap().is_empty());
    assert!(src.triggers(KEYSPACE, missing).await.unwrap().is_empty());
    assert_eq!(src.definition(KEYSPACE, missing).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn transaction_control_is_refused_by_name() {
    let src = fixture().await;
    assert!(!src.capabilities().transactional);
    // The one the whole field exists for: Cancel here stops this side's reads and
    // the coordinator never hears about it.
    assert!(!src.capabilities().cancel_stops_the_statement);
    for step in [
        dbconn::TxStep::Begin,
        dbconn::TxStep::Commit,
        dbconn::TxStep::Rollback,
        dbconn::TxStep::Savepoint("halfway".to_string()),
    ] {
        let error = src
            .transaction(&step)
            .await
            .err()
            .unwrap_or_else(|| panic!("{step:?} should be refused"));
        assert!(error.to_string().contains("Cassandra"), "got: {error}");
    }
}

// ---------------------------------------------------------------------------
// What this environment cannot do, stated so it is not mistaken for a gap
// ---------------------------------------------------------------------------

/// Why there is no materialized-view fixture.
///
/// `materialized_views_enabled` is a `cassandra.yaml` setting with no `nodetool`
/// or CQL equivalent, and the stock image ships it off. The branch that reports
/// a view was verified by hand against a container with the setting flipped; see
/// the header of this file. When a server that allows them turns up, this test
/// fails and the fixture is worth writing.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn the_stock_image_refuses_a_materialized_view() {
    let session = seeding_session().await;
    let _ = fixture().await;
    let error = session
        .query_unpaged(
            format!(
                "CREATE MATERIALIZED VIEW IF NOT EXISTS {KEYSPACE}.nums_by_label AS \
                 SELECT bucket, id, label FROM {KEYSPACE}.nums \
                 WHERE bucket IS NOT NULL AND id IS NOT NULL AND label IS NOT NULL \
                 PRIMARY KEY ((bucket), label, id)"
            ),
            &[],
        )
        .await
        .expect_err("the stock image has materialized views disabled");
    assert!(
        error
            .to_string()
            .contains("Materialized views are disabled"),
        "got: {error}"
    );
}

/// Why `CassandraSource::connect` installs an address translator.
///
/// The driver crate builds a node's address as its advertised IP paired with the
/// *contact point's* port, so a container published as `-p 59042:9042`
/// advertises its bridge IP and the driver dials that IP on 59042 — where
/// nothing listens. The control connection succeeds, so `build()` returns and
/// the first statement is what fails.
///
/// Pinned so that the day the driver crate stops doing this, the workaround in
/// `OneEndpoint` becomes a failing test rather than dead code nobody dares
/// remove.
#[tokio::test]
#[ignore = "requires a Cassandra server"]
async fn without_translation_the_driver_crate_cannot_reach_a_published_container() {
    let session = SessionBuilder::new()
        .known_node(NODE)
        .build()
        .await
        .expect("the control connection reaches the published port");
    let error = session
        .query_unpaged("SELECT release_version FROM system.local", &[])
        .await
        .expect_err("the node pool dials an address nothing listens on");
    assert!(
        error.to_string().contains("pool"),
        "the failure should be the connection pool, got: {error}"
    );
}
