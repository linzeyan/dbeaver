//! What only a live Redis server settles.
//!
//! Everything here needs a server and is `#[ignore]`d; `make db-up-redis` starts
//! one and `make db-check-redis` says whether it is there. The unit suite covers
//! the parsing and the rendering, which need nothing running.
//!
//! Each test owns one numbered database and clears it first, because a Redis
//! server has sixteen and no way to make a seventeenth — so the tests cannot
//! each have a namespace of their own the way the SQL drivers' fixtures do.
//! Database 9 is left alone: it belongs to `dbconn`'s contract suite, which
//! `cargo test --workspace -- --ignored` runs at the same time as this binary.

use arrow::array::{Array, RecordBatch, StringArray};
use dbconn::{Browse, Driver, TxStep};
use driver_redis::{KeyType, RedisSource, TYPES};
use std::sync::Arc;
use std::time::Duration;

fn url(db: u8) -> String {
    format!("redis://127.0.0.1:56379/{db}")
}

async fn raw(db: u8) -> redis::aio::MultiplexedConnection {
    redis::Client::open(url(db))
        .expect("the fixture URL should parse")
        .get_multiplexed_async_connection()
        .await
        .expect("Redis unreachable; run `make db-up-redis`")
}

/// An empty database and a driver connected to it.
async fn fixture(db: u8) -> (RedisSource, redis::aio::MultiplexedConnection) {
    let mut conn = raw(db).await;
    redis::cmd("FLUSHDB")
        .exec_async(&mut conn)
        .await
        .expect("could not clear the fixture");
    let source = RedisSource::connect(&url(db))
        .await
        .expect("the driver could not connect");
    (source, conn)
}

/// Every batch a statement produces.
async fn run(source: &RedisSource, statement: &str) -> Vec<RecordBatch> {
    let mut stream = source
        .query(statement, 100)
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"));
    let mut out = Vec::new();
    while let Some(batch) = stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
    {
        out.push(batch);
    }
    out
}

fn cell(batch: &RecordBatch, column: &str, row: usize) -> Option<String> {
    let values = batch
        .column_by_name(column)
        .unwrap_or_else(|| panic!("no column {column}"))
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| panic!("{column} is not text"));
    (!values.is_null(row)).then(|| values.value(row).to_string())
}

fn names(batch: &RecordBatch) -> Vec<String> {
    batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

/// One key of every type, so that a browse of each relation has exactly one row.
async fn seed_one_of_each(conn: &mut redis::aio::MultiplexedConnection) {
    let mut seed = redis::pipe();
    seed.cmd("SET").arg("s").arg("hello").ignore();
    seed.cmd("HSET")
        .arg("h")
        .arg("born")
        .arg("1815")
        .arg("name")
        .arg("ada")
        .ignore();
    seed.cmd("RPUSH")
        .arg("l")
        .arg("first")
        .arg("second")
        .ignore();
    seed.cmd("SADD").arg("t").arg("only").ignore();
    seed.cmd("ZADD")
        .arg("z")
        .arg(1)
        .arg("alpha")
        .arg(2)
        .arg("beta")
        .ignore();
    seed.cmd("XADD")
        .arg("x")
        .arg("1-1")
        .arg("temp")
        .arg("21")
        .ignore();
    seed.exec_async(conn).await.expect("seeding the fixture");
}

/// The exit criterion, checked against a real server: a browse of each relation
/// produces exactly the columns that relation declares, and the value is the
/// type-aware rendering rather than a stringified reply.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn every_value_type_browses_with_the_columns_its_relation_declares() {
    let (source, mut conn) = fixture(1).await;
    seed_one_of_each(&mut conn).await;

    for of in TYPES {
        let statement = source.browse(&Browse {
            schema: "db1",
            relation: of.name(),
            filter: None,
            order: None,
            keys: &[],
            limit: None,
        });
        let batches = run(&source, &statement).await;
        assert_eq!(
            batches.len(),
            1,
            "one key of type {} means one batch: {statement}",
            of.name()
        );
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1, "{}", of.name());

        let declared: Vec<String> = source
            .columns("db1", of.name())
            .await
            .expect("columns")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names(batch),
            declared,
            "the grid and the structure pane disagree about {}",
            of.name()
        );

        // Every key here was written without an expiry, so the TTL column is
        // there and empty rather than holding Redis's -1 sentinel.
        assert!(
            batch.column_by_name("ttl").expect("ttl").is_null(0),
            "{}",
            of.name()
        );
        if of.is_collection() {
            assert!(
                batch.column_by_name("size").is_some(),
                "{} is a collection and should report its size",
                of.name()
            );
        }
    }
}

