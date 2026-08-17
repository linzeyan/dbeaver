//! The delimited reader, read straight off bytes.
//!
//! No database here: these pin how text becomes fields — where a record ends,
//! which blanks are NULL, and which malformed inputs are refused by line.
//! Whether the rows then arrive in a table is `import.rs`'s job.

use arrow::array::{Array, Int32Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbtransfer::DelimitedReader;
use std::sync::Arc;

fn strings(names: &[&str]) -> SchemaRef {
    Arc::new(Schema::new(
        names
            .iter()
            .map(|n| Field::new(*n, DataType::Utf8, true))
            .collect::<Vec<_>>(),
    ))
}

/// Reads `body` as CSV against `schema`, expecting it to parse.
fn read(body: &str, schema: SchemaRef) -> Vec<RecordBatch> {
    DelimitedReader::new(body.as_bytes(), b',', schema)
        .expect("the reader must build")
        .collect::<Result<Vec<_>, _>>()
        .expect("the file must parse")
}

/// Reads `body` expecting a refusal, and hands back its message.
fn read_error(body: &str, schema: SchemaRef) -> String {
    DelimitedReader::new(body.as_bytes(), b',', schema)
        .expect("the reader must build")
        .collect::<Result<Vec<_>, _>>()
        .expect_err("this input must be refused")
        .to_string()
}

/// The one column `note` as `(is_null, text)` rows across all batches.
fn notes(batches: &[RecordBatch], column: usize) -> Vec<Option<String>> {
    let mut out = Vec::new();
    for batch in batches {
        let column = batch
            .column(column)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..column.len() {
            out.push((!column.is_null(row)).then(|| column.value(row).to_string()));
        }
    }
    out
}

#[test]
fn an_unquoted_blank_is_null_and_a_quoted_one_is_an_empty_string() {
    // The distinction this reader exists for, read straight off the bytes.
    let batches = read("id,note\n1,\n2,\"\"\n3,x\n", strings(&["id", "note"]));
    assert_eq!(
        notes(&batches, 1),
        vec![None, Some(String::new()), Some("x".into())]
    );
}

#[test]
fn a_quoted_field_carries_the_delimiter_a_quote_and_a_newline() {
    let batches = read(
        "note\n\"a,b\"\n\"she said \"\"hi\"\"\"\n\"two\nlines\"\n",
        strings(&["note"]),
    );
    assert_eq!(
        notes(&batches, 0),
        vec![
            Some("a,b".into()),
            Some("she said \"hi\"".into()),
            Some("two\nlines".into())
        ]
    );
}

#[test]
fn crlf_and_a_lone_cr_both_end_a_record() {
    let batches = read("note\r\na\r\nb\rc\n", strings(&["note"]));
    assert_eq!(
        notes(&batches, 0),
        vec![Some("a".into()), Some("b".into()), Some("c".into())]
    );
}

#[test]
fn the_last_record_may_end_without_a_newline() {
    let batches = read("id,note\n1,x", strings(&["id", "note"]));
    assert_eq!(notes(&batches, 1), vec![Some("x".into())]);
}

#[test]
fn a_trailing_unquoted_blank_before_eof_is_a_null() {
    // "1," and then nothing: the record ends mid-field, and that field is a
    // NULL, not a shorter record.
    let batches = read("id,note\n1,", strings(&["id", "note"]));
    assert_eq!(notes(&batches, 1), vec![None]);
}

#[test]
fn a_single_column_blank_line_is_a_null_row() {
    // The writer spells a single-column NULL row as a bare newline. A reader
    // that skipped blank lines — as the `csv` crate does — would drop the row
    // and report one fewer than was exported.
    let batches = read("note\n\n\"\"\nx\n", strings(&["note"]));
    assert_eq!(
        notes(&batches, 0),
        vec![None, Some(String::new()), Some("x".into())]
    );
}

#[test]
fn a_record_with_the_wrong_width_names_its_line() {
    let message = read_error("id,note\n1,x\n2,y,z\n", strings(&["id", "note"]));
    assert!(
        message.contains("line 3") && message.contains("expected 2"),
        "the refusal should say which line and what was expected, got: {message}"
    );
}

#[test]
fn a_header_of_the_wrong_width_is_refused_before_any_row() {
    // A file in the wrong format — the wrong delimiter, or not a CSV at all —
    // usually announces itself in the header, so the refusal lands there.
    let message = read_error("id;note\n1;x\n", strings(&["id", "note"]));
    assert!(
        message.contains("line 1"),
        "the header's line should be named, got: {message}"
    );
}

#[test]
fn an_unclosed_quote_is_refused_with_the_line_it_opened_on() {
    let message = read_error("note\nfine\n\"never closed\n", strings(&["note"]));
    assert!(
        message.contains("line 3") && message.contains("quote"),
        "the refusal should name the quote and its line, got: {message}"
    );
}

#[test]
fn a_value_that_does_not_parse_names_its_column() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, true),
        Field::new("note", DataType::Utf8, true),
    ]));
    let message = read_error("id,note\nnot a number,x\n", schema);
    assert!(
        message.contains("id"),
        "the refusal should name the column that would not parse, got: {message}"
    );
}

#[test]
fn a_typed_column_parses_and_a_blank_in_it_is_null_either_way() {
    // In a non-string column the quoting distinction has nowhere to land: an
    // empty field can only be NULL, quoted or not.
    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, true)]));
    let batches = read("id\n7\n\n\"\"\n", schema);
    let all: Vec<Option<i32>> = batches
        .iter()
        .flat_map(|b| {
            let ids = b.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
            (0..ids.len())
                .map(|r| (!ids.is_null(r)).then(|| ids.value(r)))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(all, vec![Some(7), None, None]);
}

#[test]
fn an_empty_file_and_a_header_only_file_both_yield_no_rows() {
    assert!(read("", strings(&["note"])).is_empty());
    assert!(read("note\n", strings(&["note"])).is_empty());
}

#[test]
fn tabs_delimit_a_tsv_and_the_quoting_rules_are_the_same() {
    let schema = strings(&["id", "note"]);
    let batches = DelimitedReader::new("id\tnote\n1\t\"a\tb\"\n2\t\n".as_bytes(), b'\t', schema)
        .expect("the reader must build")
        .collect::<Result<Vec<_>, _>>()
        .expect("the file must parse");
    assert_eq!(notes(&batches, 1), vec![Some("a\tb".into()), None]);
}

#[test]
fn a_binary_column_is_refused_before_a_byte_is_read() {
    // The writer renders binary as hex text; reading that hex back as raw
    // bytes would be corruption with a row count that looks right.
    let schema = Arc::new(Schema::new(vec![Field::new(
        "blob",
        DataType::Binary,
        true,
    )]));
    let error = DelimitedReader::new("blob\n".as_bytes(), b',', schema)
        .err()
        .expect("a binary column must be refused");
    assert!(
        error.to_string().contains("blob"),
        "the refusal should name the column, got: {error}"
    );
}

#[test]
fn a_file_longer_than_one_batch_arrives_whole() {
    // 10_000 rows crosses the internal batch size, so this pins that records
    // are not lost or duplicated at the seam between batches.
    let mut body = String::from("id\n");
    for i in 0..10_000 {
        body.push_str(&format!("{i}\n"));
    }
    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, true)]));
    let batches = read(&body, schema);
    assert!(batches.len() > 1, "one batch would prove nothing");
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 10_000);
    let first = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert_eq!(first.value(0), 0);
    let last = batches.last().unwrap();
    let ids = last
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert_eq!(ids.value(ids.len() - 1), 9_999);
}
