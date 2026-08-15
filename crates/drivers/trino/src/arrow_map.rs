//! Trino's types to Arrow's, decided from the `typeSignature` the result
//! carries.
//!
//! **The rule**, which is the same one the Cassandra driver states: a scalar
//! gets the narrowest Arrow type that holds every value it can take exactly, out
//! of the twelve the reader at the other end of the FFI has a case for —
//! `apps/macos/Sources/DbClient/ArrowTable.swift` maps `b`, `s`, `i`, `l`, `f`,
//! `g`, `u`, `z`, `tdD`, `ttu`, `tsu:` and `d:`. Anything else becomes text, and
//! the text is Trino's own rendering, which is the form a person would recognise
//! because it is the form the CLI prints.
//!
//! Everything here is decided from the wire encoding rather than from the
//! documentation, because the JSON encoding of a Trino value is not the type:
//!
//! - **A `double` is not always a number.** `nan()` and `infinity()` arrive as
//!   the JSON *strings* `"NaN"` and `"Infinity"`, because JSON has no spelling
//!   for either. A driver that read `as_f64()` and gave up would turn a
//!   perfectly ordinary aggregate over an empty group into a null.
//! - **A `decimal` is always a string.** Which is the only way it could be:
//!   `decimal(38, 3)` holds more digits than a double, and JSON's number is a
//!   double as far as most parsers are concerned. So the digits arrive exact and
//!   `Decimal128` holds them exact — Trino's maximum precision is 38, which is
//!   `Decimal128`'s maximum precision, and the two happen to line up perfectly.
//! - **A `bigint` *is* a number**, and `9223372036854775807` and
//!   `-9223372036854775808` both survive the round trip. `serde_json` parses an
//!   integer into an `i64` before it considers a float, which is what makes that
//!   true; a parser that read every number as a double would lose the low bits of
//!   every id in the table.
//! - **A `varbinary` is base64.**
//! - **A `char(n)` arrives padded.** `CAST('abc' AS char(5))` is `"abc  "`, and
//!   the padding is part of the value rather than something to trim: it is what
//!   the comparison semantics of `char` are built on.
//!
//! **Three types are text where a narrower one exists, and each is a decision.**
//!
//! - `timestamp(p)` past microseconds. The reader's only timestamp case is
//!   `tsu:` — microseconds — and Trino's `timestamp(9)` is nanoseconds. Dropping
//!   three digits quietly is worse than handing over all nine, which is the same
//!   trade the ClickHouse driver makes for `DateTime64(9)`.
//! - `timestamp(p) with time zone`, at any precision. Arrow carries one zone for
//!   a whole column and Trino carries one per *value*: `2024-01-15 12:34:56.123
//!   Asia/Taipei` and `2024-01-15 12:34:56.123 UTC` are two rows of one column.
//!   Converting them to a common instant would throw away the zone the row was
//!   stored with, and claiming a column-wide zone would state something that is
//!   not in the data.
//! - `time(p) with time zone`. Arrow has no zoned time at all.
//!
//! **Every composite is one JSON string cell.** `array`, `map` and `row` arrive
//! as JSON already, and Arrow's `List`, `Map` and `Struct` are types the reader
//! has no case for — a column of them draws as `<+l>`, `<+m>` and `<+s>` in every
//! cell. So the JSON goes across as the JSON it is. `json`, `uuid`, `ipaddress`
//! and the two `interval` types need no arm at all for the same reason: they are
//! already the string Trino wrote.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Float32Builder,
    Float64Builder, Int16Builder, Int32Builder, Int64Builder, RecordBatch, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow::compute::kernels::cast_utils::{Parser, parse_decimal};
use arrow::datatypes::{
    DataType, Date32Type, Decimal128Type, Field, Schema, SchemaRef, Time64MicrosecondType,
    TimeUnit, TimestampMicrosecondType,
};
use arrow::error::ArrowError;
use base64::Engine;
use serde_json::Value;
use std::sync::Arc;

