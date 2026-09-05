//! The Arrow Flight SQL driver against a live server.
//!
//! Marked `ignore`, so `cargo test` passes with nothing installed. To run them:
//!
//! ```text
//! make db-up-flightsql
//! cargo test -p driver-flightsql -- --ignored
//! make db-down-flightsql
//! ```
//!
//! The server is the Arrow project's own example server over DuckDB, which is the
//! only kind worth testing a client against: one written here would agree with
//! this client by construction. It ships a small TPC-H database, so the read-only
//! fixture needs no seeding and cannot drift. The few tests that write make their
//! own table through `arrow-flight`'s reference client rather than through the
//! driver, so a fixture never depends on the code under test being right.
//!
//! Three of these tests are about the server rather than about the driver, and
//! are here on purpose. `neither_cancel_action_is_implemented_by_this_server`,
//! `the_action_list_advertises_more_than_this_server_implements` and
//! `two_statements_at_once_are_more_than_this_server_can_do` are the crate
//! comment's claims made falsifiable: the day this server grows a working
//! `CancelFlightInfo`, the first fails, and somebody wires `cancel` up to it
//! instead of stopping the read on this side.
//!
//! **Every test here takes `ONE_AT_A_TIME` first, and that is about the server.**
//! The Arrow example server keeps one DuckDB connection for the whole process, so
//! a second statement starting while a first is producing rows invalidates the
//! first — "Attempting to execute an unsuccessful or closed pending query result",
//! from the reader that was there first. Measured: of two concurrent readers one
//! failed, of four two failed, of eight four failed, and it happens on two
//! separate connections exactly as it does on one.
//! `two_statements_at_once_are_more_than_this_server_can_do` is that measurement
//! kept, so the lock is a recorded fact rather than a superstition, and so a
//! server that fixes it turns the lock into something removable rather than
//! something nobody dares touch.

use arrow::array::Array;
use arrow_flight::sql::ProstMessageExt;
use arrow_flight::sql::client::FlightSqlServiceClient;
use arrow_flight::{Action, CancelFlightInfoRequest, Empty};
use dbconn::{Browse, Driver, TxStep};
use driver_flightsql::{FlightSqlSource, Rows};
use futures_util::TryStreamExt;
use prost::Message;
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Channel;

const URL: &str = "flightsql://flight_username:flight@127.0.0.1:51337/";
const GRPC: &str = "http://127.0.0.1:51337";
/// The catalog and schema the image's TPC-H tables live in, as the protocol
/// reports them and as this driver flattens them.
const SCHEMA: &str = "TPC-H-small.main";

/// One statement at a time against this server; see the header.
///
/// A cross-process lock rather than a `Mutex`, because `cargo nextest` runs each
/// test in its own process: a mutex inside one of them holds nothing back, and
/// the server answers concurrent readers with exactly the failure the header
/// describes.
const ONE_AT_A_TIME: &str = "flightsql";

async fn source() -> FlightSqlSource {
    FlightSqlSource::connect(URL)
        .await
        .expect("Flight SQL unreachable; see the header of this file")
}

/// A client that is not the code under test, for seeding and for asking the
/// server questions this driver does not ask it.
async fn reference() -> FlightSqlServiceClient<Channel> {
    let channel = Channel::from_static(GRPC)
        .connect()
        .await
        .expect("Flight SQL unreachable; see the header of this file");
    let mut client = FlightSqlServiceClient::new(channel);
    client
        .handshake("flight_username", "flight")
        .await
        .expect("the server refused the reference client's credentials");
    client
}

/// Runs `sql` through the reference client, reading it to the end.
async fn seed(client: &mut FlightSqlServiceClient<Channel>, sql: &str) {
    let info = client
        .execute(sql.to_string(), None)
        .await
        .unwrap_or_else(|e| panic!("seeding failed on {sql}: {e}"));
    let ticket = info.endpoint[0]
        .ticket
        .clone()
        .unwrap_or_else(|| panic!("no ticket for {sql}"));
    let _: Vec<arrow::array::RecordBatch> = client
        .do_get(ticket)
        .await
        .unwrap_or_else(|e| panic!("seeding failed on {sql}: {e}"))
        .try_collect()
        .await
        .unwrap_or_else(|e| panic!("seeding failed on {sql}: {e}"));
}

/// A table of this test's own, emptied and refilled through the reference client.
///
/// One per test that writes, named after it, because `cargo test` runs them in
/// parallel and a shared scratch table would turn a scheduling accident into a
/// failure.
async fn scratch(table: &str, rows: i32) -> FlightSqlServiceClient<Channel> {
    let mut client = reference().await;
    seed(&mut client, &format!("DROP TABLE IF EXISTS {table}")).await;
    seed(
        &mut client,
        &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, label VARCHAR)"),
    )
    .await;
    if rows > 0 {
        seed(
            &mut client,
            &format!(
                "INSERT INTO {table} SELECT i, 'row-' || i FROM range(1, {}) t(i)",
                rows + 1
            ),
        )
        .await;
    }
    client
}