/// The values themselves, which is the half of "type-aware value display" that
/// the column names do not cover.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_collection_arrives_as_the_json_its_type_deserves() {
    let (source, mut conn) = fixture(2).await;
    seed_one_of_each(&mut conn).await;

    let value = |of: KeyType| {
        let source = &source;
        async move {
            let statement = format!("SELECT 2\nSCAN 0 TYPE {}", of.name());
            let batches = run(source, &statement).await;
            cell(&batches[0], "value", 0).expect("a value")
        }
    };

    assert_eq!(value(KeyType::String).await, "hello");
    assert_eq!(
        value(KeyType::Hash).await,
        r#"{"born":"1815","name":"ada"}"#
    );
    assert_eq!(value(KeyType::List).await, r#"["first","second"]"#);
    assert_eq!(value(KeyType::Set).await, r#"["only"]"#);
    // A sorted set keeps its rank order and its scores, which RESP3 sends as
    // real doubles rather than as strings.
    assert_eq!(
        value(KeyType::ZSet).await,
        r#"[{"member":"alpha","score":1.0},{"member":"beta","score":2.0}]"#
    );
    assert_eq!(
        value(KeyType::Stream).await,
        r#"[{"fields":{"temp":"21"},"id":"1-1"}]"#
    );
}

/// The surprise that changes what a browse statement looks like: `SCAN` without a
/// cursor is not a shorter spelling of `SCAN 0`, it is an error.
///
/// Sent straight to the server rather than through the driver, because the
/// driver refuses it first. Pinned here so that a server which ever starts
/// accepting it is noticed rather than assumed.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn scan_requires_the_cursor_the_browse_statement_writes() {
    // Any connection will do: this asks the server about syntax and reads
    // nothing, so it does not need a database of its own.
    let mut conn = raw(0).await;
    let refused = redis::cmd("SCAN")
        .arg("MATCH")
        .arg("*")
        .arg("TYPE")
        .arg("hash")
        .query_async::<redis::Value>(&mut conn)
        .await
        .expect_err("the cursor is a required argument");
    assert_eq!(refused.code(), Some("ERR"), "{refused}");
    assert!(
        refused.to_string().contains("cursor"),
        "expected the server to name the cursor: {refused}"
    );
}

