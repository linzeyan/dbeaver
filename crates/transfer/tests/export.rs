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

#[test]
fn an_apostrophe_in_a_value_does_not_end_the_string_it_is_in() {
    // The failure this prevents is not a syntax error. A value with an
    // apostrophe, written into a script unescaped, closes its own literal and
    // the rest of the row becomes SQL — which is how a generated script stops
    // being data and starts being a statement somebody did not write.
    let mut out = Vec::new();
    let rows = dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(batch(
            vec![Some(1)],
            vec![Some("O'Brien')); DROP TABLE t; --")],
        ))],
        &dbsql::POSTGRES,
        "public.people".to_string(),
        &mut out,
    )
    .expect("export failed");
    assert_eq!(rows, 1);

    let sql = String::from_utf8(out).expect("not utf-8");
    assert_eq!(
        sql,
        "INSERT INTO public.people (id, note) VALUES\n\
         (1, 'O''Brien'')); DROP TABLE t; --');\n"
    );
}

#[test]
fn a_backslash_is_doubled_only_where_the_dialect_reads_it_as_an_escape() {
    // MySQL treats a backslash as an escape and PostgreSQL does not, so one
    // rule for both is wrong on one of them — and wrong in the direction that
    // silently changes the value rather than failing.
    let path = vec![Some(r"C:\tmp\x")];
    let mut mysql = Vec::new();
    dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(batch(vec![Some(1)], path.clone()))],
        &dbsql::MYSQL,
        "t".to_string(),
        &mut mysql,
    )
    .expect("mysql export failed");
    assert!(
        String::from_utf8(mysql).unwrap().contains(r"'C:\\tmp\\x'"),
        "MySQL reads a lone backslash as an escape, so it has to be doubled"
    );

    let mut postgres = Vec::new();
    dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(batch(vec![Some(1)], path))],
        &dbsql::POSTGRES,
        "t".to_string(),
        &mut postgres,
    )
    .expect("postgres export failed");
    assert!(
        String::from_utf8(postgres).unwrap().contains(r"'C:\tmp\x'"),
        "doubling on PostgreSQL would put two backslashes in the value"
    );
}

#[test]
fn a_null_is_the_keyword_and_an_empty_string_is_a_literal() {
    // Written as `''` a NULL becomes the empty string, which is a different
    // value and one that passes every NOT NULL constraint the real one fails.
    let mut out = Vec::new();
    dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(batch(
            vec![Some(1), None],
            vec![None, Some("")],
        ))],
        &dbsql::POSTGRES,
        "t".to_string(),
        &mut out,
    )
    .expect("export failed");
    let sql = String::from_utf8(out).unwrap();
    assert!(sql.contains("(1, NULL)"), "{sql}");
    assert!(sql.contains("(NULL, '')"), "{sql}");
}

#[test]
fn a_column_whose_name_needs_quoting_gets_it() {
    // An unquoted `order` is a keyword on every one of these databases, and a
    // script naming it bare fails on the first statement.
    let schema = Arc::new(Schema::new(vec![
        Field::new("order", DataType::Int32, true),
        Field::new("Mixed Case", DataType::Utf8, true),
    ]));
    let id: ArrayRef = Arc::new(Int32Array::from(vec![Some(1)]));
    let note: ArrayRef = Arc::new(StringArray::from(vec![Some("a")]));
    let b = RecordBatch::try_new(schema, vec![id, note]).expect("batch");

    let mut out = Vec::new();
    dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(b)],
        &dbsql::POSTGRES,
        "t".to_string(),
        &mut out,
    )
    .expect("export failed");
    let sql = String::from_utf8(out).unwrap();
    assert!(
        sql.starts_with(r#"INSERT INTO t ("order", "Mixed Case") VALUES"#),
        "{sql}"
    );
}

#[test]
fn a_result_longer_than_one_statement_is_split_into_valid_ones() {
    // One statement for a million rows exceeds what most databases will parse,
    // so the writer breaks them up — and every piece has to be terminated, or
    // the second INSERT is read as a continuation of the first.
    let ids: Vec<Option<i32>> = (0..450).map(Some).collect();
    let notes: Vec<Option<&str>> = (0..450).map(|_| Some("x")).collect();
    let mut out = Vec::new();
    dbtransfer::export_sql(
        vec![Ok::<_, ArrowError>(batch(ids, notes))],
        &dbsql::POSTGRES,
        "t".to_string(),
        &mut out,
    )
    .expect("export failed");
    let sql = String::from_utf8(out).unwrap();

    assert_eq!(sql.matches("INSERT INTO").count(), 3, "200 + 200 + 50");
    assert_eq!(sql.matches(";\n").count(), 3, "each one terminated");
    assert!(sql.ends_with(";\n"), "including the last");
    // The rows themselves must all be there, split or not.
    assert_eq!(sql.matches("'x'").count(), 450);
}