use crate::wire::Column;

/// The digits of precision past which a datetime no longer fits the reader's
/// microsecond types.
const MICROSECOND_DIGITS: i64 = 6;

/// Which Arrow builder a column's values go into.
///
/// A small closed set rather than `DataType` itself, because the decision this
/// makes is "which builder", and the fourteen Trino types that land in `Text`
/// should not each be a case further down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cell {
    Bool,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal(u8, i8),
    Binary,
    Date,
    Time,
    Timestamp,
    Text,
}

impl Cell {
    fn arrow(self) -> DataType {
        match self {
            Cell::Bool => DataType::Boolean,
            Cell::Int16 => DataType::Int16,
            Cell::Int32 => DataType::Int32,
            Cell::Int64 => DataType::Int64,
            Cell::Float32 => DataType::Float32,
            Cell::Float64 => DataType::Float64,
            Cell::Decimal(precision, scale) => DataType::Decimal128(precision, scale),
            Cell::Binary => DataType::Binary,
            Cell::Date => DataType::Date32,
            Cell::Time => DataType::Time64(TimeUnit::Microsecond),
            // No zone, and stated by its absence rather than as "UTC". A Trino
            // `timestamp(p)` is a wall clock with no zone attached — the type
            // that has one is a different type, and it is text here.
            Cell::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Cell::Text => DataType::Utf8,
        }
    }
}

/// The builder one Trino type's values belong in.
///
/// Matched on `rawType` and never on the display name, because the display name
/// of a `map(varchar(1), array(row(a integer)))` needs a parser and `rawType`
/// is that same type already taken apart by the server.
pub(crate) fn cell_of(column: &Column) -> Cell {
    let signature = &column.signature;
    match signature.raw_type.as_str() {
        "boolean" => Cell::Bool,
        // Arrow has an Int8 and the reader on the far side of the FFI does not,
        // so a `tinyint` is widened rather than left as a column of `<c>`.
        "tinyint" | "smallint" => Cell::Int16,
        "integer" => Cell::Int32,
        "bigint" => Cell::Int64,
        "real" => Cell::Float32,
        "double" => Cell::Float64,
        "decimal" => match (signature.number(0), signature.number(1)) {
            (Some(precision), Some(scale))
                if (1..=38).contains(&precision) && (0..=precision).contains(&scale) =>
            {
                Cell::Decimal(precision as u8, scale as i8)
            }
            // A decimal whose own signature does not describe a decimal. Not
            // reachable against a Trino that is working, and the alternative to
            // this arm is an `unwrap` that turns a server change into a panic.
            _ => Cell::Text,
        },
        "varbinary" => Cell::Binary,
        "date" => Cell::Date,
        // The precision arrives in the signature only because the request asked
        // for `PARAMETRIC_DATETIME`; without that header the server reports
        // every one of these as precision 3 and truncates the values to match.
        // See `wire`.
        "time" if precision(signature) <= MICROSECOND_DIGITS => Cell::Time,
        "timestamp" if precision(signature) <= MICROSECOND_DIGITS => Cell::Timestamp,
        _ => Cell::Text,
    }
}

/// A datetime type's declared digits, defaulting to the value Trino defaults to.
///
/// A bare `timestamp` in Trino means `timestamp(3)`, and the signature of one
/// carries no argument at all. Defaulting to 0 would work by accident and read
/// as a mistake; defaulting to something above the microsecond line would turn
/// every unparameterised column into text.
fn precision(signature: &crate::wire::TypeSignature) -> i64 {
    signature.number(0).unwrap_or(3)
}

/// A result's columns and how to read their values.
pub(crate) struct Plan {
    schema: SchemaRef,
    cells: Vec<Cell>,
}