/// The assumption `read_keys` rests on, pinned against the library and the
/// server rather than against the documentation.
///
/// A browse reads every key on a page in one pipeline, and a key can change type
/// between the `SCAN` that listed it and the read. Without `ignore_errors` that
/// one key would fail the whole page; with it the failed command comes back as an
/// element of the reply and the rest of the page survives. There is no way to
/// provoke the race deliberately through the driver, so it is provoked here
/// directly: a `WRONGTYPE` beside two commands that work.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_failed_command_in_a_pipeline_leaves_the_others_alone() {
    // Self-contained, down to writing its own key: this is the one test with no
    // database of its own — sixteen is all there are — so it must not depend on
    // anything outliving the pipeline it is about.
    let mut conn = raw(0).await;
    let key = "pipeline:probe";

    let mut pipeline = redis::pipe();
    pipeline.ignore_errors();
    pipeline.cmd("SET").arg(key).arg("hello");
    pipeline.cmd("GET").arg(key);
    // A string is not a list, so this one is refused by the server.
    pipeline.cmd("LRANGE").arg(key).arg(0).arg(-1);
    pipeline.cmd("TTL").arg(key);
    let replies: Vec<redis::Value> = pipeline
        .query_async(&mut conn)
        .await
        .expect("ignore_errors should not fail the pipeline");
    assert_eq!(replies.len(), 4, "every command answers: {replies:?}");
    assert_eq!(replies[1], redis::Value::BulkString(b"hello".to_vec()));
    assert!(
        matches!(replies[2], redis::Value::ServerError(_)),
        "the refusal arrives in its own place: {:?}",
        replies[2]
    );
    assert_eq!(replies[3], redis::Value::Int(-1));

    // And without it, one refusal takes the whole page with it — which is the
    // behaviour the browse cannot afford.
    let mut strict = redis::pipe();
    strict.cmd("SET").arg(key).arg("hello");
    strict.cmd("LRANGE").arg(key).arg(0).arg(-1);
    strict
        .query_async::<Vec<redis::Value>>(&mut conn)
        .await
        .expect_err("one refusal fails the pipeline");

    redis::cmd("DEL")
        .arg(key)
        .exec_async(&mut conn)
        .await
        .expect("clearing up after a test with no database of its own");
}

/// What RESP3 buys, checked end to end: a map reply is two columns and not a
/// flat list somebody has to know comes in pairs.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_map_reply_is_two_columns_and_an_array_reply_is_one() {
    let (source, mut conn) = fixture(4).await;
    seed_one_of_each(&mut conn).await;

    let batches = run(&source, "HGETALL h").await;
    assert_eq!(names(&batches[0]), vec!["field", "value"]);
    assert_eq!(batches[0].num_rows(), 2);

    let batches = run(&source, "LRANGE l 0 -1").await;
    assert_eq!(names(&batches[0]), vec!["value"]);
    assert_eq!(batches[0].num_rows(), 2);

    // A reply that is one thing is one row, and a nil is an empty cell rather
    // than no rows at all.
    let batches = run(&source, "GET nosuchkey").await;
    assert_eq!(batches[0].num_rows(), 1);
    assert_eq!(cell(&batches[0], "value", 0), None);
}

/// The cursor the contract subject declines to claim, paging a real keyspace.
///
/// It works, and this is where that is checked — the contract's `cursors: false`
/// says the trait's *guarantee* is not met, not that there is no cursor. The keys
/// are distinct here because nothing writes to the database during the
/// iteration, which is exactly the condition `SCAN` needs and cannot promise.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn the_cursor_pages_the_keyspace_forward() {
    let (source, mut conn) = fixture(5).await;
    let mut seed = redis::pipe();
    for i in 1..=500 {
        seed.cmd("SET").arg(format!("nums:{i}")).arg(i).ignore();
    }
    seed.exec_async(&mut conn).await.expect("seeding");

    let mut cursor = source
        .cursor("SELECT 5\nSCAN 0 TYPE string", 50)
        .await
        .expect("cursor");
    assert!(cursor.schema().field_with_name("key").is_ok());

    let mut seen: Vec<String> = Vec::new();
    while let Some(batch) = cursor.fetch().await.expect("fetch") {
        for row in 0..batch.num_rows() {
            seen.push(cell(&batch, "key", row).expect("a key"));
        }
    }
    assert_eq!(
        seen.len(),
        500,
        "every key exactly once over a quiet keyspace"
    );
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 500, "no key was returned twice");

    cursor.close().await.expect("close");
}

