//! What a file's columns would have to be, if a table had to be made for them.
//!
//! The module `import` refuses to have. Reading a file into a table that exists
//! needs no inference — the table already knows what its columns are, and
//! guessing would only be a way of getting that answer wrong. Making the table
//! is the one job where there is nothing else to ask, and it is a different job:
//! what comes out of here is a `CREATE TABLE` somebody reads before it runs,
//! not a parse somebody finds out about afterwards.
//!
//! Deliberately conservative, and only for the formats that have no types of
//! their own. A delimited file is text and nothing else, so four answers are
//! offered — whole number, number, timestamp, text — and anything a column
//! disagrees about lands on text. Widening later is a `ALTER TABLE` somebody
//! runs on purpose; narrowing later is data that no longer fits.
//!
//! JSON Lines and Parquet are not guessed at. Both carry types, so what they
//! say is what is used: a Parquet `bool` column becomes a boolean column rather
//! than the word "true" in a text one.

use crate::{Format, delimited};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use dbconn::{DbError, DbResult};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

/// How many records of a delimited file are read before deciding.
///
/// Enough that a column of integers with one stray word in it is usually caught,
/// and few enough that the answer arrives while somebody is still looking at the
/// open panel. It is a sample and is described as one: the sheet shows the
/// statement, and the statement is the thing to correct.
const SAMPLE_ROWS: usize = 1000;

/// The columns a table would need to hold this file.
pub fn infer_schema(path: &Path, format: Format) -> DbResult<SchemaRef> {
    let file = File::open(path).map_err(|e| DbError::new(e.to_string()))?;
    let schema = match format {
        Format::Csv => delimited_schema(file, b',')?,
        Format::Tsv => delimited_schema(file, b'\t')?,
        Format::JsonLines => {
            arrow::json::reader::infer_json_schema(std::io::BufReader::new(file), Some(SAMPLE_ROWS))
                .map(|(schema, _)| schema)
                .map_err(|e| DbError::new(e.to_string()))?
        }
        Format::Parquet => {
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| DbError::new(e.to_string()))?
                .schema()
                .as_ref()
                .clone()
        }
    };
    if schema.fields().is_empty() {
        return Err(DbError::new(
            "this file has no columns to make a table from",
        ));
    }
    Ok(Arc::new(schema))
}

fn delimited_schema(file: File, delimiter: u8) -> DbResult<Schema> {
    let (names, rows) =
        delimited::head(file, delimiter, SAMPLE_ROWS).map_err(|e| DbError::new(e.to_string()))?;
    let fields = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let values = rows.iter().filter_map(|row| row.get(index)).flatten();
            // Every column nullable, whatever the sample shows. A thousand rows
            // with nothing missing is not a promise about the ten thousand after
            // them, and a NOT NULL inferred from a sample is a table that starts
            // refusing rows part way through the import that created it.
            Field::new(name, kind_of(values), true)
        })
        .collect::<Vec<_>>();
    Ok(Schema::new(fields))
}

/// The narrowest of the four that every value in `values` fits.
///
/// A blank is not a value: an empty field is this reader's NULL, and a column of
/// numbers with a gap in it is still a column of numbers. A column that is
/// nothing but blanks has said nothing, and text is what says nothing back.
fn kind_of<'a>(values: impl Iterator<Item = &'a String>) -> DataType {
    let (mut integer, mut double, mut timestamp, mut seen) = (true, true, true, false);
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        seen = true;
        integer = integer && value.parse::<i64>().is_ok();
        double = double && value.parse::<f64>().is_ok();
        timestamp = timestamp && looks_like_timestamp(value);
        if !integer && !double && !timestamp {
            break;
        }
    }
    match (seen, integer, double, timestamp) {
        (false, ..) => DataType::Utf8,
        (_, true, ..) => DataType::Int64,
        (_, _, true, _) => DataType::Float64,
        // Microseconds and no zone, which is the one shape every database here
        // has a word for. A column carrying offsets is a column this gets wrong
        // in the direction the sheet can fix.
        (.., true) => DataType::Timestamp(TimeUnit::Microsecond, None),
        _ => DataType::Utf8,
    }
}

/// Whether `value` has the shape of a date, or a date and a time.
///
/// A shape check and not a parse. What this decides is which word goes in a
/// `CREATE TABLE`; the values themselves are parsed later by Arrow's own cast,
/// against the column this helped choose, and a file that gets past this and
/// fails there fails loudly with the line in the message. Being stricter here
/// would mean carrying a date library to answer a question that is asked once
/// per column.
///
/// `2026-08-24` and `2026-08-24 09:08:19` and `2026-08-24T09:08:19.5Z` all pass;
/// `2026` and `08/24/2026` do not — the second deliberately, since a client that
/// guessed which half was the month would guess wrong for half the world.
fn looks_like_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() < 10 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10)) {
        return false;
    }
    if b.len() == 10 {
        return true;
    }
    // A date and something after it: one separator, then at least `HH:MM`.
    b.len() >= 16 && (b[10] == b' ' || b[10] == b'T') && digits(11..13) && b[13] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(values: &[&str]) -> DataType {
        let owned: Vec<String> = values.iter().map(|v| (*v).to_string()).collect();
        kind_of(owned.iter())
    }

    #[test]
    fn a_column_of_whole_numbers_is_a_whole_number_column() {
        assert_eq!(kind(&["1", "2", "-3"]), DataType::Int64);
    }

    /// One value that is not a number is the whole column's answer. The
    /// alternative is a table that takes most of the file.
    #[test]
    fn one_word_makes_the_column_text() {
        assert_eq!(kind(&["1", "2", "n/a"]), DataType::Utf8);
    }

    #[test]
    fn a_gap_is_not_a_value() {
        assert_eq!(kind(&["1", "", "3"]), DataType::Int64);
        assert_eq!(kind(&["", "", ""]), DataType::Utf8);
    }

    #[test]
    fn a_number_with_a_point_in_it_widens_rather_than_falling_to_text() {
        assert_eq!(kind(&["1", "2.5"]), DataType::Float64);
    }

    #[test]
    fn dates_and_datetimes_are_timestamps_and_ambiguous_ones_are_not() {
        assert_eq!(
            kind(&["2026-08-24", "2026-08-24 09:08:19"]),
            DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        assert_eq!(kind(&["08/24/2026"]), DataType::Utf8);
        assert_eq!(kind(&["2026"]), DataType::Int64);
    }
}