/// The failure `sql` produces, insisting there is one.
///
/// Either call can be the one that fails: `GetFlightInfo` prepares the statement
/// and `DoGet` runs it, so a broken statement is refused by the first and a
/// statement that fails while running is refused by the second.
async fn failure(source: &FlightSqlSource, sql: &str) -> dbconn::DbError {
    match Driver::query(source, sql, 10).await {
        Err(e) => e,
        Ok(mut stream) => match stream.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {sql}"),
        },
    }
}

/// Every buffer of every column of `batch`, and whether it points inside `body`.
fn buffers_inside(batch: &arrow::array::RecordBatch, body: &std::ops::Range<usize>) -> Vec<bool> {
    batch
        .columns()
        .iter()
        .flat_map(|column| {
            column
                .to_data()
                .buffers()
                .iter()
                .map(|buffer| {
                    let at = buffer.as_ptr() as usize;
                    body.contains(&at)
                })
                .collect::<Vec<bool>>()
        })
        .collect()
}

/// Whether a column's type has a buffer arrow will move to align it.
///
/// The offsets of a `Utf8` are 4-byte integers and get realigned; its characters
/// are bytes and never do. That difference is the whole shape of what this driver
/// can and cannot promise, so the test states it rather than counting.
fn is_variable_width(data_type: &arrow::datatypes::DataType) -> bool {
    use arrow::datatypes::DataType;
    matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary
    )
}

// ---------------------------------------------------------------------------
// The claim this driver was written to test
// ---------------------------------------------------------------------------

/// A batch handed to the caller is the bytes the socket read, not a copy of them.
///
/// The central claim of this driver, made falsifiable. `wire_body` is the address
/// range of the gRPC message the page was decoded out of, and every buffer of the
/// page has to point inside it — except where the body did not land on an 8-byte
/// boundary, in which case Arrow realigns the fixed-width buffers and leaves the
/// variable-width ones where they are. Both halves are checked, so the test fails
/// whether somebody reintroduces a whole-body copy (arrow-flight's own decoder
/// does exactly that) or rebuilds the arrays column by column.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_batch_reaches_the_caller_in_the_bytes_the_socket_read() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    // Far fewer rows per page than the server puts in a message, so a page is a
    // slice of one arrival rather than a concatenation of two — which is what
    // makes `wire_body` answer at all.
    let mut rows: Rows = source
        .query("SELECT * FROM lineitem LIMIT 20000", 100)
        .await
        .expect("query failed");

    let (mut aligned, mut misaligned, mut assembled) = (0, 0, 0);
    for page in 1..=200 {
        let batch = rows
            .next_page()
            .await
            .expect("batch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        let Some(body) = rows.wire_body() else {
            // A page that straddled two arrivals, which the next test is about.
            assembled += 1;
            continue;
        };
        let placed = buffers_inside(&batch, &body);

        if body.start % 8 == 0 {
            // Nothing needed moving, so nothing should have been.
            aligned += 1;
            assert!(
                placed.iter().all(|inside| *inside),
                "an aligned body still had {} of {} buffers moved out of it",
                placed.iter().filter(|inside| !**inside).count(),
                placed.len()
            );
        } else {
            // Arrow realigns the fixed-width buffers and only those. A
            // variable-width values buffer needs one-byte alignment, so nothing
            // about where the body landed can justify moving it — and a copy of
            // the whole body, which is what arrow-flight's own decoder makes,
            // would move it.
            misaligned += 1;
            for (column, field) in batch.columns().iter().zip(batch.schema().fields()) {
                if !is_variable_width(field.data_type()) {
                    continue;
                }
                let data = column.to_data();
                let values = data.buffers().last().expect("a values buffer");
                assert!(
                    body.contains(&(values.as_ptr() as usize)),
                    "{}'s characters were copied, and nothing about alignment asks for that",
                    field.name()
                );
            }
        }
    }
    // Vacuous for a page with no body to point at, so at least one page has to
    // have had one. Which way a body lands is the gRPC framing's business and not
    // this side's, so the split is printed rather than asserted.
    assert!(
        aligned + misaligned > 0,
        "every page was assembled from several arrivals, so nothing was checked"
    );
    println!("pages: {aligned} aligned, {misaligned} misaligned, {assembled} assembled");
}