/// The measurement behind `Reach::stop` preferring `CLIENT UNBLOCK`.
///
/// A blocking command is the one case Redis can interrupt without ending the
/// connection, and this is what that buys: the statement fails as a
/// cancellation, the server's own `UNBLOCKED` is what says so, and the session
/// connection is still there for the next statement. `CLIENT KILL` would have
/// left the second half of this test connecting again.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn cancelling_a_blocking_command_stops_it_without_ending_the_session() {
    let (source, _conn) = fixture(6).await;
    let source = Arc::new(source);

    let running = Arc::clone(&source);
    let blocked = tokio::spawn(async move { running.query("BLPOP nothing 0", 10).await });
    // Long enough for the command to have reached the server and blocked; the
    // assertion below fails loudly rather than hanging if it has not.
    tokio::time::sleep(Duration::from_millis(200)).await;
    source.cancel().await.expect("cancel should be delivered");

    let outcome = blocked.await.expect("the task should not panic");
    let err = outcome.err().expect("a cancelled statement fails");
    assert!(
        err.is_cancelled(),
        "a cancel should not read as a fault: {err}"
    );
    assert!(
        err.to_string().contains("UNBLOCKED"),
        "the server's own word for it should survive: {err}"
    );

    // The connection is still usable, which is the whole point of preferring
    // UNBLOCK.
    let batches = run(&source, "PING").await;
    assert_eq!(cell(&batches[0], "value", 0).as_deref(), Some("PONG"));
}

/// The other half of the measurement: what a cancel costs when `CLIENT UNBLOCK`
/// cannot help.
///
/// Only a statement that is *between* round trips can be stopped this way, and
/// that is a consequence of Redis being single-threaded rather than a limit of
/// this driver: a command the server is busy with keeps it busy, so the
/// `CLIENT KILL` sent from a second connection is not read until that command
/// has finished. A browse is the case that matters and the case that works — a
/// hundred thousand keys is a thousand `SCAN` calls, and a kill lands between two
/// of them.
///
/// What it costs is the connection. The statement fails as a cancellation rather
/// than as a fault, and the next statement opens a new connection — which starts
/// on the database the URL named rather than the one the killed statement had
/// selected. Nothing else is lost, because this driver holds no transaction to
/// lose.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn cancelling_a_browse_ends_the_connection_and_says_it_was_cancelled() {
    let (source, mut conn) = fixture(3).await;
    // Large enough that the browse takes over a second, which is what makes the
    // cancel below land in the middle of it rather than after it.
    for chunk in 0..100 {
        let mut seed = redis::pipe();
        for i in 0..1000 {
            seed.cmd("SET")
                .arg(format!("k:{chunk}:{i}"))
                .arg(i)
                .ignore();
        }
        seed.exec_async(&mut conn).await.expect("seeding");
    }

    let source = Arc::new(source);
    let running = Arc::clone(&source);
    let browsing =
        tokio::spawn(async move { running.query("SELECT 3\nSCAN 0 TYPE string", 100).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    source.cancel().await.expect("cancel should be delivered");

    let err = browsing
        .await
        .expect("the task should not panic")
        .err()
        .expect("a cancelled browse fails");
    assert!(
        err.is_cancelled(),
        "a cancel should not read as a fault: {err}"
    );
    assert!(
        !err.to_string().contains("UNBLOCKED"),
        "a browse is not blocked, so this is the kill and not the unblock: {err}"
    );

    // The session is usable again, on a connection opened in place of the one
    // that was killed.
    let batches = run(&source, "PING").await;
    assert_eq!(cell(&batches[0], "value", 0).as_deref(), Some("PONG"));
}

/// Cancelling with nothing running succeeds and destroys nothing.
///
/// The contract requires this of an idle cursor, and it matters more here than
/// anywhere else: a `CLIENT KILL` aimed at an idle connection would end a session
/// nobody asked to end.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn cancelling_an_idle_session_changes_nothing() {
    let (source, mut conn) = fixture(7).await;
    seed_one_of_each(&mut conn).await;

    source.cancel().await.expect("an idle cancel succeeds");
    let batches = run(&source, "GET s").await;
    assert_eq!(cell(&batches[0], "value", 0).as_deref(), Some("hello"));

    let cursor = source
        .cursor("SELECT 7\nSCAN 0 TYPE string", 10)
        .await
        .expect("cursor");
    cursor.canceller().cancel().await.expect("an idle cursor");
}

