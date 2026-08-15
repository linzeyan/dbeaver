//! A file's rows arriving in a table.
//!
//! One in-memory DuckDB is enough here: what is under test is the reading and
//! the round trip through the table, and DuckDB is a real database with real
//! column types. Each test writes its own file, because the suite runs in
//! parallel and a shared name is a shared file.

use arrow::array::{Array, Int32Array, StringArray};
use dbtransfer::{Format, import};
use driver_duckdb::DuckSource;
use std::path::PathBuf;

async fn database() -> DuckSource {
    DuckSource::connect(":memory:").await.unwrap()
}

/// Runs a statement to completion, which is what executes it.
async fn run(conn: &DuckSource, statement: &str) {
    let mut stream = conn.query(statement, 1).await.unwrap();
    while stream.next_batch().await.unwrap().is_some() {}
}

/// Every `(id, note)` row in `t`, in id order.
async fn rows_of(conn: &DuckSource) -> Vec<(i32, Option<String>)> {
    let mut stream = conn
        .query("SELECT id, note FROM t ORDER BY id", 1)
        .await
        .unwrap();
    let mut rows = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let notes = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            let note = (!notes.is_null(row)).then(|| notes.value(row).to_string());
            rows.push((ids.value(row), note));
        }
    }
    rows
}

async fn count(conn: &DuckSource, sql: &str) -> i64 {
    let mut stream = conn.query(sql, 1).await.unwrap();
    let batch = stream
        .next_batch()
        .await
        .unwrap()
        .expect("a count always answers a row");
    batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap()
        .value(0)
}

/// Writes `body` to a file of this test's own, and hands back the path.
fn file_of(name: &str, body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("dbtransfer-import-{name}.csv"));
    std::fs::write(&path, body).expect("the temp file must be writable");
    path
}

/// A table with the two columns every test here reads back.
async fn table(conn: &DuckSource) {
    run(conn, "CREATE TABLE t (id INTEGER, note VARCHAR)").await;
}

#[tokio::test]
async fn a_files_rows_arrive_in_the_table() {
    let db = database().await;
    table(&db).await;
    let path = file_of("plain", "id,note\n1,hello\n2,world\n");

    let rows = import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");

    assert_eq!(rows, 2);
    assert_eq!(
        rows_of(&db).await,
        vec![(1, Some("hello".into())), (2, Some("world".into()))]
    );
}

#[tokio::test]
async fn the_header_names_the_columns_and_is_not_one_of_the_rows() {
    let db = database().await;
    table(&db).await;
    let path = file_of("header", "id,note\n1,hello\n");

    let rows = import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");

    // Both halves matter: a header read as data would be a third row, and a
    // header read as data in a typed column would fail the parse instead —
    // which is why the text is checked for as well as the count.
    assert_eq!(rows, 1);
    assert_eq!(
        count(&db, "SELECT count(*) FROM t WHERE note = 'note'").await,
        0,
        "the word 'note' came from the header, not from a row"
    );
}

#[tokio::test]
async fn an_empty_field_arrives_as_a_null() {
    let db = database().await;
    table(&db).await;
    let path = file_of("null", "id,note\n1,\n2,x\n");

    import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");

    // Asked of the database, because this is the distinction someone writes a
    // WHERE clause against — and the one the exporter goes out of its way to
    // preserve on the way out.
    assert_eq!(
        count(&db, "SELECT count(*) FROM t WHERE note IS NULL").await,
        1,
        "the empty field is a NULL and not an empty string"
    );
}

#[tokio::test]
async fn an_apostrophe_survives_the_trip() {
    let db = database().await;
    table(&db).await;
    let path = file_of("apostrophe", "id,note\n1,O'Brien\n");

    import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");

    // Unescaped this ends its own literal, and whatever follows is read as SQL.
    assert_eq!(rows_of(&db).await, vec![(1, Some("O'Brien".into()))]);
}