/// Most of a result reaches the caller without being touched, counted in bytes.
///
/// The test above says no buffer is copied without a reason; this one says what
/// that is worth. A page the size of a server message is exactly one arrival, so
/// every buffer can be attributed to its body and the bytes add up to the whole
/// transfer. Asserting a majority rather than a number keeps it a regression test
/// — reintroduce a copy anywhere in `decode` and the in-place figure goes to zero
/// — without pinning it to a page size or a server version.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn most_of_a_result_reaches_the_caller_without_being_touched() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut rows = source
        .query("SELECT * FROM lineitem", 2048)
        .await
        .expect("query failed");

    let (mut in_place, mut moved) = (0usize, 0usize);
    let (mut aligned, mut misaligned) = (0, 0);
    while let Some(batch) = rows.next_page().await.expect("batch error") {
        let body = rows
            .wire_body()
            .expect("a page of a whole arrival names its body");
        if body.start % 8 == 0 {
            aligned += 1;
        } else {
            misaligned += 1;
        }
        // Counted in bytes rather than in buffers, because that is the question a
        // grid cares about: a `Utf8` column's characters are most of its weight and
        // its offsets are a twentieth of it, and one buffer each would say the
        // opposite of what happened.
        for column in batch.columns() {
            for buffer in column.to_data().buffers() {
                let at = buffer.as_ptr() as usize;
                if body.contains(&at) {
                    in_place += buffer.len();
                } else {
                    moved += buffer.len();
                }
            }
        }
    }
    assert!(
        in_place > moved,
        "only {in_place} of {} bytes reached the caller in the bytes the socket read",
        in_place + moved
    );
    println!(
        "arrivals: {aligned} aligned, {misaligned} misaligned; \
         bytes: {in_place} in place, {moved} moved ({:.0}% in place)",
        100.0 * in_place as f64 / (in_place + moved).max(1) as f64
    );
}

/// A page assembled from two arrivals says so, because assembling one is a copy.
///
/// The other half of the claim: the one place this driver moves data is the carry
/// buffer, and it is honest about it. Asking for more rows than the server puts in
/// a message is what makes it happen.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_page_assembled_from_two_arrivals_points_at_neither() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut rows = source
        .query("SELECT l_orderkey FROM lineitem LIMIT 20000", 5000)
        .await
        .expect("query failed");
    let batch = rows
        .next_page()
        .await
        .expect("batch error")
        .expect("a first page");
    assert_eq!(batch.num_rows(), 5000);
    assert!(
        rows.wire_body().is_none(),
        "a page concatenated out of several arrivals has no single body to name"
    );
}

/// arrow-flight's own decoder copies the whole body, which is why this driver has
/// one of its own.
///
/// Not a test of this driver: a test of the reason it exists. It reads the same
/// statement through `FlightSqlServiceClient::do_get` — the obvious way — and
/// shows that no buffer of the resulting batch points into the body it arrived in,
/// because `utils::flight_data_to_arrow_batch` only ever sees a `&FlightData` and
/// so can only do `Buffer::from(&[u8])`. The day arrow-flight takes the owned
/// `Bytes` instead, this fails and forty lines of `Rows::pull` can go.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn the_reference_decoder_copies_every_body_it_reads() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    use arrow_flight::decode::{DecodedPayload, FlightDataDecoder};

    let mut client = reference().await;
    let info = client
        .execute("SELECT * FROM lineitem LIMIT 4000".to_string(), None)
        .await
        .expect("execute");
    let ticket = info.endpoint[0].ticket.clone().expect("a ticket");
    let mut request = tonic::Request::new(ticket);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", client.token().expect("a token"))
            .parse()
            .expect("a header value"),
    );
    let stream = client
        .inner_mut()
        .do_get(request)
        .await
        .expect("do_get")
        .into_inner();

    let mut decoder = FlightDataDecoder::new(stream.map_err(Into::into));
    let mut checked = 0;
    while let Some(decoded) = decoder.try_next().await.expect("decode") {
        let body = decoded.inner.data_body.clone();
        let at = body.as_ptr() as usize;
        let range = at..at + body.len();
        if let DecodedPayload::RecordBatch(batch) = decoded.payload {
            checked += 1;
            assert!(
                buffers_inside(&batch, &range).iter().all(|inside| !*inside),
                "arrow-flight's decoder used to copy the body and now does not; \
                 this driver's own decode can go"
            );
        }
    }
    assert!(checked > 0, "the statement produced no batches to check");
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A result arrives in batches of the size the caller asked for, whatever size
/// the server sends.
///
/// The server's messages hold 2048 rows here, which is DuckDB's vector size and
/// not the caller's number. The carry is what makes the page size the caller's.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_result_arrives_in_batches_of_the_size_that_was_asked_for() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut stream = Driver::query(&source, "SELECT o_orderkey FROM orders", 300)
        .await
        .expect("query failed");
    // Before a row has been read: the schema is the first message of the stream,
    // so a front end can lay out a grid immediately.
    assert!(stream.schema().field_with_name("o_orderkey").is_ok());
    assert_eq!(stream.rows_affected(), None);

    for page in 1..=5 {
        let batch = stream
            .next_batch()
            .await
            .expect("batch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        assert_eq!(batch.num_rows(), 300, "page {page}");
    }
}

/// The last page is short and the count is only known once it is in.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_row_count_arrives_only_when_the_result_has_been_read_to_the_end() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut stream = Driver::query(&source, "SELECT n_nationkey FROM nation", 10)
        .await
        .expect("query failed");
    let mut seen = stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a first page")
        .num_rows();
    // Zero is a real answer — an UPDATE that matched nothing — so "not finished"
    // has to be something else, and a result with rows still in it is not
    // finished.
    assert_eq!(stream.rows_affected(), None);

    while let Some(batch) = stream.next_batch().await.expect("batch error") {
        seen += batch.num_rows();
    }
    assert_eq!(seen, 25, "TPC-H has 25 nations");
    assert_eq!(stream.rows_affected(), Some(25));
}