/// The grammar, checked where it matters: a statement's earlier lines really do
/// take effect on the connection the last line runs on.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_statement_reaches_the_database_its_first_line_selects() {
    let (source, mut here) = fixture(8).await;
    redis::cmd("SET")
        .arg("marker")
        .arg("db8")
        .exec_async(&mut here)
        .await
        .expect("seeding");
    let mut there = raw(10).await;
    redis::cmd("FLUSHDB")
        .exec_async(&mut there)
        .await
        .expect("clearing");
    redis::cmd("SET")
        .arg("marker")
        .arg("db10")
        .exec_async(&mut there)
        .await
        .expect("seeding");

    // The connection opened on db8, and the statement reads db10.
    let batches = run(&source, "SELECT 10\nGET marker").await;
    assert_eq!(cell(&batches[0], "value", 0).as_deref(), Some("db10"));

    // And a statement that does not select reads where the last one left the
    // connection, which is Redis's own behaviour and worth pinning rather than
    // discovering.
    let batches = run(&source, "GET marker").await;
    assert_eq!(cell(&batches[0], "value", 0).as_deref(), Some("db10"));
}

/// A failing command names the line it was on, which is the only position Redis
/// makes available.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_command_that_fails_points_at_its_own_line() {
    let (source, _conn) = fixture(11).await;
    let statement = "SELECT 11\nWIBBLE nums";
    let err = source
        .query(statement, 10)
        .await
        .err()
        .expect("WIBBLE is not a command");
    assert_eq!(err.statement_position(), Some(11), "{err}");
    assert!(!err.is_cancelled(), "{err}");
    // The server's sentence, not redis-rs's category for it.
    assert!(
        err.to_string().starts_with("ERR unknown command"),
        "got: {err}"
    );

    // And a WRONGTYPE, which is the failure a user meets most often here.
    redis::cmd("SET")
        .arg("s")
        .arg("hello")
        .exec_async(&mut raw(11).await)
        .await
        .expect("seeding");
    let err = source
        .query("LRANGE s 0 -1", 10)
        .await
        .err()
        .expect("a string is not a list");
    assert!(err.to_string().starts_with("WRONGTYPE"), "got: {err}");
    // One line, so there is no position worth reporting.
    assert_eq!(err.statement_position(), None, "{err}");
}

/// `COUNT` is the row ceiling this driver reads it as, and the unbounded browse
/// is what shows the ceiling is doing the stopping.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn the_count_on_a_browse_is_the_number_of_rows_it_stops_at() {
    let (source, mut conn) = fixture(12).await;
    let mut seed = redis::pipe();
    for i in 1..=100 {
        seed.cmd("SET").arg(format!("nums:{i}")).arg(i).ignore();
    }
    seed.exec_async(&mut conn).await.expect("seeding");

    let rows = |limit: Option<u32>| {
        let source = &source;
        async move {
            let statement = source.browse(&Browse {
                schema: "db12",
                relation: "string",
                filter: None,
                order: None,
                keys: &[],
                limit,
            });
            run(source, &statement)
                .await
                .iter()
                .map(RecordBatch::num_rows)
                .sum::<usize>()
        }
    };

    assert_eq!(rows(Some(3)).await, 3);
    assert_eq!(rows(Some(40)).await, 40);
    assert_eq!(rows(None).await, 100, "no ceiling reads the whole keyspace");
}

/// The filter bar becomes `MATCH`, as typed.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn the_filter_becomes_the_pattern_the_user_typed() {
    let (source, mut conn) = fixture(13).await;
    let mut seed = redis::pipe();
    for i in 1..=10 {
        seed.cmd("SET").arg(format!("user:{i}")).arg(i).ignore();
        seed.cmd("SET").arg(format!("order:{i}")).arg(i).ignore();
    }
    seed.exec_async(&mut conn).await.expect("seeding");

    let statement = source.browse(&Browse {
        schema: "db13",
        relation: "string",
        filter: Some("user:*"),
        order: None,
        keys: &[],
        limit: None,
    });
    assert_eq!(statement, "SELECT 13\nSCAN 0 MATCH user:* TYPE string");
    let rows: usize = run(&source, &statement)
        .await
        .iter()
        .map(RecordBatch::num_rows)
        .sum();
    assert_eq!(rows, 10);
}

