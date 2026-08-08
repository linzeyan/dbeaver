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
        ("num_val", DataType::Decimal128(38, 10)),
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
        assert_eq!(a.value(0), expected, "decimal column {name}");
    }
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