/// A cursor pages forward without repeating or skipping.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_cursor_pages_forward_without_repeating_or_skipping() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut cursor = Driver::cursor(
        &source,
        "SELECT o_orderkey FROM orders ORDER BY o_orderkey",
        50,
    )
    .await
    .expect("cursor failed");
    assert!(cursor.schema().field_with_name("o_orderkey").is_ok());

    let mut keys = Vec::new();
    for page in 1..=4 {
        let batch = cursor
            .fetch()
            .await
            .expect("fetch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        assert_eq!(batch.num_rows(), 50);
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .expect("o_orderkey arrives as a 64-bit integer here");
        keys.extend((0..column.len()).map(|i| column.value(i)));
    }
    assert_eq!(keys.len(), 200);
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 200, "a page was repeated");
    assert_eq!(sorted, keys, "the pages arrived out of order");

    cursor.close().await.expect("close failed");
}

/// A write goes through the query path, and the count is a row of the result.
///
/// `CommandStatementUpdate` over `DoPut` is the protocol's dedicated path and is
/// deliberately not used: on this server its `record_count` reports the size of
/// the result set rather than the rows changed, so a five-row delete answers 1.
/// The `Count` column below is the true number, and the second half of this test
/// pins the difference so that a server which fixes it is noticed.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_write_reports_the_engines_own_count_as_a_row() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let table = "flightsql_write";
    let mut client = scratch(table, 0).await;
    let source = source().await;

    let mut stream = Driver::query(
        &source,
        &format!("INSERT INTO {table} SELECT i, 'row-' || i FROM range(1, 6) t(i)"),
        100,
    )
    .await
    .expect("the insert did not run");
    let batch = stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a write answers with its count");
    assert_eq!(batch.num_rows(), 1);
    let count = batch
        .column_by_name("Count")
        .expect("the engine names the column Count")
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("a count is an integer");
    assert_eq!(count.value(0), 5, "five rows were written");
    // Rows produced, which is the only number this driver can know without
    // parsing the statement. The five is in the row above.
    while stream.next_batch().await.expect("batch error").is_some() {}
    assert_eq!(stream.rows_affected(), Some(1));

    let counted = client
        .execute_update(format!("DELETE FROM {table}"), None)
        .await
        .expect("the reference client's update path");
    assert_eq!(
        counted, 1,
        "this server's CommandStatementUpdate counts the result set and not the rows changed; \
         if that is fixed, this driver should use it"
    );

    seed(&mut client, &format!("DROP TABLE {table}")).await;
}

/// A statement with no result set runs and produces nothing.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_statement_with_no_rows_still_names_its_columns() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let table = "flightsql_ddl";
    let mut client = reference().await;
    seed(&mut client, &format!("DROP TABLE IF EXISTS {table}")).await;

    let source = source().await;
    let mut stream = Driver::query(&source, &format!("CREATE TABLE {table} (n INTEGER)"), 10)
        .await
        .expect("the create did not run");
    // The stream still opens with a schema message, so `schema()` answers rather
    // than failing — which is what a front end laying out a grid needs.
    assert_eq!(stream.schema().fields().len(), 1);
    assert!(stream.next_batch().await.expect("batch error").is_none());
    assert_eq!(stream.rows_affected(), Some(0));

    seed(&mut client, &format!("DROP TABLE {table}")).await;
}

/// A broken statement fails with the server's own words and no caret.
///
/// No position, and it is a decision rather than a gap: the `LINE 1: … ^` in the
/// message is DuckDB's prose, and what is behind a Flight SQL server is not
/// knowable from the protocol. Parsing it would put a caret in the right place
/// against this server and wherever the text happened to reach against the next
/// one.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_broken_statement_carries_the_servers_words_and_no_position() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let error = failure(
        &source,
        "SELECT o_orderkey FROM orders WHERE ORDER BY o_orderkey",
    )
    .await;
    let message = error.to_string();
    assert!(
        message.contains("syntax error"),
        "the server's own words should survive: {message}"
    );
    assert!(
        !message.contains("Tonic") && !message.contains("status:"),
        "the transport should not be apologising in front of the database: {message}"
    );
    assert_eq!(error.statement_position(), None);
    assert!(!error.is_cancelled());
}