/// A `SCAN` that names no type lists whatever is there, and says what each row
/// is.
///
/// The one listing whose rows disagree with each other, and the reason it has a
/// `type` column that a typed browse does not.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_scan_with_no_type_says_what_each_key_holds() {
    let (source, mut conn) = fixture(14).await;
    seed_one_of_each(&mut conn).await;

    let batches = run(&source, "SELECT 14\nSCAN 0").await;
    assert_eq!(
        names(&batches[0]),
        vec!["key", "ttl", "type", "size", "value"]
    );
    let mut found: Vec<String> = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            found.push(cell(batch, "type", row).expect("a type"));
        }
    }
    found.sort();
    assert_eq!(
        found,
        vec!["hash", "list", "set", "stream", "string", "zset"]
    );
}

/// A TTL is seconds, and a key without one leaves the cell empty rather than
/// showing Redis's -1.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_ttl_is_the_seconds_left_and_nothing_where_there_is_no_expiry() {
    let (source, mut conn) = fixture(15).await;
    let mut seed = redis::pipe();
    seed.cmd("SET").arg("forever").arg("x").ignore();
    seed.cmd("SET")
        .arg("fleeting")
        .arg("x")
        .arg("EX")
        .arg(600)
        .ignore();
    seed.exec_async(&mut conn).await.expect("seeding");

    let batches = run(&source, "SELECT 15\nSCAN 0 TYPE string").await;
    let batch = &batches[0];
    let ttls: Vec<(String, Option<i64>)> = (0..batch.num_rows())
        .map(|row| {
            let key = cell(batch, "key", row).expect("a key");
            let column = batch
                .column_by_name("ttl")
                .expect("ttl")
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .expect("int64");
            (key, (!column.is_null(row)).then(|| column.value(row)))
        })
        .collect();
    for (key, ttl) in ttls {
        match key.as_str() {
            "forever" => assert_eq!(ttl, None, "no expiry is an empty cell"),
            "fleeting" => assert!(
                matches!(ttl, Some(seconds) if (1..=600).contains(&seconds)),
                "expected seconds left, got {ttl:?}"
            ),
            other => panic!("unexpected key {other}"),
        }
    }
}

/// Rows are counted once the result has been read to the end, and not before.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn a_result_reports_its_rows_only_once_it_has_been_read() {
    let (source, mut conn) = fixture(0).await;
    let mut seed = redis::pipe();
    for i in 1..=10 {
        seed.cmd("SET").arg(format!("nums:{i}")).arg(i).ignore();
    }
    seed.exec_async(&mut conn).await.expect("seeding");

    let mut stream = source
        .query("SELECT 0\nSCAN 0 TYPE string", 4)
        .await
        .expect("query");
    assert_eq!(stream.rows_affected(), None);
    let mut seen = 0;
    while let Some(batch) = stream.next_batch().await.expect("batch") {
        seen += batch.num_rows();
    }
    assert_eq!(seen, 10);
    assert_eq!(stream.rows_affected(), Some(10));
}

/// Every step of transaction control is refused by name.
///
/// Connected rather than seeded: this touches no data, so it shares a database
/// with another test instead of clearing one out from under it.
#[tokio::test]
#[ignore = "requires a Redis server"]
async fn transaction_control_is_refused_rather_than_quietly_skipped() {
    let source = RedisSource::connect(&url(0))
        .await
        .expect("the driver could not connect");
    assert!(!source.transactional());
    for step in [
        TxStep::Begin,
        TxStep::Commit,
        TxStep::Rollback,
        TxStep::Savepoint("halfway".to_string()),
        TxStep::RollbackTo("halfway".to_string()),
        TxStep::Release("halfway".to_string()),
    ] {
        let err = source
            .transaction(&step)
            .await
            .err()
            .unwrap_or_else(|| panic!("{step:?} should be refused"));
        assert!(err.to_string().contains("MULTI"), "got: {err}");
    }
}