impl Plan {
    pub fn of(columns: &[Column]) -> Plan {
        let cells: Vec<Cell> = columns.iter().map(cell_of).collect();
        let fields: Vec<Field> = columns
            .iter()
            .zip(&cells)
            // Nullable throughout, and not from asking the catalog: this is a
            // result and not a table, and an outer join over a column declared
            // `NOT NULL` produces nulls in it.
            .map(|(column, cell)| Field::new(&column.name, cell.arrow(), true))
            .collect();
        Plan {
            schema: Arc::new(Schema::new(fields)),
            cells,
        }
    }

    /// The plan for a statement that has no result set.
    pub fn empty() -> Plan {
        Plan {
            schema: Arc::new(Schema::empty()),
            cells: Vec::new(),
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// One page of Trino's row-major JSON, as a column-major batch.
    pub fn batch(&self, rows: &[Vec<Value>]) -> Result<RecordBatch, ArrowError> {
        let columns: Result<Vec<ArrayRef>, ArrowError> = self
            .cells
            .iter()
            .enumerate()
            .map(|(at, cell)| build(*cell, self.schema.field(at).name(), rows, at))
            .collect();
        RecordBatch::try_new(Arc::clone(&self.schema), columns?)
    }
}

/// One column, read out of every row at the same offset.
///
/// A row shorter than the schema is refused rather than padded. It cannot happen
/// against a coordinator that is working — the protocol sends a value per column
/// per row — and padding it would put nulls in a grid and call them data.
fn build(cell: Cell, name: &str, rows: &[Vec<Value>], at: usize) -> Result<ArrayRef, ArrowError> {
    let values = || {
        rows.iter().map(move |row| {
            row.get(at).ok_or_else(|| {
                ArrowError::InvalidArgumentError(format!(
                    "a row arrived with {} values where the result has more, reading {name}",
                    row.len()
                ))
            })
        })
    };

    Ok(match cell {
        Cell::Bool => {
            let mut builder = BooleanBuilder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        value
                            .as_bool()
                            .ok_or_else(|| wrong(name, "boolean", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int16 => {
            let mut builder = Int16Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        integer(value, name)?
                            .try_into()
                            .map_err(|_| wrong(name, "a 16-bit integer", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int32 => {
            let mut builder = Int32Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        integer(value, name)?
                            .try_into()
                            .map_err(|_| wrong(name, "a 32-bit integer", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int64 => {
            let mut builder = Int64Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(integer(value, name)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Float32 => {
            let mut builder = Float32Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(float(value, name)? as f32),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Float64 => {
            let mut builder = Float64Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(float(value, name)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Decimal(precision, scale) => {
            let mut builder = Decimal128Builder::with_capacity(rows.len())
                .with_precision_and_scale(precision, scale)?;
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => {
                        let digits = value
                            .as_str()
                            .ok_or_else(|| wrong(name, "a decimal", value))?;
                        builder.append_value(parse_decimal::<Decimal128Type>(
                            digits, precision, scale,
                        )?);
                    }
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Binary => {
            let mut builder = BinaryBuilder::with_capacity(rows.len(), rows.len() * 16);
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => {
                        let encoded = value
                            .as_str()
                            .ok_or_else(|| wrong(name, "base64 text", value))?;
                        builder.append_value(
                            base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .map_err(|e| {
                                    ArrowError::ParseError(format!(
                                        "{name} is varbinary and did not decode as base64: {e}"
                                    ))
                                })?,
                        );
                    }
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Date => {
            let mut builder = Date32Builder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        value
                            .as_str()
                            .and_then(Date32Type::parse)
                            .ok_or_else(|| wrong(name, "a date", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Time => {
            let mut builder = Time64MicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        value
                            .as_str()
                            .and_then(Time64MicrosecondType::parse)
                            .ok_or_else(|| wrong(name, "a time", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Timestamp => {
            let mut builder = TimestampMicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(
                        value
                            .as_str()
                            .and_then(TimestampMicrosecondType::parse)
                            .ok_or_else(|| wrong(name, "a timestamp", value))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Text => {
            let mut builder = StringBuilder::with_capacity(rows.len(), rows.len() * 16);
            for value in values() {
                match null_or(value?) {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(text(value)),
                }
            }
            Arc::new(builder.finish())
        }
    })
}

/// A JSON null as an Arrow null, and anything else as itself.
fn null_or(value: &Value) -> Option<&Value> {
    (!value.is_null()).then_some(value)
}

/// A value that arrived as something its column's type does not produce.
///
/// Loud rather than nulled. Against a coordinator that is working this cannot
/// happen; if it does, the type mapping is wrong about a type, and a column of
/// silent nulls is the one outcome that would hide it.
fn wrong(name: &str, expected: &str, value: &Value) -> ArrowError {
    ArrowError::ParseError(format!(
        "{name} should have arrived as {expected}, got {value}"
    ))
}

fn integer(value: &Value, name: &str) -> Result<i64, ArrowError> {
    value
        .as_i64()
        .ok_or_else(|| wrong(name, "an integer", value))
}

/// A float, including the two values JSON cannot spell.
fn float(value: &Value, name: &str) -> Result<f64, ArrowError> {
    if let Some(number) = value.as_f64() {
        return Ok(number);
    }
    match value.as_str() {
        Some("NaN") => Ok(f64::NAN),
        Some("Infinity") => Ok(f64::INFINITY),
        Some("-Infinity") => Ok(f64::NEG_INFINITY),
        _ => Err(wrong(name, "a number", value)),
    }
}

/// A value as the text a person would recognise.
///
/// A JSON string is taken as it is and everything else is rendered as JSON. That
/// one rule covers both halves of `Cell::Text`: `uuid`, `json` and `interval`
/// arrive as strings and are already what the user would type, while `array`,
/// `map` and `row` arrive as JSON structures and should read as JSON. Quoting
/// the first group would show `"8e14e760-…"` in a grid cell, quotes included.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Column, TypeArgument, TypeSignature};

    fn column(name: &str, raw: &str, arguments: Vec<i64>) -> Column {
        Column {
            name: name.to_string(),
            signature: TypeSignature {
                raw_type: raw.to_string(),
                arguments: arguments
                    .into_iter()
                    .map(|n| TypeArgument {
                        kind: "LONG".to_string(),
                        value: serde_json::json!(n),
                    })
                    .collect(),
            },
        }
    }

    /// The line that decides whether a value keeps every digit it was stored
    /// with. Trino's own default precision is 3 and its maximum is 12.
    #[test]
    fn a_datetime_is_text_exactly_when_it_is_finer_than_the_reader_can_hold() {
        assert_eq!(cell_of(&column("t", "timestamp", vec![])), Cell::Timestamp);
        assert_eq!(cell_of(&column("t", "timestamp", vec![0])), Cell::Timestamp);
        assert_eq!(cell_of(&column("t", "timestamp", vec![6])), Cell::Timestamp);
        assert_eq!(cell_of(&column("t", "timestamp", vec![7])), Cell::Text);
        assert_eq!(cell_of(&column("t", "timestamp", vec![9])), Cell::Text);
        assert_eq!(cell_of(&column("t", "time", vec![6])), Cell::Time);
        assert_eq!(cell_of(&column("t", "time", vec![9])), Cell::Text);
        // The zoned pair, at every precision: Arrow carries one zone per column
        // and Trino one per value.
        assert_eq!(
            cell_of(&column("t", "timestamp with time zone", vec![3])),
            Cell::Text
        );
        assert_eq!(
            cell_of(&column("t", "time with time zone", vec![3])),
            Cell::Text
        );
    }

    /// A decimal keeps its own precision and scale, and one that could not be a
    /// decimal falls back to its digits rather than panicking.
    #[test]
    fn a_decimal_carries_the_precision_it_was_declared_with() {
        assert_eq!(
            cell_of(&column("d", "decimal", vec![18, 2])),
            Cell::Decimal(18, 2)
        );
        assert_eq!(
            cell_of(&column("d", "decimal", vec![38, 3])),
            Cell::Decimal(38, 3)
        );
        assert_eq!(cell_of(&column("d", "decimal", vec![39, 0])), Cell::Text);
        assert_eq!(cell_of(&column("d", "decimal", vec![])), Cell::Text);
    }

    /// Everything this driver has no arm for is the text Trino wrote, which is
    /// the whole reason there are so few arms.
    #[test]
    fn a_type_with_no_arrow_home_arrives_as_the_text_trino_wrote() {
        for raw in [
            "uuid",
            "ipaddress",
            "json",
            "interval year to month",
            "interval day to second",
            "array",
            "map",
            "row",
            "varchar",
            "char",
            "a type this driver has never met",
        ] {
            assert_eq!(cell_of(&column("c", raw, vec![])), Cell::Text, "{raw}");
        }
    }

    /// The two values a `double` can hold that JSON has no spelling for. A
    /// driver that read `as_f64()` alone would refuse an ordinary aggregate.
    #[test]
    fn a_double_that_is_not_a_number_still_arrives() {
        assert!(float(&serde_json::json!("NaN"), "d").unwrap().is_nan());
        assert_eq!(
            float(&serde_json::json!("Infinity"), "d").unwrap(),
            f64::INFINITY
        );
        assert_eq!(
            float(&serde_json::json!("-Infinity"), "d").unwrap(),
            f64::NEG_INFINITY
        );
        assert_eq!(float(&serde_json::json!(2.5), "d").unwrap(), 2.5);
        assert!(float(&serde_json::json!("wibble"), "d").is_err());
    }

    /// The widest `bigint` there is, which is what separates a driver that
    /// parses JSON numbers as integers from one that parses them as doubles.
    #[test]
    fn the_ends_of_the_bigint_range_survive_the_json() {
        let widest: Vec<Vec<Value>> = vec![
            vec![serde_json::json!(i64::MAX)],
            vec![serde_json::json!(i64::MIN)],
        ];
        let plan = Plan::of(&[column("id", "bigint", vec![])]);
        let batch = plan.batch(&widest).expect("a batch");
        let ids =
            arrow::array::cast::as_primitive_array::<arrow::datatypes::Int64Type>(batch.column(0));
        assert_eq!(ids.value(0), i64::MAX);
        assert_eq!(ids.value(1), i64::MIN);
    }

    /// A quoted string in a grid cell is the driver showing its own JSON rather
    /// than the user's value.
    #[test]
    fn a_string_is_not_quoted_and_a_structure_is_rendered() {
        assert_eq!(text(&serde_json::json!("8e14e760-7fa8")), "8e14e760-7fa8");
        assert_eq!(text(&serde_json::json!([1, 2, 3])), "[1,2,3]");
        assert_eq!(text(&serde_json::json!({"n": 1})), r#"{"n":1}"#);
    }

    /// Trino writes a space between the date and the time, and the parser has to
    /// take it — this is every timestamp in every result.
    #[test]
    fn trinos_own_datetime_spelling_parses() {
        let rows: Vec<Vec<Value>> = vec![
            vec![
                serde_json::json!("2024-01-15 12:34:56.123456"),
                serde_json::json!("12:34:56.123456"),
                serde_json::json!("2024-01-15"),
                serde_json::json!("AP8="),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ];
        let plan = Plan::of(&[
            column("ts", "timestamp", vec![6]),
            column("tm", "time", vec![6]),
            column("d", "date", vec![]),
            column("raw", "varbinary", vec![]),
        ]);
        let batch = plan.batch(&rows).expect("a batch");
        assert_eq!(batch.num_rows(), 2);
        for at in 0..4 {
            assert!(batch.column(at).is_null(1), "column {at} row 2 is a null");
            assert!(!batch.column(at).is_null(0), "column {at} row 1 is not");
        }
        let raw = arrow::array::cast::as_generic_binary_array::<i32>(batch.column(3));
        assert_eq!(raw.value(0), &[0x00, 0xff]);
    }
}
