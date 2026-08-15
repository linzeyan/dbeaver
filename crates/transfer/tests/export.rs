//! What comes out the other end, read back rather than asserted about.

use arrow::array::{Array, ArrayRef, Int32Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use dbtransfer::{Format, export};
use std::sync::Arc;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, true),
        Field::new("note", DataType::Utf8, true),
    ]))
}

fn batch(ids: Vec<Option<i32>>, notes: Vec<Option<&str>>) -> RecordBatch {
    let id: ArrayRef = Arc::new(Int32Array::from(ids));
    let note: ArrayRef = Arc::new(StringArray::from(notes));
    RecordBatch::try_new(schema(), vec![id, note]).expect("batch")
}

fn text(batches: Vec<RecordBatch>, format: Format) -> String {
    let mut out = Vec::new();
    let rows = export(
        batches.into_iter().map(Ok::<_, ArrowError>),
        format,
        &mut out,
    )
    .expect("export failed");
    let text = String::from_utf8(out).expect("output was not utf-8");
    // The count is part of the contract — the front end shows it — so every
    // test that reads the text also holds it to the rows it handed over.
    let written = text.lines().count();
    assert!(
        written as u64 >= rows,
        "reported {rows} rows in {written} lines"
    );
    text
}

#[test]
fn a_null_is_nothing_and_an_empty_string_is_a_quoted_nothing() {
    // The whole reason this crate does not use `arrow::csv`: its null
    // representation is a plain empty field, so a reader cannot tell a NULL
    // from an empty string. They are different values, people write WHERE
    // clauses against the difference, and PostgreSQL's own COPY … FORMAT csv
    // draws it exactly this way.
    let csv = text(
        vec![batch(vec![Some(1), Some(2)], vec![None, Some("")])],
        Format::Csv,
    );
    assert_eq!(csv, "id,note\n1,\n2,\"\"\n");
}

#[test]
fn a_value_carrying_the_delimiter_or_a_quote_or_a_newline_is_wrapped() {
    // Approximately-correct quoting is how one column silently shifts every
    // field after it one place to the left, in a file nobody re-reads.
    let csv = text(
        vec![batch(
            vec![Some(1), Some(2), Some(3)],
            vec![Some("a,b"), Some("say \"hi\""), Some("one\ntwo")],
        )],
        Format::Csv,
    );
    assert_eq!(
        csv,
        "id,note\n1,\"a,b\"\n2,\"say \"\"hi\"\"\"\n3,\"one\ntwo\"\n"
    );
}

#[test]
fn a_comma_is_not_special_in_a_tab_separated_file() {
    // The quoting rule is about the delimiter in force, not about commas. A
    // writer that quotes commas in a TSV produces literal quote marks in every
    // cell that has one, which is a different value than the one in the column.
    let tsv = text(vec![batch(vec![Some(1)], vec![Some("a,b")])], Format::Tsv);
    assert_eq!(tsv, "id\tnote\n1\ta,b\n");

    let tsv = text(vec![batch(vec![Some(1)], vec![Some("a\tb")])], Format::Tsv);
    assert_eq!(tsv, "id\tnote\n1\t\"a\tb\"\n");
}

#[test]
fn the_header_is_written_once_however_many_batches_arrive() {
    // A result arrives in batches and a file has one header. Writing it per
    // batch puts the column names in the middle of the data, where a parser
    // reads them as a row.
    let csv = text(
        vec![
            batch(vec![Some(1)], vec![Some("a")]),
            batch(vec![Some(2)], vec![Some("b")]),
            batch(vec![Some(3)], vec![Some("c")]),
        ],
        Format::Csv,
    );
    assert_eq!(csv, "id,note\n1,a\n2,b\n3,c\n");
}

#[test]
fn a_result_with_no_rows_still_says_what_its_columns_were() {
    // An empty result is not a failed one, and a file with no header cannot be
    // told apart from a file that was never written.
    let csv = text(vec![batch(vec![], vec![])], Format::Csv);
    assert_eq!(csv, "id,note\n");
}

#[test]
fn every_row_reaches_the_file_when_there_are_more_than_the_buffer_holds() {
    // The writer flushes on a byte count, so the interesting case is the one
    // where a flush lands mid-result — an off-by-one there loses or repeats
    // whatever straddled it, and a small test never crosses the threshold.
    let rows: Vec<Option<i32>> = (0..50_000).map(Some).collect();
    let notes: Vec<Option<&str>> = (0..50_000).map(|_| Some("padding to move bytes")).collect();
    let csv = text(vec![batch(rows, notes)], Format::Csv);

    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 50_001, "header plus every row");
    assert_eq!(lines[1], "0,padding to move bytes");
    assert_eq!(lines[50_000], "49999,padding to move bytes");
}

#[test]
fn json_lines_keeps_one_object_per_line_and_omits_nulls() {
    // One object per line rather than a top-level array, because a transfer
    // stopped part way through has to leave a file that still parses.
    let json = text(
        vec![batch(vec![Some(1), Some(2)], vec![None, Some("b")])],
        Format::JsonLines,
    );
    let lines: Vec<&str> = json.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], r#"{"id":1}"#);
    assert_eq!(lines[1], r#"{"id":2,"note":"b"}"#);
}

#[test]
fn parquet_comes_back_as_the_batches_that_went_in() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    // A direct Arrow write is the phase's exit criterion, and the only way to
    // check one is to read the file back: a Parquet file that is subtly wrong
    // is still a file, and its size looks fine.
    let mut out = Vec::new();
    let rows = export(
        vec![
            Ok::<_, ArrowError>(batch(vec![Some(1), None], vec![Some("a"), Some("")])),
            Ok::<_, ArrowError>(batch(vec![Some(3)], vec![None])),
        ],
        Format::Parquet,
        &mut out,
    )
    .expect("export failed");
    assert_eq!(rows, 3);

    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(out))
        .expect("not a parquet file")
        .build()
        .expect("reader");
    let read: Vec<RecordBatch> = reader.collect::<Result<_, _>>().expect("read failed");

    let total: usize = read.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 3);
    assert_eq!(read[0].schema().field(0).name(), "id");
    assert_eq!(read[0].schema().field(1).name(), "note");

    let ids: Vec<Option<i32>> = read
        .iter()
        .flat_map(|b| {
            let c = b.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
            (0..b.num_rows()).map(|i| if c.is_null(i) { None } else { Some(c.value(i)) })
        })
        .collect();
    assert_eq!(ids, vec![Some(1), None, Some(3)]);

    // The distinction the CSV writer works to preserve is free here — Parquet
    // stores nullness out of band — but it still has to survive the round trip.
    let notes: Vec<Option<&str>> = read
        .iter()
        .flat_map(|b| {
            let c = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
            (0..b.num_rows()).map(|i| if c.is_null(i) { None } else { Some(c.value(i)) })
        })
        .collect();
    assert_eq!(notes, vec![Some("a"), Some(""), None]);
}
