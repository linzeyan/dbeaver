//! A file's rows arriving in a table.
//!
//! One in-memory DuckDB is enough here: what is under test is the reading and
//! the round trip through the table, and DuckDB is a real database with real
//! column types. Each test writes its own file, because the suite runs in
//! parallel and a shared name is a shared file.

use arrow::array::{Array, Int32Array, StringArray};
use dbtransfer::{Format, Import, file_columns, import};
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
async fn a_csv_round_trip_keeps_the_empty_string() {
    // This test used to pin the opposite, on purpose: with `arrow-csv` doing
    // the reading, `` and `""` reached Arrow as the same empty string and both
    // came back NULL, because the `csv` crate resolves quoting while it
    // parses. The distinction the writer spends effort preserving was being
    // thrown away on the way back in, silently. `DelimitedReader` is the fix —
    // reading by hand the way the file is written by hand — and this test is
    // now the proof that the round trip keeps its promise.
    let db = database().await;
    three_cases(&db).await;

    let (nulls, empties) = round_trip(&db, Format::Csv, "csv").await;
    assert_eq!(nulls, 1, "the NULL came back a NULL");
    assert_eq!(empties, 1, "the empty string came back an empty string");
}

#[tokio::test]
async fn a_tsv_round_trip_keeps_it_too() {
    // TSV shares the writer, the reader and therefore the promise; only the
    // delimiter differs. Pinned separately because it used to share the CSV
    // defect too, through the same shared path.
    let db = database().await;
    three_cases(&db).await;

    let (nulls, empties) = round_trip(&db, Format::Tsv, "tsv").await;
    assert_eq!(nulls, 1, "the NULL came back a NULL");
    assert_eq!(empties, 1, "the empty string came back an empty string");
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

/// A mapping drives which of the file's columns go where, and what is skipped.
///
/// The file here is deliberately wrong for the table in all three ways at once:
/// its columns are in the other order, it carries one the table has no room for,
/// and it names them differently. Positionally it would put a note in `id` and
/// fail the parse; by name it would refuse the extra column. With a mapping it
/// is an ordinary import.
#[tokio::test]
async fn a_mapping_says_which_column_goes_where() {
    let db = std::sync::Arc::new(database().await);
    table(&db).await;
    let path = file_of("mapped", "remark,ignored,number\nhello,x,1\nworld,y,2\n");

    let mut reading = Import::open(
        &path,
        Format::Csv,
        db.clone(),
        &dbsql::DUCKDB,
        "t".to_string(),
        Some(vec![Some("note".to_string()), None, Some("id".to_string())]),
    )
    .await
    .expect("the import should open");
    let rows = drain(&mut reading).await;

    assert_eq!(rows, 2);
    assert_eq!(
        rows_of(&db).await,
        vec![(1, Some("hello".into())), (2, Some("world".into()))],
        "each value landed in the column the mapping named"
    );
}

/// A skipped column is still parsed past, not parsed.
///
/// The one that would be missed by a mapping applied at the reader: `junk` holds
/// text that no typed column would accept, and it is skipped, so nothing should
/// ever try. A reader that narrowed before parsing would also pass this; one
/// that parsed everything and narrowed after would fail it, which is the bug
/// this pins.
#[tokio::test]
async fn a_skipped_column_is_not_parsed() {
    let db = std::sync::Arc::new(database().await);
    table(&db).await;
    let path = file_of("skipped", "id,junk\n1,not a number at all\n");

    let mut reading = Import::open(
        &path,
        Format::Csv,
        db.clone(),
        &dbsql::DUCKDB,
        "t".to_string(),
        Some(vec![Some("id".to_string()), None]),
    )
    .await
    .expect("the import should open");
    let rows = drain(&mut reading).await;

    assert_eq!(rows, 1);
    assert_eq!(rows_of(&db).await, vec![(1, None)]);
}

/// A mapping naming a column the table does not have is refused before a row
/// moves, and the message says which name.
#[tokio::test]
async fn a_mapping_onto_a_column_that_is_not_there_is_refused() {
    let db = std::sync::Arc::new(database().await);
    table(&db).await;
    let path = file_of("nocolumn", "a,b\n1,2\n");

    let failure = Import::open(
        &path,
        Format::Csv,
        db.clone(),
        &dbsql::DUCKDB,
        "t".to_string(),
        Some(vec![Some("id".to_string()), Some("nowhere".to_string())]),
    )
    .await
    .err()
    .expect("a column that is not there is not importable into");
    assert!(
        failure.to_string().contains("nowhere"),
        "the refusal should name the column: {failure}"
    );
}

/// A mapping that skips everything is refused rather than run.
///
/// It would otherwise be a valid import of nothing: an INSERT with no columns,
/// which most servers refuse in their own words at the first batch — after the
/// window has said it is importing.
#[tokio::test]
async fn a_mapping_that_keeps_nothing_is_refused() {
    let db = std::sync::Arc::new(database().await);
    table(&db).await;
    let path = file_of("nothing", "a,b\n1,2\n");

    let failure = Import::open(
        &path,
        Format::Csv,
        db.clone(),
        &dbsql::DUCKDB,
        "t".to_string(),
        Some(vec![None, None]),
    )
    .await
    .err()
    .expect("an import of no columns is not an import");
    assert!(
        failure.to_string().contains("skipped"),
        "the refusal should say why: {failure}"
    );
}

/// The names a mapping is chosen against come from the file itself.
#[tokio::test]
async fn a_files_columns_are_read_from_its_header() {
    let path = file_of("columns", "id,note,extra\n1,hello,x\n");
    assert_eq!(
        file_columns(&path, Format::Csv).expect("the header should be readable"),
        vec!["id", "note", "extra"]
    );

    // An empty file has no columns and is not an error: what that is, is a file
    // with nothing in it, and saying so is the panel's job rather than an error
    // banner's.
    let empty = file_of("columns-empty", "");
    assert!(
        file_columns(&empty, Format::Csv)
            .expect("an empty file is readable")
            .is_empty()
    );
}

/// Steps an import to its end and answers the total.
async fn drain(reading: &mut Import) -> u64 {
    loop {
        match reading.step().await.expect("the import should not fail") {
            dbtransfer::Step::Moved(_) => {}
            dbtransfer::Step::Done(n) | dbtransfer::Step::Stopped(n) => return n,
        }
    }
}