/// Reading a relation that is not there is a failure, and the server says which.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn reading_a_relation_that_is_not_there_is_a_failure() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let error = failure(&source, "SELECT * FROM no_such_relation_anywhere").await;
    assert!(
        error.to_string().contains("no_such_relation_anywhere"),
        "got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Cancellation, which this server has none of
// ---------------------------------------------------------------------------

/// This server implements neither of the protocol's two cancel actions.
///
/// The crate comment's claim, made falsifiable. Both are asked for here through
/// the reference client, because this driver does not send them — and the day one
/// of them works, this test fails and `cancel` should send it instead of stopping
/// the read on this side.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn neither_cancel_action_is_implemented_by_this_server() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let mut client = reference().await;
    let info = client
        .execute("SELECT * FROM lineitem".to_string(), None)
        .await
        .expect("execute");

    // Each in the body its own action is specified with: `CancelFlightInfo`
    // takes the request message bare, and the older `CancelQuery` takes it
    // wrapped in a protobuf `Any`, which is how every Flight SQL action is sent.
    let bodies = [
        (
            "CancelFlightInfo",
            CancelFlightInfoRequest::new(info.clone()).encode_to_vec(),
        ),
        (
            "CancelQuery",
            arrow_flight::sql::ActionCancelQueryRequest {
                info: info.encode_to_vec().into(),
            }
            .as_any()
            .encode_to_vec(),
        ),
    ];
    for (name, body) in bodies {
        let action = Action {
            r#type: name.to_string(),
            body: body.into(),
        };
        let refused = match client.do_action(action).await {
            Err(e) => e.to_string(),
            Ok(mut results) => match results.message().await {
                Err(status) => status.message().to_string(),
                Ok(_) => panic!("{name} works now; wire cancel up to it"),
            },
        };
        assert!(
            refused.contains("not implemented"),
            "{name} answered something new: {refused}"
        );
    }
}

/// Two statements at once are more than this server can do.
///
/// The reason every test in this file takes `ONE_AT_A_TIME`, kept as a
/// measurement rather than a habit. The example server holds one DuckDB
/// connection for the whole process, so a second statement starting while a first
/// is still producing rows invalidates the first — and it happens on two separate
/// connections exactly as it does on one, which is what rules out anything this
/// driver could do about it. Asked through the reference client, so it is the
/// server being measured and not this code.
///
/// The day the server grows a connection per session this fails, and the lock at
/// the top of every other test can go.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn two_statements_at_once_are_more_than_this_server_can_do() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;

    let mut readers = Vec::new();
    for _ in 0..4 {
        readers.push(tokio::spawn(async {
            let mut client = reference().await;
            let info = client
                .execute("SELECT * FROM lineitem LIMIT 10000".to_string(), None)
                .await?;
            let ticket = info.endpoint[0].ticket.clone().expect("a ticket");
            let batches: Vec<arrow::array::RecordBatch> =
                client.do_get(ticket).await?.try_collect().await?;
            Ok::<usize, arrow_flight::error::FlightError>(
                batches.iter().map(|b| b.num_rows()).sum(),
            )
        }));
    }

    let mut collided = 0;
    for reader in readers {
        if let Err(e) = reader.await.expect("join") {
            assert!(
                e.to_string().contains("pending query result")
                    || e.to_string().contains("Unexpected error in RPC handling"),
                "a reader failed for some new reason: {e}"
            );
            collided += 1;
        }
    }
    assert!(
        collided > 0,
        "four concurrent readers all succeeded; this server can do more than one \
         statement at a time now, and ONE_AT_A_TIME can go"
    );
}

/// The action list advertises four things this server has not implemented.
///
/// `ListActions` here is the base implementation's advertisement rather than the
/// server's capability, which is worth pinning: a client that offered a button
/// per advertised action would offer four that fail.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn the_action_list_advertises_more_than_this_server_implements() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let mut client = reference().await;
    let mut request = tonic::Request::new(Empty {});
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", client.token().expect("a token"))
            .parse()
            .expect("a header value"),
    );
    let advertised: Vec<String> = client
        .inner_mut()
        .list_actions(request)
        .await
        .expect("list_actions")
        .into_inner()
        .map_ok(|action| action.r#type)
        .try_collect()
        .await
        .expect("collect");

    for name in [
        "CancelFlightInfo",
        "CancelQuery",
        "BeginSavepoint",
        "EndSavepoint",
    ] {
        assert!(
            advertised.iter().any(|a| a == name),
            "{name} is no longer advertised: {advertised:?}"
        );
    }
    // And the two that are real, so this is a statement about the gap rather than
    // about the list being wrong throughout.
    assert!(advertised.iter().any(|a| a == "BeginTransaction"));
    assert!(advertised.iter().any(|a| a == "EndTransaction"));
}

/// A cancel between two pages stops the next one.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_cancel_between_two_pages_stops_the_next_one() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut stream = Driver::query(&source, "SELECT * FROM lineitem", 100)
        .await
        .expect("query failed");
    stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a first page");

    source.cancel().await.expect("cancel failed");
    let error = stream
        .next_batch()
        .await
        .expect_err("the read was stopped, so the next page is a failure");
    assert!(error.is_cancelled(), "got: {error}");
    assert!(
        error.to_string().contains("CancelFlightInfo"),
        "the message should say what it could not do: {error}"
    );
}

