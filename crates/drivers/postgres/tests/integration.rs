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
use driver_postgres::{PgError, PgSource};

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
