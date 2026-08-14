//! End-to-end correctness against the benchmark database.
//!
//! Phase 0 measured throughput; these tests check the values that throughput is
//! moving are actually right. Fast and wrong is worthless, and a type-conversion
//! bug in the Arrow path would not show up in any timing number.
//!
//! Requires `make db-seed`. Run with `make test-integration`.

use arrow::array::{
    Array, BooleanArray, Date32Array, Decimal128Array, Int16Array, Int32Array, Int64Array,
    StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use driver_postgres::{ConstraintKind, PgError, PgSource, RelationKind};
use std::sync::Arc;

const CONN: &str = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench";

async fn connect() -> PgSource {
    PgSource::connect(CONN)
        .await
        .expect("benchmark database unreachable; run `make db-seed`")
}

/// Reads the first `n` rows in id order as a single batch.
async fn first_rows(n: usize) -> arrow::array::RecordBatch {
    let src = connect().await;
    let sql = format!("SELECT * FROM bench_wide WHERE id <= {n} ORDER BY id");
    let mut stream = src.query(&sql, 8192).await.expect("query failed");
    stream
        .next_batch()
        .await
        .expect("batch error")
        .expect("expected at least one batch")
}

fn col<'a, T: 'static>(batch: &'a arrow::array::RecordBatch, name: &str) -> &'a T {
    let idx = batch.schema().index_of(name).expect("column missing");
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column {name} has unexpected array type"))
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn schema_maps_every_column_type() {
    let batch = first_rows(1).await;
    let schema = batch.schema();
    assert_eq!(schema.fields().len(), 20, "bench_wide has 20 columns");

    let expected = [
        ("id", DataType::Int32),
        ("big_val", DataType::Int64),
        ("int_val", DataType::Int32),
        // As declared: numeric(18,4). Reporting every NUMERIC at one normalized
        // scale would make the schema describe a column the table does not have.
        ("num_val", DataType::Decimal128(18, 4)),
        ("real_val", DataType::Float32),
        ("dbl_val", DataType::Float64),
        ("name", DataType::Utf8),
        ("hash_hex", DataType::Utf8),
        ("payload", DataType::Utf8),
        ("category", DataType::Utf8),
        ("flag", DataType::Boolean),
        (
            "created_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
        ),
        ("created_on", DataType::Date32),
        ("created_time", DataType::Time64(TimeUnit::Microsecond)),
        ("uuid_val", DataType::Utf8),
        ("small_val", DataType::Int16),
        ("nullable_text", DataType::Utf8),
        ("nullable_int", DataType::Int32),
        ("json_val", DataType::Utf8),
        ("bytes_val", DataType::Binary),
    ];

    for (name, dt) in expected {
        let f = schema.field_with_name(name).expect("column missing");
        assert_eq!(f.data_type(), &dt, "type mapping for {name}");
        assert!(f.is_nullable(), "{name} should be nullable");
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn integer_and_text_values_round_trip() {
    let batch = first_rows(100).await;
    assert_eq!(batch.num_rows(), 100);

    let id = col::<Int32Array>(&batch, "id");
    let big = col::<Int64Array>(&batch, "big_val");
    let name = col::<StringArray>(&batch, "name");
    let category = col::<StringArray>(&batch, "category");
    let small = col::<Int16Array>(&batch, "small_val");
    let flag = col::<BooleanArray>(&batch, "flag");

    let categories = ["alpha", "beta", "gamma", "delta"];
    for i in 0..100usize {
        let g = i as i64 + 1;
        assert_eq!(id.value(i) as i64, g, "id at row {i}");
        // Widening before multiplying is what keeps this from overflowing in
        // the seed; verify the value survived as int64.
        assert_eq!(big.value(i), g * 7919, "big_val at row {i}");
        assert_eq!(name.value(i), format!("row-{g}"), "name at row {i}");
        assert_eq!(
            category.value(i),
            categories[(g % 4) as usize],
            "category at row {i}"
        );
        assert_eq!(small.value(i) as i64, g % 100, "small_val at row {i}");
        assert_eq!(flag.value(i), g % 2 == 0, "flag at row {i}");
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn nulls_land_in_the_right_rows() {
    let batch = first_rows(100).await;
    let ntext = col::<StringArray>(&batch, "nullable_text");
    let nint = col::<Int32Array>(&batch, "nullable_int");

    for i in 0..100usize {
        let g = i as i64 + 1;
        if g % 17 == 0 {
            assert!(ntext.is_null(i), "nullable_text should be null at id {g}");
        } else {
            assert_eq!(
                ntext.value(i),
                format!("opt-{g}"),
                "nullable_text at id {g}"
            );
        }
        if g % 23 == 0 {
            assert!(nint.is_null(i), "nullable_int should be null at id {g}");
        } else {
            assert_eq!(nint.value(i) as i64, g * 3, "nullable_int at id {g}");
        }
    }

    assert!(ntext.null_count() > 0, "expected some nulls in the sample");
    assert!(nint.null_count() > 0, "expected some nulls in the sample");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn temporal_values_convert_correctly() {
    let batch = first_rows(50).await;
    let on = col::<Date32Array>(&batch, "created_on");
    let at = col::<TimestampMicrosecondArray>(&batch, "created_at");
    let time = col::<Time64MicrosecondArray>(&batch, "created_time");

    // 2020-01-01 is 18262 days after the Unix epoch.
    const BASE_DAYS: i32 = 18262;

    for i in 0..50usize {
        let g = i as i64 + 1;
        let expected_days = BASE_DAYS + (g % 2000) as i32;
        assert_eq!(on.value(i), expected_days, "created_on at row {i}");
        assert_eq!(
            at.value(i),
            expected_days as i64 * 86_400_000_000,
            "created_at at row {i}"
        );
        let expected_micros = ((g % 24) * 3600 + (g % 60) * 60) * 1_000_000;
        assert_eq!(time.value(i), expected_micros, "created_time at row {i}");
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn decimals_keep_their_value_at_fixed_scale() {
    let src = connect().await;
    // Literal values, so the expected result is exact rather than a re-derived
    // approximation of whatever random() produced.
    let mut stream = src
        .query(
            "SELECT 1.5::numeric AS a, (-2.25)::numeric AS b, 0::numeric AS c, \
             12345.6789::numeric AS d",
            8192,
        )
        .await
        .expect("query failed");
    let batch = stream.next_batch().await.unwrap().unwrap();

    let scale = 10i32;
    let unit = 10i128.pow(scale as u32);
    for (name, expected) in [
        ("a", 3 * unit / 2),
        ("b", -9 * unit / 4),
        ("c", 0),
        ("d", 123_456_789 * unit / 10_000),
    ] {
        let a = col::<Decimal128Array>(&batch, name);
        // A cast with no declared precision has no scale to read, so these keep
        // the normalized layout.
        assert_eq!(a.scale(), 10, "undeclared numeric keeps the fallback scale");
        assert_eq!(a.value(0), expected, "decimal column {name}");
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_declared_numeric_arrives_at_its_own_scale() {
    let src = connect().await;
    let mut stream = src
        .query(
            "SELECT revenue FROM reporting.daily_totals ORDER BY day LIMIT 1",
            8192,
        )
        .await
        .expect("query failed");
    let batch = stream.next_batch().await.unwrap().unwrap();

    let revenue = col::<Decimal128Array>(&batch, "revenue");
    // Declared numeric(12,2). Normalizing it to scale 10 would leave the front
    // end unable to tell 1000.00 from 1000, which for a money column is the
    // difference between two column definitions.
    assert_eq!(revenue.precision(), 12);
    assert_eq!(revenue.scale(), 2);
    assert_eq!(revenue.value(0), 100_000, "1000.00 at scale 2");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn batching_splits_without_losing_or_duplicating_rows() {
    let src = connect().await;
    let mut stream = src
        .query(
            "SELECT id FROM bench_wide WHERE id <= 20000 ORDER BY id",
            4096,
        )
        .await
        .expect("query failed");

    let mut seen = 0i64;
    let mut batches = 0;
    while let Some(batch) = stream.next_batch().await.expect("batch error") {
        let id = col::<Int32Array>(&batch, "id");
        for i in 0..batch.num_rows() {
            seen += 1;
            assert_eq!(
                id.value(i) as i64,
                seen,
                "row {seen} out of order or missing"
            );
        }
        batches += 1;
    }

    assert_eq!(seen, 20_000, "every row should arrive exactly once");
    assert_eq!(batches, 5, "20000 rows at 4096 per batch");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn empty_result_yields_no_batch() {
    let src = connect().await;
    let mut stream = src
        .query("SELECT * FROM bench_wide WHERE id < 0", 8192)
        .await
        .expect("query failed");
    assert!(stream.next_batch().await.expect("batch error").is_none());
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn unsupported_column_type_is_rejected_at_prepare() {
    let src = connect().await;
    // `expect_err` would require ArrowStream: Debug; matching keeps the
    // assertion in the test rather than adding a trait impl to satisfy it.
    let err = match src.query("SELECT point(1,2) AS p", 8192).await {
        Ok(_) => panic!("point should not be silently accepted"),
        Err(e) => e,
    };
    match err {
        PgError::UnsupportedType { pg_type, .. } => assert_eq!(pg_type, "point"),
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

/// Runs `sql` for its effect, draining the stream so the command tag lands.
///
/// Returns what the server said it affected. The drain is not incidental: the
/// count arrives with the end of the result, so a caller that stops early gets
/// `None` and has no way to tell that from a statement that touched nothing.
async fn run(src: &PgSource, sql: &str) -> u64 {
    let mut stream = src.query(sql, 8192).await.expect("statement failed");
    while stream.next_batch().await.expect("batch error").is_some() {}
    stream
        .rows_affected()
        .expect("an exhausted statement has reported its count")
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_statement_returning_no_rows_still_runs_and_says_what_it_touched() {
    let src = connect().await;
    // A schema of its own, dropped at the end. The seeded fixtures are shared,
    // and a test that writes to them is a test that breaks somebody's screenshot.
    run(&src, "DROP SCHEMA IF EXISTS script_probe_counts CASCADE").await;
    run(&src, "CREATE SCHEMA script_probe_counts").await;

    // DDL prepares to no columns at all. That is the server's own answer to
    // "does this return rows", and it is what lets a front end tell a statement
    // with an empty result from one that has no result to speak of — the
    // alternative being to guess from the verb it thinks it sees in the SQL.
    let mut created = src
        .query(
            "CREATE TABLE script_probe_counts.t (id int primary key, n int)",
            8192,
        )
        .await
        .expect("CREATE should be accepted, not just SELECT");
    assert!(
        created.schema().fields().is_empty(),
        "a CREATE describes no columns"
    );
    assert!(created.next_batch().await.expect("batch error").is_none());

    // `query` returning `Ok` is not the statement having succeeded. It awaits
    // BindComplete and no further, and Bind is before Execute — so everything a
    // statement can fail at while running (a duplicate relation, a constraint,
    // a division by zero) is still ahead of it. The error arrives out of
    // `next_batch`, which is why a caller that wants to know whether a statement
    // worked has to read the result to the end even when there is nothing in it.
    let mut duplicate = src
        .query("CREATE TABLE script_probe_counts.t (id int)", 8192)
        .await
        .expect("the collision is not detected this early");
    let err = duplicate
        .next_batch()
        .await
        .expect_err("the first CREATE ran, so the second must collide");
    assert!(
        err.to_string().contains("already exists"),
        "expected a duplicate-relation error, got {err}"
    );

    assert_eq!(
        run(
            &src,
            "INSERT INTO script_probe_counts.t SELECT g, g * 2 FROM generate_series(1, 5) g"
        )
        .await,
        5,
        "an INSERT reports the rows it wrote"
    );
    assert_eq!(
        run(
            &src,
            "UPDATE script_probe_counts.t SET n = n + 1 WHERE id <= 3"
        )
        .await,
        3,
        "an UPDATE reports the rows it matched"
    );
    assert_eq!(
        run(&src, "DELETE FROM script_probe_counts.t WHERE id > 100").await,
        0,
        "nothing matched is a real answer, not a missing one"
    );

    // The distinction the front end has to draw: rows returned versus rows
    // affected. A SELECT reports both and they agree; a DELETE has only the
    // second, and calling it a row count would put rows in a grid that has none.
    let mut selected = src
        .query("SELECT id FROM script_probe_counts.t ORDER BY id", 8192)
        .await
        .expect("select failed");
    let batch = selected.next_batch().await.unwrap().unwrap();
    assert_eq!(batch.num_rows(), 5);
    assert!(selected.next_batch().await.unwrap().is_none());
    assert_eq!(selected.rows_affected(), Some(5));

    run(&src, "DROP SCHEMA script_probe_counts CASCADE").await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn statements_share_the_connection_and_the_transaction_the_user_opened() {
    let src = connect().await;
    run(&src, "DROP SCHEMA IF EXISTS script_probe_tx CASCADE").await;
    run(&src, "CREATE SCHEMA script_probe_tx").await;
    run(&src, "CREATE TABLE script_probe_tx.t (id int)").await;

    // Why a script run does not wrap itself in a transaction: it does not have
    // to. Statements go out one after another on one connection, so a BEGIN the
    // user typed opens a block that the statements after it are inside, and a
    // ROLLBACK they typed takes those statements back. Atomicity stays a thing
    // the script asks for rather than a thing the client silently imposes.
    run(&src, "BEGIN").await;
    run(&src, "INSERT INTO script_probe_tx.t VALUES (1)").await;
    run(&src, "ROLLBACK").await;
    let mut after = src
        .query("SELECT id FROM script_probe_tx.t", 8192)
        .await
        .expect("select failed");
    assert!(
        after.next_batch().await.unwrap().is_none(),
        "the rollback the script asked for must actually roll the insert back"
    );

    run(&src, "BEGIN").await;
    run(&src, "INSERT INTO script_probe_tx.t VALUES (2)").await;
    run(&src, "COMMIT").await;
    let mut kept = src
        .query("SELECT id FROM script_probe_tx.t", 8192)
        .await
        .expect("select failed");
    assert_eq!(kept.next_batch().await.unwrap().unwrap().num_rows(), 1);

    run(&src, "DROP SCHEMA script_probe_tx CASCADE").await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_failed_statement_leaves_the_connection_usable() {
    let src = connect().await;
    // Stop-on-error only means anything if the connection survives the error:
    // the run has to be able to report what the earlier statements did, and the
    // window has to go on working afterwards without a reconnect.
    assert!(src.query("SELECT nosuchcolumn", 8192).await.is_err());
    let mut ok = src
        .query("SELECT 1 AS one", 8192)
        .await
        .expect("the connection should still be good after a failed statement");
    assert_eq!(ok.next_batch().await.unwrap().unwrap().num_rows(), 1);
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn cursor_pages_stably() {
    let src = connect().await;

    // Test cursor pagination with bench_wide table
    let mut cursor = src
        .cursor("SELECT * FROM bench_wide ORDER BY id", 1000)
        .await
        .expect("cursor failed");

    let mut all_rows = Vec::new();
    let mut page_count = 0;

    loop {
        let batch = cursor.fetch().await.expect("fetch failed");
        if let Some(batch) = batch {
            page_count += 1;
            let id_col = col::<Int32Array>(&batch, "id");
            for i in 0..batch.num_rows() {
                all_rows.push(id_col.value(i));
            }
        } else {
            break;
        }
    }

    // Verify we got all rows in order
    assert_eq!(all_rows.len(), 1000000, "Should have fetched all 1M rows");

    // Verify stable ordering
    for i in 1..all_rows.len() {
        assert!(
            all_rows[i - 1] < all_rows[i],
            "Rows should be in ascending order"
        );
    }

    // Verify no duplicates or missing rows
    let expected: Vec<i32> = (1..=1000000i32).collect();
    assert_eq!(
        all_rows, expected,
        "Should have all rows without duplicates or gaps"
    );

    // Verify we got the expected number of pages
    assert_eq!(page_count, 1000, "Should have 1000 pages of 1000 rows each");

    // Test explicit close
    cursor.close().await.expect("close failed");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn cursor_last_page_is_short_and_next_empty() {
    let src = connect().await;

    // Test with a page size that will result in a partial last page
    // bench_wide has 1,000,000 rows. With page size 333333:
    // - Page 1: 333333 rows
    // - Page 2: 333333 rows
    // - Page 3: 333333 rows
    // - Page 4: 1 row (remaining)
    // - Page 5: empty (should return None)
    let mut cursor = src
        .cursor("SELECT id FROM bench_wide ORDER BY id", 333333)
        .await
        .expect("cursor failed");

    // Fetch first page
    let batch1 = cursor
        .fetch()
        .await
        .expect("fetch failed")
        .expect("first page should exist");
    assert_eq!(batch1.num_rows(), 333333);

    // Fetch second page
    let batch2 = cursor
        .fetch()
        .await
        .expect("fetch failed")
        .expect("second page should exist");
    assert_eq!(batch2.num_rows(), 333333);

    // Fetch third page (should be partial)
    let batch3 = cursor
        .fetch()
        .await
        .expect("fetch failed")
        .expect("third page should exist");
    assert_eq!(batch3.num_rows(), 333333); // This is actually correct - 1M total, 3*333333 = 999999, so 1 remaining

    // Fetch fourth page (should be the final 1 row)
    let batch4 = cursor
        .fetch()
        .await
        .expect("fetch failed")
        .expect("fourth page should exist");
    assert_eq!(batch4.num_rows(), 1); // The final 1 row

    // Fetch fifth page (should be empty)
    let batch5 = cursor.fetch().await.expect("fetch failed");
    assert!(batch5.is_none(), "Fifth page should be empty");

    // Test explicit close
    cursor.close().await.expect("close failed");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn cursor_works_without_primary_key() {
    let src = connect().await;

    // Create a temporary table without a primary key for testing
    run(&src, "DROP TABLE IF EXISTS test_cursor_no_pk CASCADE").await;
    run(&src, "CREATE TABLE test_cursor_no_pk (id int, name text)").await;
    run(&src, "INSERT INTO test_cursor_no_pk VALUES (1, 'first'), (2, 'second'), (3, 'third'), (4, 'fourth'), (5, 'fifth')").await;

    // Test cursor on a relation without a primary key
    // This should work because cursor doesn't require a primary key
    let mut cursor = src
        .cursor("SELECT id, name FROM test_cursor_no_pk ORDER BY id", 2)
        .await
        .expect("cursor failed");

    let mut all_rows = Vec::new();
    let mut page_count = 0;

    loop {
        let batch = cursor.fetch().await.expect("fetch failed");
        if let Some(batch) = batch {
            page_count += 1;
            let id_col = col::<Int32Array>(&batch, "id");
            for i in 0..batch.num_rows() {
                all_rows.push(id_col.value(i));
            }
        } else {
            break;
        }
    }

    // Verify we got all rows in order
    assert_eq!(all_rows.len(), 5, "Should have fetched all 5 rows");

    // Verify stable ordering
    for i in 1..all_rows.len() {
        assert!(
            all_rows[i - 1] < all_rows[i],
            "Rows should be in ascending order"
        );
    }

    // Verify no duplicates or missing rows
    let expected: Vec<i32> = vec![1, 2, 3, 4, 5];
    assert_eq!(
        all_rows, expected,
        "Should have all rows without duplicates or gaps"
    );

    // Verify we got the expected number of pages
    assert_eq!(
        page_count, 3,
        "Should have 3 pages of 2 rows each (plus 1 partial)"
    );

    // Test explicit close
    cursor.close().await.expect("close failed");

    // Clean up
    run(&src, "DROP TABLE test_cursor_no_pk CASCADE").await;
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn schemas_exclude_catalog_namespaces() {
    let src = connect().await;
    let schemas = src.schemas().await.expect("schemas failed");
    let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();

    assert!(names.contains(&"public"), "public schema should be listed");
    // Catalog schemas would bury the user's own objects in the navigator.
    assert!(
        !names.iter().any(|n| n.starts_with("pg_")),
        "pg_* schemas should be filtered out, got {names:?}"
    );
    assert!(!names.contains(&"information_schema"));
    assert!(
        names.contains(&"reporting"),
        "a non-public user schema should be listed too, got {names:?}"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn relations_are_scoped_to_the_schema_asked_for() {
    let src = connect().await;
    // Nothing hardcodes "public", but with one schema in the database that is
    // untested. A schema argument quietly ignored would show every table under
    // every branch of the navigator.
    let rels = src.relations("reporting").await.expect("relations failed");
    let names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["daily_totals"]);

    let cols = src
        .columns("reporting", "daily_totals")
        .await
        .expect("columns failed");
    assert_eq!(cols.len(), 3);
    assert!(cols[0].is_primary_key, "day is the key");

    assert!(
        src.columns("public", "daily_totals")
            .await
            .expect("columns failed")
            .is_empty(),
        "a relation must not be findable under a schema it is not in"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn relations_report_kind_and_row_estimate() {
    let src = connect().await;
    let rels = src.relations("public").await.expect("relations failed");

    let bench = rels
        .iter()
        .find(|r| r.name == "bench_wide")
        .expect("bench_wide should be listed");
    assert_eq!(bench.kind, RelationKind::Table);
    assert_eq!(bench.schema, "public");
    // The planner estimate is approximate by design, but should be the right
    // order of magnitude after the seed's ANALYZE.
    assert!(
        bench.estimated_rows > 900_000 && bench.estimated_rows < 1_100_000,
        "estimate {} is not near 1M",
        bench.estimated_rows
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn views_are_reported_as_views_and_carry_their_definition() {
    let src = connect().await;
    let rels = src.relations("public").await.expect("relations failed");
    let kind = |name: &str| {
        rels.iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .kind
    };

    // The two kinds are distinct in the navigator, not both "some sort of view":
    // one is a stored query and the other is a stored result, and only one of
    // them can go stale.
    assert_eq!(kind("bench_open_lines"), RelationKind::View);
    assert_eq!(
        kind("bench_category_totals"),
        RelationKind::MaterializedView
    );

    for name in ["bench_open_lines", "bench_category_totals"] {
        let sql = src
            .definition("public", name)
            .await
            .expect("definition failed")
            .unwrap_or_else(|| panic!("{name} should have a definition"));
        // The join is the reason the view exists, so it is the thing whose
        // absence would mean the definition came back truncated or generic.
        assert!(
            sql.contains("JOIN bench_wide w ON w.id = c.parent_id"),
            "{name} definition should carry its join: {sql}"
        );
        assert!(
            sql.lines().count() > 1,
            "pg_get_viewdef was asked for its pretty form, which is multi-line"
        );
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_relation_without_a_definition_reports_none_rather_than_empty() {
    let src = connect().await;
    // pg_get_viewdef answers an empty string for a table rather than refusing,
    // so without the relkind filter every table would claim a definition it
    // does not have — and the Structure tab would offer an empty section for it.
    assert_eq!(
        src.definition("public", "bench_wide")
            .await
            .expect("definition failed"),
        None,
        "a table has no definition, not a blank one"
    );
    assert_eq!(
        src.definition("public", "no_such_relation")
            .await
            .expect("a missing relation should be empty, not an error"),
        None
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn only_a_materialized_view_can_report_an_index() {
    let src = connect().await;
    // The one structural difference between the two kinds that the Structure
    // tab can show: a materialized view stores its rows, so it can be indexed.
    let idx = src
        .indexes("public", "bench_category_totals")
        .await
        .expect("indexes failed");
    assert_eq!(idx.len(), 1);
    assert_eq!(idx[0].columns, vec!["category"]);
    assert!(idx[0].is_unique);
    assert!(
        !idx[0].is_primary,
        "a materialized view has no primary key to be"
    );

    assert!(
        src.indexes("public", "bench_open_lines")
            .await
            .expect("indexes failed")
            .is_empty(),
        "a plain view stores nothing, so there is nothing to index"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_views_columns_are_described_like_a_tables() {
    let src = connect().await;
    // The Structure tab's upper table is the same code for both, but a view's
    // columns come from pg_attribute entries the query's output types produced
    // rather than from a CREATE TABLE, so it is worth watching that they arrive.
    let cols = src
        .columns("public", "bench_open_lines")
        .await
        .expect("columns failed");
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "order_id",
            "line_no",
            "sku",
            "qty",
            "category",
            "ordered_on"
        ]
    );
    // Renamed in the view's select list; reporting the base column's name would
    // describe a column the view does not expose.
    assert_eq!(cols[5].name, "ordered_on");
    assert_eq!(cols[5].data_type, "date");
    assert!(
        !cols.iter().any(|c| c.is_primary_key),
        "a view has no primary key, which is why the browse cannot page one"
    );

    let agg = src
        .columns("public", "bench_category_totals")
        .await
        .expect("columns failed");
    assert_eq!(agg.len(), 4);
    // count() is bigint however the counted column was typed.
    assert_eq!(agg[1].name, "lines");
    assert_eq!(agg[1].data_type, "bigint");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn concurrent_metadata_calls() {
    let src = connect().await;
    let src = Arc::new(src);

    // Run two metadata calls concurrently on the same PgSource
    let a = Arc::clone(&src);
    let handle1 = tokio::spawn(async move { a.schemas().await });

    let b = Arc::clone(&src);
    let handle2 = tokio::spawn(async move { b.relations("public").await });

    // Wait for both to complete
    let result1 = handle1.await.unwrap().expect("first metadata call failed");
    let result2 = handle2.await.unwrap().expect("second metadata call failed");

    // Both should succeed
    assert!(!result1.is_empty());
    assert!(!result2.is_empty());
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn metadata_call_while_streaming() {
    let src = Arc::new(connect().await);

    // The stream needs a handle of its own: it outlives this scope, and the metadata
    // call below is the point of the test, so the original has to stay usable.
    let streaming = Arc::clone(&src);
    let stream_handle = tokio::spawn(async move {
        streaming
            .query("SELECT * FROM bench_wide ORDER BY id", 1000)
            .await
    });

    // Give the stream a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // While the stream is running, make a metadata call
    let metadata_result = src.schemas().await;

    // The metadata call should succeed even though a stream is running
    assert!(!metadata_result.unwrap().is_empty());

    // Wait for the stream to complete
    let mut stream = stream_handle.await.unwrap().unwrap();
    while stream.next_batch().await.unwrap().is_some() {}
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn bad_connection_still_fails_immediately() {
    // Test that connecting with a bad password still fails at connect time
    let result =
        PgSource::connect("host=127.0.0.1 port=55432 user=baduser password=badpass dbname=bench")
            .await;

    // Should fail immediately with a connection error
    let Err(err) = result else {
        panic!("connecting with a bad password should fail");
    };
    // Should be a postgres error, not a pool exhausted error
    assert!(!matches!(err, PgError::PoolExhausted));
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn columns_describe_types_keys_and_nullability() {
    let src = connect().await;
    let cols = src
        .columns("public", "bench_wide")
        .await
        .expect("columns failed");

    assert_eq!(cols.len(), 20);
    assert_eq!(cols[0].position, 1, "positions start at 1 and are ordered");

    let id = &cols[0];
    assert_eq!(id.name, "id");
    assert!(id.is_primary_key, "id is the declared primary key");
    assert!(!id.nullable, "a primary key column cannot be null");

    let num = cols.iter().find(|c| c.name == "num_val").unwrap();
    assert_eq!(
        num.data_type, "numeric(18,4)",
        "type should carry its modifiers, not just the base name"
    );

    let nullable = cols.iter().find(|c| c.name == "nullable_text").unwrap();
    assert!(nullable.nullable);

    assert!(
        cols.iter().filter(|c| c.is_primary_key).count() == 1,
        "exactly one column is in the primary key"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn indexes_keep_key_order_and_describe_what_they_cover() {
    let src = connect().await;
    let idx = src
        .indexes("public", "bench_child")
        .await
        .expect("indexes failed");

    let pk = &idx[0];
    assert!(pk.is_primary, "the primary key sorts first");
    assert_eq!(
        pk.columns,
        vec!["order_id", "line_no"],
        "composite keys are ordered by index position, not by attnum"
    );

    let expression = idx.iter().find(|i| i.name.contains("email_lower")).unwrap();
    assert_eq!(
        expression.columns,
        vec!["lower(email)"],
        "an expression key must not be reported as a plain column"
    );

    let partial = idx.iter().find(|i| i.name.contains("pending")).unwrap();
    // Parenthesised because pg_get_expr renders it that way, which is also how
    // psql's \d prints it. Passed through rather than unwrapped: the moment
    // this starts editing the server's own rendering it can get it wrong.
    assert_eq!(
        partial.predicate.as_deref(),
        Some("(shipped_at IS NULL)"),
        "a partial index reported without its predicate claims coverage it lacks"
    );

    let unique = idx.iter().find(|i| i.name.contains("sku")).unwrap();
    assert!(unique.is_unique && !unique.is_primary);
    assert_eq!(unique.method, "btree");
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn foreign_keys_carry_their_target_and_action() {
    let src = connect().await;
    let keys = src
        .foreign_keys("public", "bench_child")
        .await
        .expect("foreign keys failed");

    assert_eq!(keys.len(), 1);
    let fk = &keys[0];
    assert_eq!(fk.local_columns, vec!["parent_id"]);
    assert_eq!(fk.other_schema, "public");
    assert_eq!(fk.other_table, "bench_wide");
    assert_eq!(fk.other_columns, vec!["id"]);
    assert_eq!(fk.on_delete, "CASCADE");
    assert_eq!(
        fk.on_update, "NO ACTION",
        "an undeclared action is NO ACTION, not an empty string"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn the_two_directions_of_a_key_do_not_bleed_into_each_other() {
    let src = connect().await;
    // bench_wide is the target of bench_child's key, not the holder of one.
    // Reporting it as an outbound key would put a constraint on the wrong
    // table's Structure tab; failing to report it inbound would hide the
    // dependency that makes deleting a row cascade.
    let outbound = src
        .foreign_keys("public", "bench_wide")
        .await
        .expect("foreign keys failed");
    assert!(outbound.is_empty());

    let inbound = src
        .referenced_by("public", "bench_wide")
        .await
        .expect("referenced by failed");
    assert_eq!(inbound.len(), 1);
    let r = &inbound[0];
    // Named for the vantage point: from bench_wide, "local" is its own id and
    // "other" is the referencing side. Swapping these would draw the arrow
    // backwards.
    assert_eq!(r.local_columns, vec!["id"]);
    assert_eq!(r.other_table, "bench_child");
    assert_eq!(r.other_columns, vec!["parent_id"]);
    assert_eq!(r.on_delete, "CASCADE");

    assert!(
        src.referenced_by("public", "bench_child")
            .await
            .expect("referenced by failed")
            .is_empty(),
        "nothing references bench_child"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn constraints_report_check_and_unique_but_not_keys() {
    let src = connect().await;
    let all = src
        .constraints("public", "bench_child")
        .await
        .expect("constraints failed");

    // The primary and foreign keys have sections of their own. Repeating them
    // here would make one table's rules look like two sets of rules.
    assert_eq!(all.len(), 2, "only the CHECK and the UNIQUE: {all:?}");

    let check = all
        .iter()
        .find(|c| c.name.contains("qty_positive"))
        .unwrap();
    assert_eq!(check.kind, ConstraintKind::Check);
    // Single-parenthesised: pg_get_constraintdef is asked for its pretty form,
    // which drops the redundant pair that pg_get_expr leaves in an index
    // predicate. Passed through as the server renders it either way.
    assert_eq!(check.definition, "CHECK (qty > 0)");

    let unique = all.iter().find(|c| c.name.contains("order_line")).unwrap();
    assert_eq!(unique.kind, ConstraintKind::Unique);
    assert_eq!(
        unique.definition, "UNIQUE (order_id, line_no, sku)",
        "the column list is what makes a unique constraint readable"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn triggers_decode_their_bitmask_and_admit_when_disabled() {
    let src = connect().await;
    let all = src
        .triggers("public", "bench_child")
        .await
        .expect("triggers failed");

    // bench_child's foreign key installs constraint triggers of its own. They
    // are the server enforcing a key that is already listed, not behaviour
    // anyone wrote, so they must not appear.
    assert_eq!(all.len(), 2, "internal constraint triggers are excluded");

    let before = all.iter().find(|t| t.name.contains("before")).unwrap();
    assert_eq!(before.timing, "BEFORE");
    assert_eq!(
        before.events,
        vec!["INSERT", "UPDATE"],
        "a multi-event trigger fires on every event in its mask"
    );
    assert_eq!(before.level, "ROW");
    assert_eq!(before.function, "bench_child_touch");
    assert!(before.enabled);

    let after = all.iter().find(|t| t.name.contains("after")).unwrap();
    assert_eq!(after.timing, "AFTER");
    assert_eq!(after.events, vec!["DELETE"]);
    assert_eq!(after.level, "STATEMENT");
    assert!(
        !after.enabled,
        "a disabled trigger shown as active promises behaviour that will not happen"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn unknown_relation_yields_no_columns() {
    let src = connect().await;
    let cols = src
        .columns("public", "no_such_table")
        .await
        .expect("missing relation should be empty, not an error");
    assert!(cols.is_empty());
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn invalid_sql_surfaces_the_database_error() {
    let src = connect().await;
    let err = match src
        .query("SELECT * FROM table_that_does_not_exist", 8192)
        .await
    {
        Ok(_) => panic!("expected a database error"),
        Err(e) => e,
    };
    assert!(
        matches!(err, PgError::Postgres(_)),
        "expected a postgres error, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_located_error_says_where_it_is() {
    let src = connect().await;
    // The message says what is wrong and never where. The position is the other
    // half of the answer, and it is the half a front end can act on: with it the
    // caret goes to the character, without it the user re-reads a hundred lines
    // of SQL looking for the one the server already found.
    let sql = "SELECT id FROM bench_wide WHERE nosuchcolumn = 1";
    let err = match src.query(sql, 8192).await {
        Ok(_) => panic!("expected a database error"),
        Err(e) => e,
    };
    let position = err
        .statement_position()
        .expect("a column that does not exist has a position");
    // Counted in characters from 1, so it indexes the statement as sent.
    assert_eq!(position, 33);
    let at = position as usize - 1;
    assert_eq!(
        &sql[at..at + "nosuchcolumn".len()],
        "nosuchcolumn",
        "the position should land on the offending token, not near it"
    );

    // An error with nothing to point at must say so rather than guess. A caller
    // that gets a plausible number for every failure moves the caret to a
    // character that had nothing to do with it.
    let unsupported = src
        .query("SELECT point(1,2) AS p", 8192)
        .await
        .err()
        .expect("point is unsupported");
    assert_eq!(unsupported.statement_position(), None);
}

/// The error a statement fails with, from whichever call surfaces it.
///
/// Which one that is depends on the server's output buffer, not on the kind of
/// failure. `query` waits for a BindComplete the server has no reason to flush
/// on its own, so a statement that fails before anything forces a flush reports
/// from there, and one that fails afterwards reports from the first batch. Both
/// are the same failure, and a test that pins itself to one of them is testing
/// the buffer.
async fn error_from(src: &PgSource, sql: &str) -> PgError {
    match src.query(sql, 8192).await {
        Err(e) => e,
        Ok(mut stream) => stream
            .next_batch()
            .await
            .expect_err("expected the statement to fail"),
    }
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn cancelling_names_the_pooled_connections_too() {
    // Metadata reads moved off the session onto pooled connections and cancel went
    // on naming only the session, so Stop during a navigator load was a button that
    // did nothing. A pooled connection that is in use is not in the pool to be
    // found, which is why the source keeps a token for every connection it opened.
    //
    // What this proves is the mechanism: that the request reaches every backend
    // without deadlocking on the registry, that naming an idle one is not an error,
    // and that the session and the pool both still work afterwards. What it does
    // not prove is the interruption — that needs a catalog read slow enough to
    // catch in flight, and this database answers every one of them in a
    // millisecond. The interruption itself is covered on the session by the test
    // below, and on a cursor by the ffi harness.
    let src = connect().await;

    // Four at once cannot share one connection, so the pool ends up holding
    // several — which is the state the old cancel could not see.
    let (a, b, c, d) = tokio::join!(src.schemas(), src.schemas(), src.schemas(), src.schemas());
    for schemas in [a, b, c, d] {
        assert!(!schemas.expect("metadata read failed").is_empty());
    }

    src.cancel().await.expect("cancel request failed");

    assert!(
        !src.schemas()
            .await
            .expect("pool unusable after cancel")
            .is_empty(),
        "cancelling an idle backend is a no-op, not damage"
    );
    let mut stream = src
        .query("SELECT 1", 8192)
        .await
        .expect("session unusable after cancel");
    assert!(stream.next_batch().await.expect("batch error").is_some());
}

#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_running_statement_stops_when_asked_and_says_that_is_why() {
    use std::sync::Arc;
    use std::time::Duration;

    let src = Arc::new(connect().await);

    // Scheduled before the statement is sent, not after `query` returns. `query`
    // does not come back while the statement is running: the server buffers its
    // output and flushes when the command ends, so the BindComplete it waits for
    // arrives with the result rather than ahead of it. Cancelling from after
    // that call is cancelling something that has already finished — which is how
    // the first version of this test sat through the whole sleep and passed
    // nothing.
    let canceller = Arc::clone(&src);
    let cancel = tokio::spawn(async move {
        // Long enough for the statement to be running. Cancelling before the
        // server starts it would find nothing to stop, which is the one outcome
        // that looks identical to a broken cancel.
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await
    });

    // pg_sleep rather than a large scan: a scan the server finishes early makes
    // this pass without cancelling anything, and the test would then be green on
    // a build where cancellation does not work at all.
    //
    // In the WHERE clause rather than the select list because pg_sleep returns
    // void, which has no Arrow type and so fails while the schema is being
    // built — before anything has run to be cancelled.
    //
    // Bounded so a cancel that never arrives fails the run instead of hanging it
    // for the full sleep.
    let sql = "SELECT 1 AS n WHERE pg_sleep(30) IS NULL";
    let err = tokio::time::timeout(Duration::from_secs(10), error_from(&src, sql))
        .await
        .expect("the statement was still running 10s after being cancelled");

    assert!(err.is_cancelled(), "expected a cancellation, got: {err}");
    cancel
        .await
        .expect("cancel task panicked")
        .expect("cancel request failed");

    // Every other failure has to stay distinguishable from this one, or the
    // front end labels real faults as something the user did on purpose.
    let ordinary = error_from(&src, "SELECT 1/0").await;
    assert!(!ordinary.is_cancelled(), "got: {ordinary}");
}