/// A cancel arriving while a read is running stops it where it is.
///
/// The page already buffered is not handed over either, which is the part a
/// driver gets wrong by checking the stop only inside the fetch: a caller
/// draining a cursor in a loop would otherwise see one more page after they asked
/// for none.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_cancel_during_a_read_stops_it_where_it_is() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = Arc::new(source().await);
    let mut stream = Driver::query(source.as_ref(), "SELECT * FROM lineitem", 100)
        .await
        .expect("query failed");

    // From another task, because that is the situation: `cancel` has to be able
    // to arrive while the call it interrupts is still in flight.
    let pressing = Arc::clone(&source);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(2)).await;
        pressing.cancel().await.expect("cancel failed");
    });

    let mut seen = 0;
    loop {
        match stream.next_batch().await {
            Ok(Some(batch)) => seen += batch.num_rows(),
            Ok(None) => panic!(
                "the whole of lineitem was read in under the time the cancel took to arrive; \
                 {seen} rows"
            ),
            Err(e) => {
                assert!(e.is_cancelled(), "got: {e}");
                assert!(seen > 0, "nothing was read before the cancel");
                break;
            }
        }
    }
}

/// A session cancel does not reach a cursor, and a cursor's canceller does not
/// reach the session.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_session_cancel_and_a_cursor_do_not_reach_each_other() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let mut cursor = Driver::cursor(&source, "SELECT * FROM lineitem", 100)
        .await
        .expect("cursor failed");
    let mut stream = Driver::query(&source, "SELECT * FROM lineitem", 100)
        .await
        .expect("query failed");

    source.cancel().await.expect("cancel failed");
    assert!(
        stream.next_batch().await.is_err(),
        "the session's own result should be stopped"
    );
    cursor
        .fetch()
        .await
        .expect("a cursor is outside the session's cancel")
        .expect("a page");

    cursor.canceller().cancel().await.expect("cancel failed");
    assert!(
        cursor.fetch().await.is_err(),
        "the cursor's own canceller should stop it"
    );
}

/// Cancelling an idle cursor is a no-op rather than an error.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn cancelling_an_idle_cursor_is_not_a_failure() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let cursor = Driver::cursor(&source, "SELECT n_nationkey FROM nation", 10)
        .await
        .expect("cursor failed");
    cursor.canceller().cancel().await.expect("cancel failed");
    source.cancel().await.expect("session cancel failed");
}

// ---------------------------------------------------------------------------
// The navigator, answered by the protocol's own commands
// ---------------------------------------------------------------------------

/// The navigator's three levels come from `CommandGetDbSchemas` and
/// `CommandGetTables`, and no SQL is sent.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn the_navigator_is_answered_by_the_protocols_own_commands() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;

    let schemas = source.schemas().await.expect("schemas failed");
    assert!(
        schemas.iter().any(|s| s.name == SCHEMA),
        "the catalog and the schema are one name here: {schemas:?}",
        schemas = schemas.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let relations = source.relations(SCHEMA).await.expect("relations failed");
    for expected in ["customer", "lineitem", "nation", "orders", "region"] {
        let found = relations
            .iter()
            .find(|r| r.name == expected)
            .unwrap_or_else(|| panic!("{expected} should be listed under {SCHEMA}"));
        assert_eq!(found.schema, SCHEMA);
        assert_eq!(found.kind, dbconn::RelationKind::Table);
        // The protocol carries no row count for a relation, and declining to
        // answer is not the same as answering zero.
        assert_eq!(found.estimated_rows, None);
    }
}

/// A view is told apart from a table by the protocol's own `table_type`.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_view_is_told_apart_from_a_table() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let view = "flightsql_view";
    let mut client = reference().await;
    seed(&mut client, &format!("DROP VIEW IF EXISTS {view}")).await;
    seed(
        &mut client,
        &format!("CREATE VIEW {view} AS SELECT n_name FROM nation"),
    )
    .await;

    let source = source().await;
    let relations = source.relations(SCHEMA).await.expect("relations failed");
    let found = relations
        .iter()
        .find(|r| r.name == view)
        .expect("the view should be listed");
    assert_eq!(found.kind, dbconn::RelationKind::View);
    // And no definition, because the protocol has no command that carries one.
    // A driver reaching into `information_schema` for it would be a DuckDB driver
    // that happens to speak Flight SQL.
    assert_eq!(source.definition(SCHEMA, view).await.unwrap(), None);

    seed(&mut client, &format!("DROP VIEW {view}")).await;
}

/// A relation's columns come out of the Arrow schema the protocol carries.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_relations_columns_come_from_the_schema_the_protocol_carries() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let columns = source
        .columns(SCHEMA, "nation")
        .await
        .expect("columns failed");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["n_nationkey", "n_name", "n_regionkey", "n_comment"],
        "the columns arrive in the order the schema declares them"
    );
    for (at, column) in columns.iter().enumerate() {
        assert_eq!(column.position, at as i32 + 1, "one-based and ascending");
        assert!(!column.data_type.is_empty());
        // No column defaults: the protocol carries none, and inventing one would
        // mean asking the engine.
        assert_eq!(column.default_value, None);
    }
    // The Arrow type, because this server sets no `ARROW:FLIGHT:SQL:TYPE_NAME`
    // on its fields. Against a Flight SQL server the Arrow type is what the
    // database states, since it is what the values will arrive as.
    assert_eq!(columns[0].data_type, "Int32");
    assert_eq!(columns[1].data_type, "Utf8");
}