#[tokio::test]
async fn a_file_longer_than_one_statement_arrives_whole() {
    let db = database().await;
    table(&db).await;
    let mut body = String::from("id,note\n");
    for i in 0..500 {
        body.push_str(&format!("{i},row{i}\n"));
    }
    let path = file_of("long", &body);

    let rows = import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");

    assert_eq!(rows, 500);
    let arrived = rows_of(&db).await;
    assert_eq!(arrived.len(), 500);
    // The rows either side of the seam between two statements.
    assert_eq!(arrived[199], (199, Some("row199".into())));
    assert_eq!(arrived[200], (200, Some("row200".into())));
}

/// Exports `source` and imports it back into `t`, and answers (nulls, empties).
async fn round_trip(db: &DuckSource, format: Format, extension: &str) -> (i64, i64) {
    let mut stream = db
        .query("SELECT id, note FROM source ORDER BY id", 1024)
        .await
        .unwrap();
    let mut batches = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        batches.push(Ok(batch));
    }

    let path = std::env::temp_dir().join(format!("dbtransfer-round-trip.{extension}"));
    let file = std::fs::File::create(&path).unwrap();
    assert_eq!(dbtransfer::export(batches, format, file).unwrap(), 3);

    let read = import(&path, format, db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect("import failed");
    assert_eq!(read, 3, "every row came back");

    (
        count(db, "SELECT count(*) FROM t WHERE note IS NULL").await,
        count(db, "SELECT count(*) FROM t WHERE note = ''").await,
    )
}

/// A NULL, an empty string, and a value — the three cases a round trip can
/// confuse.
async fn three_cases(db: &DuckSource) {
    run(db, "CREATE TABLE source (id INTEGER, note VARCHAR)").await;
    run(db, "INSERT INTO source VALUES (1, NULL), (2, ''), (3, 'x')").await;
    table(db).await;
}

#[tokio::test]
async fn json_lines_bring_back_what_they_took_away() {
    // The round trip that keeps its promises. JSON has a null of its own, so
    // the distinction the exporter preserves survives being read back.
    let db = database().await;
    three_cases(&db).await;

    let (nulls, empties) = round_trip(&db, Format::JsonLines, "jsonl").await;
    assert_eq!(nulls, 1, "the NULL came back a NULL");
    assert_eq!(empties, 1, "the empty string came back an empty string");
}

#[tokio::test]
async fn parquet_brings_back_what_it_took_away() {
    let db = database().await;
    three_cases(&db).await;

    let (nulls, empties) = round_trip(&db, Format::Parquet, "parquet").await;
    assert_eq!(nulls, 1, "the NULL came back a NULL");
    assert_eq!(empties, 1, "the empty string came back an empty string");
}

#[tokio::test]
async fn a_csv_round_trip_loses_the_empty_string_and_this_is_known() {
    // This one is pinned as it is, not as it should be.
    //
    // The exporter goes out of its way to write a NULL as nothing and an empty
    // string as `""` — that is why this crate has its own CSV writer instead of
    // `arrow-csv`'s. `arrow-csv`'s READER throws the difference away again: the
    // `csv` crate resolves quoting while parsing, so by the time Arrow sees the
    // field, `` and `""` are the same empty string, and it calls both NULL.
    //
    // So a CSV that leaves this application and comes back has had its empty
    // strings turned into NULLs, silently. Fixing it means reading CSV by hand
    // the way it is already written by hand, which is a piece of work and a
    // decision, not a patch. Until then this test is the record: if someone
    // fixes the reader, this fails and points at the paragraph above.
    let db = database().await;
    three_cases(&db).await;

    let (nulls, empties) = round_trip(&db, Format::Csv, "csv").await;
    assert_eq!(nulls, 2, "the empty string was read back as a second NULL");
    assert_eq!(empties, 0, "and so no empty string survived");
}

#[tokio::test]
async fn importing_into_a_table_that_is_not_there_fails() {
    let db = database().await;
    let path = file_of("missing", "id,note\n1,hello\n");

    // The schema probe is the first thing `import` does, so this fails before a
    // row is read rather than part way through writing one.
    let error = import(&path, Format::Csv, &db, &dbsql::DUCKDB, "t".to_string())
        .await
        .expect_err("a missing table must not import");
    assert!(
        error.to_string().to_lowercase().contains("t"),
        "the failure should name what it could not find, got: {error}"
    );
}