/// A primary key is read from `CommandGetPrimaryKeys` and marked on the columns.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_primary_key_is_read_from_the_protocol_and_not_from_the_engine() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let table = "flightsql_keys";
    let mut client = reference().await;
    seed(&mut client, &format!("DROP TABLE IF EXISTS {table}")).await;
    seed(
        &mut client,
        &format!("CREATE TABLE {table} (a INTEGER, b INTEGER, label VARCHAR, PRIMARY KEY (a, b))"),
    )
    .await;

    let source = source().await;
    let columns = source.columns(SCHEMA, table).await.expect("columns failed");
    let keys: Vec<&str> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(keys, ["a", "b"], "a key of two columns is two columns");

    // And the TPC-H tables, which declare none, are not given one.
    let nation = source
        .columns(SCHEMA, "nation")
        .await
        .expect("columns failed");
    assert!(nation.iter().all(|c| !c.is_primary_key));

    seed(&mut client, &format!("DROP TABLE {table}")).await;
}

/// A foreign key is reported from both ends, with the protocol's own actions.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_foreign_key_is_reported_from_both_ends() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let parent = "flightsql_parent";
    let child = "flightsql_child";
    let mut client = reference().await;
    seed(&mut client, &format!("DROP TABLE IF EXISTS {child}")).await;
    seed(&mut client, &format!("DROP TABLE IF EXISTS {parent}")).await;
    seed(
        &mut client,
        &format!("CREATE TABLE {parent} (id INTEGER PRIMARY KEY)"),
    )
    .await;
    seed(
        &mut client,
        &format!(
            "CREATE TABLE {child} (id INTEGER PRIMARY KEY, parent INTEGER REFERENCES {parent}(id))"
        ),
    )
    .await;

    let source = source().await;
    let outbound = source
        .foreign_keys(SCHEMA, child)
        .await
        .expect("foreign keys failed");
    assert_eq!(outbound.len(), 1, "one key out of {child}");
    assert_eq!(outbound[0].local_columns, ["parent"]);
    assert_eq!(outbound[0].other_table, parent);
    assert_eq!(outbound[0].other_columns, ["id"]);
    assert_eq!(outbound[0].other_schema, SCHEMA);
    // The protocol numbers the referential actions and this driver reports the
    // words `FlightSql.proto` numbers them with. This server sends 1 for both,
    // which is RESTRICT — DuckDB's foreign keys are checked at statement end and
    // have no ON DELETE clause to declare, so RESTRICT is what it always is.
    assert_eq!(outbound[0].on_delete, "RESTRICT");
    assert_eq!(outbound[0].on_update, "RESTRICT");

    let inbound = source
        .referenced_by(SCHEMA, parent)
        .await
        .expect("inbound references failed");
    assert_eq!(inbound.len(), 1, "one key into {parent}");
    assert_eq!(inbound[0].local_columns, ["id"]);
    assert_eq!(inbound[0].other_table, child);
    assert_eq!(inbound[0].other_columns, ["parent"]);

    seed(&mut client, &format!("DROP TABLE {child}")).await;
    seed(&mut client, &format!("DROP TABLE {parent}")).await;
}

/// Three metadata calls answer with nothing because the protocol has no command
/// to ask with.
///
/// Not a gap in the driver. Flight SQL's `CommandGet…` set is catalogs, schemas,
/// tables, table types, keys, cross references, SQL info and XDBC type info —
/// there is nothing in it about an index, a constraint or a trigger. A driver
/// that answered these from `information_schema` would work against this server
/// and no other.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn what_the_protocol_cannot_ask_is_answered_with_nothing() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    assert!(source.indexes(SCHEMA, "orders").await.unwrap().is_empty());
    assert!(
        source
            .constraints(SCHEMA, "orders")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(source.triggers(SCHEMA, "orders").await.unwrap().is_empty());
}

/// `CommandGetXdbcTypeInfo` is not implemented by this server.
///
/// Recorded because it is the one metadata command in the protocol that this
/// server refuses, and because a driver that used it to name column types would
/// fail here and nowhere else. Asked through the reference client, since this
/// driver does not send it.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn the_type_information_command_is_not_implemented_by_this_server() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let mut client = reference().await;
    let error = client
        .get_xdbc_type_info(arrow_flight::sql::CommandGetXdbcTypeInfo { data_type: None })
        .await
        .expect_err("this server does not implement it");
    assert!(
        error.to_string().contains("not implemented"),
        "got: {error}"
    );
}

/// Asking about a relation that is not there is an empty answer, not a failure.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn asking_about_a_relation_that_is_not_there_is_an_empty_answer() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let missing = "no_such_relation_anywhere";
    assert!(source.columns(SCHEMA, missing).await.unwrap().is_empty());
    assert!(source.indexes(SCHEMA, missing).await.unwrap().is_empty());
    assert!(
        source
            .foreign_keys(SCHEMA, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .referenced_by(SCHEMA, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(source.definition(SCHEMA, missing).await.unwrap(), None);
}

/// A schema that is not there is an empty list of relations, not a failure.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_schema_that_is_not_there_lists_nothing() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    assert!(
        source
            .relations("no_such_catalog.no_such_schema")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The URL's catalog restricts the navigator to it.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_catalog_in_the_connection_string_restricts_the_navigator() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    // The one the tables are in, percent-encoded because the name has no
    // characters a URL objects to but the next server's might.
    let named = FlightSqlSource::connect(&format!("{URL}TPC-H-small"))
        .await
        .expect("connect");
    assert_eq!(named.catalog(), "TPC-H-small");
    let schemas = named.schemas().await.expect("schemas failed");
    assert!(schemas.iter().all(|s| s.name.starts_with("TPC-H-small.")));

    // And a catalog that is not there shows nothing rather than failing — which
    // is what this server's ignored `catalog` filter would otherwise produce,
    // since it answers with every schema it has whatever is asked for.
    let elsewhere = FlightSqlSource::connect(&format!("{URL}no_such_catalog"))
        .await
        .expect("connect");
    assert!(
        elsewhere
            .schemas()
            .await
            .expect("schemas failed")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Transactions, which are a token here rather than a connection
// ---------------------------------------------------------------------------

/// A transaction holds across statements with no connection held back.
///
/// The thing this driver does that no other one here can: nothing is pinned, and
/// two statements are in the same transaction because they carry the same handle.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_transaction_holds_without_a_connection_held_back() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let table = "flightsql_tx";
    let mut client = scratch(table, 0).await;
    let outside = source().await;
    let source = source().await;

    source
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");
    Driver::query(&source, &format!("INSERT INTO {table} VALUES (1, 'a')"), 10)
        .await
        .expect("the insert did not run")
        .next_batch()
        .await
        .expect("batch error");

    assert_eq!(
        rows(&source, &format!("SELECT id FROM {table}")).await,
        1,
        "an open transaction should see its own change"
    );
    assert_eq!(
        rows(&outside, &format!("SELECT id FROM {table}")).await,
        0,
        "another session should not"
    );

    source
        .transaction(&TxStep::Rollback)
        .await
        .expect("could not roll back");
    assert_eq!(
        rows(&source, &format!("SELECT id FROM {table}")).await,
        0,
        "a rolled-back change should be gone"
    );

    seed(&mut client, &format!("DROP TABLE {table}")).await;
}

/// A savepoint is refused rather than skipped.
///
/// The protocol has `BeginSavepoint` and `EndSavepoint`, this driver sends them,
/// and this server answers `Unimplemented`. Refusing is the whole point: a client
/// that quietly did nothing would leave somebody believing there is a point they
/// can come back to.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_savepoint_is_refused_rather_than_skipped() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    source
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");
    let refused = source
        .transaction(&TxStep::Savepoint("halfway".to_string()))
        .await
        .expect_err("this server has no savepoints");
    assert!(
        refused.to_string().contains("not implemented"),
        "the server's own refusal should reach the caller: {refused}"
    );
    // And refusing one leaves the transaction usable, or the refusal has cost
    // more than the missing feature.
    assert_eq!(rows(&source, "SELECT 1").await, 1);
    source
        .transaction(&TxStep::Rollback)
        .await
        .expect("could not roll back");
}

/// Committing with nothing open is refused rather than sent.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn ending_a_transaction_that_was_never_begun_is_refused() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let refused = source
        .transaction(&TxStep::Commit)
        .await
        .expect_err("there is nothing to commit");
    assert!(refused.to_string().contains("no transaction"), "{refused}");
}

// ---------------------------------------------------------------------------
// Browse
// ---------------------------------------------------------------------------

/// The statement a navigator writes for a table is one this server runs.
///
/// The catalog level is what makes this worth running rather than comparing to an
/// expected string: `TPC-H-small` has to be quoted and `main` must not be, and a
/// name quoted whole would address a schema that does not exist.
#[tokio::test]
#[ignore = "requires an Arrow Flight SQL server"]
async fn a_browse_of_a_table_in_a_named_catalog_runs() {
    let _turn = dbfixture::exclusive(ONE_AT_A_TIME).await;
    let source = source().await;
    let keys = ["n_nationkey".to_string()];
    let statement = source.browse(&Browse {
        schema: SCHEMA,
        relation: "nation",
        filter: Some("n_regionkey = 1"),
        order: None,
        keys: &keys,
        limit: Some(3),
    });
    assert!(
        statement.contains(r#""TPC-H-small""#),
        "the catalog needs quoting: {statement}"
    );

    let mut stream = Driver::query(&source, &statement, 10)
        .await
        .unwrap_or_else(|e| panic!("the browse statement did not run: {statement}: {e}"));
    let batch = stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("a browse of a table with rows should produce a batch");
    assert_eq!(batch.num_rows(), 3, "the row ceiling should be honoured");
    assert!(batch.schema().field_with_name("n_nationkey").is_ok());
}

/// How many rows `sql` returns.
async fn rows(source: &FlightSqlSource, sql: &str) -> usize {
    let mut stream = Driver::query(source, sql, 100)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut seen = 0;
    while let Some(batch) = stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        seen += batch.num_rows();
    }
    seen
}
