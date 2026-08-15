//! Snowflake's types to Arrow's, decided from the `rowType` the result carries.
//!
//! **The rule** is the one the Cassandra and Trino drivers state: a scalar gets
//! the narrowest Arrow type that holds every value it can take exactly, out of
//! the ones the reader at the other end of the FFI has a case for —
//! `apps/macos/Sources/DbClient/ArrowTable.swift` maps `b`, `s`, `i`, `l`, `f`,
//! `g`, `u`, `z`, `tdD`, `ttu`, `tsu:` with and without a zone, and `d:`.
//! Anything else becomes text.
//!
//! **Every value in a `jsonv2` result is a JSON string**, including the numbers,
//! and that is the fact this whole file is arranged around. A `NUMBER(38,0)`
//! arrives as `"123"`, a `BOOLEAN` as `"true"`, a `DATE` as the number of days
//! since the epoch written out — `"19723"` — and a `TIMESTAMP_NTZ` as seconds
//! since the epoch with a fraction, `"1706000000.123456789"`. So there is no
//! JSON type to read a value out of; there is a string to parse, and the column
//! says how.
//!
//! No account has answered any of this. Four decisions below are the ones most
//! likely to be wrong, and each says so where it is made: the `+1440` on a
//! `TIMESTAMP_TZ` offset, the spelling of a `FLOAT` that is not a number, the
//! case of a `BINARY`'s hex, and whether a value can also arrive as the JSON
//! scalar it stands for rather than as a string. The first three would show up
//! as a column of loud failures rather than as wrong numbers, which is the
//! property worth having when nobody can check.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Float64Builder,
    Int64Builder, RecordBatch, StringBuilder, Time64MicrosecondBuilder,
    TimestampMicrosecondBuilder,
};
use arrow::compute::kernels::cast_utils::parse_decimal;
use arrow::datatypes::{DataType, Decimal128Type, Field, Schema, SchemaRef, TimeUnit};
use arrow::error::ArrowError;
use serde_json::Value;
use std::sync::Arc;

use crate::wire::Column;

/// The digits of precision past which a datetime no longer fits the reader's
/// microsecond types.
const MICROSECOND_DIGITS: i64 = 6;

/// The widest `NUMBER` whose integers all fit an `i64`.
///
/// `NUMBER(19,0)` reaches 10^19, which is past `i64::MAX`; `NUMBER(18,0)` cannot.
/// Above the line the values go into a `Decimal128`, whose maximum precision is
/// 38 and happens to be exactly Snowflake's.
const INT64_DIGITS: i64 = 18;

/// What Snowflake adds to a `TIMESTAMP_TZ` offset before writing it down.
///
/// The second field of a `TIMESTAMP_TZ` value is the zone offset in minutes with
/// 1440 added, so UTC is `1440` and not `0`. It is a bias rather than a sign
/// bit, and it is not in the SQL API reference — it is in the behaviour of
/// Snowflake's own connectors, which is the only place it is written down. A
/// driver that read the field as plain minutes would put every zoned timestamp
/// exactly 24 hours out, which is a wrong answer that looks like a right one, so
/// the constant is named here rather than subtracted inline.
const OFFSET_BIAS: i64 = 1440;

/// Which Arrow builder a column's values go into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cell {
    Bool,
    Int64,
    Float64,
    Decimal(u8, i8),
    Binary,
    Date,
    Time,
    /// Microseconds since the epoch. `zoned` is a `TIMESTAMP_LTZ`, whose value
    /// is an absolute instant and is therefore tagged `UTC` — the "local" in the
    /// name is about how a session renders it, not about what is stored.
    Timestamp {
        zoned: bool,
    },
    /// A datetime rendered as ISO-8601 text, keeping digits Arrow cannot.
    ///
    /// Two columns land here: one finer than microseconds, and any
    /// `TIMESTAMP_TZ`. See `cell_of`.
    Instant {
        digits: u32,
        offset: bool,
    },
    Text,
}

impl Cell {
    fn arrow(self) -> DataType {
        match self {
            Cell::Bool => DataType::Boolean,
            Cell::Int64 => DataType::Int64,
            Cell::Float64 => DataType::Float64,
            Cell::Decimal(precision, scale) => DataType::Decimal128(precision, scale),
            Cell::Binary => DataType::Binary,
            Cell::Date => DataType::Date32,
            Cell::Time => DataType::Time64(TimeUnit::Microsecond),
            Cell::Timestamp { zoned: false } => DataType::Timestamp(TimeUnit::Microsecond, None),
            Cell::Timestamp { zoned: true } => {
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
            }
            Cell::Instant { .. } | Cell::Text => DataType::Utf8,
        }
    }
}

/// The builder one Snowflake type's values belong in.
pub(crate) fn cell_of(column: &Column) -> Cell {
    let scale = column.scale.unwrap_or(0);
    let digits = scale.clamp(0, 9) as u32;
    match column.kind.as_str() {
        "boolean" => Cell::Bool,
        // `NUMBER(p,s)`, which is every integer and every exact decimal
        // Snowflake has: there is no separate `INTEGER` type, only a `NUMBER`
        // with scale zero, which is why this one arm covers both.
        "fixed" => match (column.precision, scale) {
            (Some(precision), 0) if precision <= INT64_DIGITS => Cell::Int64,
            (Some(precision), scale)
                if (1..=38).contains(&precision) && (0..=precision).contains(&scale) =>
            {
                Cell::Decimal(precision as u8, scale as i8)
            }
            // A `NUMBER` whose own metadata does not describe one. Unreachable
            // against an account that is working, and the alternative to this
            // arm is an `unwrap` that turns a server change into a panic.
            _ => Cell::Text,
        },
        "real" => Cell::Float64,
        "binary" => Cell::Binary,
        "date" => Cell::Date,
        "time" if scale <= MICROSECOND_DIGITS => Cell::Time,
        "timestamp_ntz" if scale <= MICROSECOND_DIGITS => Cell::Timestamp { zoned: false },
        "timestamp_ltz" if scale <= MICROSECOND_DIGITS => Cell::Timestamp { zoned: true },
        // Finer than the reader can hold. Handing over all nine digits as text
        // is better than dropping three quietly, which is the trade the
        // ClickHouse driver makes for `DateTime64(9)` and the Trino driver for
        // `timestamp(9)`.
        "time" | "timestamp_ntz" | "timestamp_ltz" => Cell::Instant {
            digits,
            offset: false,
        },
        // `TIMESTAMP_TZ` at every precision, because Arrow carries one zone for a
        // whole column and Snowflake carries one per *value*. Tagging the column
        // `UTC` would be true about the instant and would throw away the offset
        // the row was stored with, which is the same reason the Trino driver
        // gives for the same type. Rendered rather than passed through, because
        // unlike Trino's `2024-01-15 12:34:56.123 Asia/Taipei` the raw value here
        // is `1706000000.123456789 1920`, which is not something to show anybody.
        "timestamp_tz" => Cell::Instant {
            digits,
            offset: true,
        },
        // `variant`, `object` and `array` arrive as JSON text and are already
        // what a person would read; `geography` and `geometry` are GeoJSON or
        // WKT by the session's setting; `vector` is a JSON array. None has an
        // Arrow home the reader has a case for, and all of them are already
        // strings.
        _ => Cell::Text,
    }
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

    /// What the result called its columns.
    ///
    /// For `metadata.rs`, which reads `SHOW` output by column name rather than
    /// by position — Snowflake has added columns to the middle of a `SHOW`
    /// answer before, and a driver counting from the left would then read the
    /// wrong one and report it as data.
    pub fn names(&self) -> Vec<String> {
        self.schema
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect()
    }

    /// One partition of Snowflake's row-major JSON, as a column-major batch.
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
/// A row shorter than the schema is refused rather than padded, as in the Trino
/// driver: padding it would put nulls in a grid and call them data.
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
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(match text.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err(wrong(name, "true or false", &text)),
                    }),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int64 => {
            let mut builder = Int64Builder::with_capacity(rows.len());
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        text.parse::<i64>()
                            .map_err(|_| wrong(name, "an integer", &text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Float64 => {
            let mut builder = Float64Builder::with_capacity(rows.len());
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(float(&text, name)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Decimal(precision, scale) => {
            let mut builder = Decimal128Builder::with_capacity(rows.len())
                .with_precision_and_scale(precision, scale)?;
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder
                        .append_value(parse_decimal::<Decimal128Type>(&text, precision, scale)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Binary => {
            let mut builder = BinaryBuilder::with_capacity(rows.len(), rows.len() * 16);
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder
                        .append_value(hex(&text).ok_or_else(|| wrong(name, "hex digits", &text))?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Date => {
            let mut builder = Date32Builder::with_capacity(rows.len());
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    // Days since the epoch, already, which is what `Date32` is.
                    // No date parsing anywhere in this driver, because Snowflake
                    // never sends a date as one.
                    Some(text) => builder.append_value(
                        text.parse::<i32>()
                            .map_err(|_| wrong(name, "a day number", &text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Time => {
            let mut builder = Time64MicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        scaled(&text, 6)
                            .ok_or_else(|| wrong(name, "seconds past midnight", &text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Timestamp { .. } => {
            let mut builder = TimestampMicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        scaled(&text, 6)
                            .ok_or_else(|| wrong(name, "seconds since the epoch", &text))?,
                    ),
                }
            }
            Arc::new(builder.finish().with_timezone_opt(match cell {
                Cell::Timestamp { zoned: true } => Some("UTC"),
                _ => None,
            }))
        }
        Cell::Instant { digits, offset } => {
            let mut builder = StringBuilder::with_capacity(rows.len(), rows.len() * 32);
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        instant(&text, digits, offset)
                            .ok_or_else(|| wrong(name, "an instant", &text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Text => {
            let mut builder = StringBuilder::with_capacity(rows.len(), rows.len() * 16);
            for value in values() {
                match text_of(value?) {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(text),
                }
            }
            Arc::new(builder.finish())
        }
    })
}

/// A value as the string `jsonv2` states it as, and `None` for a null.
///
/// A JSON string is taken as it is. A JSON number or boolean is rendered, which
/// is not defensive padding: the format documents every value as a string, and
/// if any of them is not, the difference belongs to the encoder rather than to
/// the value — `123` and `"123"` are the same `NUMBER`. Doing it here means the
/// question is answered once instead of in ten builders.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// A value that arrived as something its column's type does not produce.
///
/// Loud rather than nulled, as in the Trino driver: if this fires, the type
/// mapping is wrong about a type, and a column of silent nulls is the one
/// outcome that would hide it — which matters more here than anywhere else in
/// this workspace, because no account has told this file it is right.
fn wrong(name: &str, expected: &str, got: &str) -> ArrowError {
    ArrowError::ParseError(format!(
        "{name} should have arrived as {expected}, got {got}"
    ))
}

/// A float, including the values that are not numbers.
///
/// Snowflake's `FLOAT` holds NaN and both infinities and JSON cannot spell any
/// of them, so they arrive as words. Which words is the guess: `NaN`, `inf` and
/// `-inf` are what Snowflake's SQL prints and what its connectors read, and the
/// two longer spellings are accepted beside them because being wrong here costs
/// a failed column and accepting an extra spelling costs nothing.
fn float(text: &str, name: &str) -> Result<f64, ArrowError> {
    match text {
        "NaN" | "nan" => Ok(f64::NAN),
        "inf" | "Infinity" => Ok(f64::INFINITY),
        "-inf" | "-Infinity" => Ok(f64::NEG_INFINITY),
        digits => digits
            .parse::<f64>()
            .map_err(|_| wrong(name, "a number", text)),
    }
}

/// `<whole>.<fraction>` as an integer scaled by `digits` decimal places.
///
/// Every datetime Snowflake sends is in this form — seconds past midnight for a
/// `TIME`, seconds since the epoch for a `TIMESTAMP` — so this one function is
/// how all of them are read. The fraction is padded on the right rather than
/// parsed as a number, because `.5` is five hundred thousand microseconds and
/// `.000005` is five, and parsing `5` twice gives the same answer to two
/// different questions.
///
/// The sign is taken off the front before anything else. A timestamp before 1970
/// is negative, and `"-0.5"` has a whole part that parses to zero and loses it.
fn scaled(text: &str, digits: u32) -> Option<i64> {
    let text = text.trim();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }

    let seconds = if whole.is_empty() {
        0i64
    } else {
        whole.parse::<i64>().ok()?
    };
    let mut places = 0i64;
    for at in 0..digits as usize {
        let digit = fraction
            .as_bytes()
            .get(at)
            .map_or(0, |b| i64::from(b - b'0'));
        places = places * 10 + digit;
    }
    let value = seconds
        .checked_mul(10i64.checked_pow(digits)?)?
        .checked_add(places)?;
    Some(if negative { -value } else { value })
}

/// One instant as ISO-8601 text, with as many fraction digits as the column has.
///
/// `offset` says the value carries a second field: `"1706000000.123456789 1920"`,
/// where the number after the space is the zone offset in minutes plus
/// `OFFSET_BIAS`. The rendered text puts the wall clock of that zone first and
/// the offset last, `2024-01-23T12:33:20.123456789+08:00`, which is what a
/// person reading a grid expects and what they would type back into a `WHERE`.
fn instant(text: &str, digits: u32, offset: bool) -> Option<String> {
    let (value, minutes) = match offset {
        false => (text.trim(), 0i64),
        true => {
            let (value, zone) = text.trim().split_once(' ')?;
            (value, zone.trim().parse::<i64>().ok()? - OFFSET_BIAS)
        }
    };

    // Nanoseconds, so that a column finer than microseconds keeps every digit it
    // was stored with — which is the whole reason this column is text.
    let nanos = scaled(value, 9)?;
    let seconds = nanos.div_euclid(1_000_000_000) + minutes * 60;
    // Euclidean, so that the fraction of an instant before 1970 is still a
    // fraction forward from the second below it rather than a negative number.
    let fraction = nanos.rem_euclid(1_000_000_000);

    let clock = arrow::temporal_conversions::timestamp_s_to_datetime(seconds)?;
    let mut out = clock.format("%Y-%m-%dT%H:%M:%S").to_string();
    if digits > 0 {
        let scale = 10u32.pow(9 - digits.min(9)) as i64;
        out.push('.');
        out.push_str(&format!(
            "{:0width$}",
            fraction / scale,
            width = digits as usize
        ));
    }
    if offset {
        let sign = if minutes < 0 { '-' } else { '+' };
        out.push(sign);
        out.push_str(&format!(
            "{:02}:{:02}",
            minutes.abs() / 60,
            minutes.abs() % 60
        ));
    }
    Some(out)
}

/// Hex digits as the bytes they stand for.
///
/// Snowflake writes a `BINARY` as hex rather than base64, upper case by default
/// and lower case when the session's `BINARY_OUTPUT_FORMAT` says so — so both
/// are read, which costs one `match` arm and removes a session setting from the
/// list of things that can break a column.
fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, kind: &str, precision: Option<i64>, scale: Option<i64>) -> Column {
        Column {
            name: name.to_string(),
            kind: kind.to_string(),
            precision,
            scale,
        }
    }

    /// `NUMBER` is every exact number Snowflake has, and the line between an
    /// integer and a decimal is where the integers stop fitting.
    #[test]
    fn a_number_is_an_integer_exactly_while_its_digits_fit_one() {
        assert_eq!(
            cell_of(&column("n", "fixed", Some(18), Some(0))),
            Cell::Int64
        );
        assert_eq!(
            cell_of(&column("n", "fixed", Some(19), Some(0))),
            Cell::Decimal(19, 0)
        );
        assert_eq!(
            cell_of(&column("n", "fixed", Some(38), Some(0))),
            Cell::Decimal(38, 0)
        );
        assert_eq!(
            cell_of(&column("n", "fixed", Some(18), Some(2))),
            Cell::Decimal(18, 2)
        );
        // A `NUMBER` whose metadata could not describe one falls back to its
        // digits rather than panicking.
        assert_eq!(
            cell_of(&column("n", "fixed", Some(39), Some(0))),
            Cell::Text
        );
        assert_eq!(cell_of(&column("n", "fixed", None, Some(0))), Cell::Text);
    }

    /// The line that decides whether a value keeps every digit it was stored
    /// with. Snowflake's default precision for a timestamp is 9.
    #[test]
    fn a_datetime_is_text_exactly_when_it_is_finer_than_the_reader_can_hold() {
        for kind in ["timestamp_ntz", "timestamp_ltz"] {
            let zoned = kind == "timestamp_ltz";
            assert_eq!(
                cell_of(&column("t", kind, None, Some(6))),
                Cell::Timestamp { zoned },
                "{kind}"
            );
            assert_eq!(
                cell_of(&column("t", kind, None, Some(9))),
                Cell::Instant {
                    digits: 9,
                    offset: false
                },
                "{kind}"
            );
        }
        assert_eq!(cell_of(&column("t", "time", None, Some(6))), Cell::Time);
        assert_eq!(
            cell_of(&column("t", "time", None, Some(9))),
            Cell::Instant {
                digits: 9,
                offset: false
            }
        );
        // The zoned one at every precision, because the reason is the zone and
        // not the digits.
        for scale in [0, 3, 6, 9] {
            assert_eq!(
                cell_of(&column("t", "timestamp_tz", None, Some(scale))),
                Cell::Instant {
                    digits: scale as u32,
                    offset: true
                }
            );
        }
    }

    /// Everything with no Arrow home is the text Snowflake wrote, which is why
    /// there are so few arms.
    #[test]
    fn a_type_with_no_arrow_home_arrives_as_text() {
        for kind in [
            "text",
            "variant",
            "object",
            "array",
            "geography",
            "geometry",
            "vector",
            "a type this driver has never met",
        ] {
            assert_eq!(
                cell_of(&column("c", kind, None, None)),
                Cell::Text,
                "{kind}"
            );
        }
    }

    /// The fraction is a position and not a number: `.5` is half a second and
    /// `.000005` is five microseconds, and both parse to `5` as an integer.
    #[test]
    fn a_fraction_is_padded_by_position_and_not_read_as_a_number() {
        assert_eq!(scaled("1.5", 6), Some(1_500_000));
        assert_eq!(scaled("1.000005", 6), Some(1_000_005));
        assert_eq!(scaled("1", 6), Some(1_000_000));
        assert_eq!(scaled("1.", 6), Some(1_000_000));
        assert_eq!(scaled("0.123456", 6), Some(123_456));
        assert_eq!(scaled("45296.123456", 6), Some(45_296_123_456));
    }

    /// A timestamp before 1970 is negative, and the sign is on the whole value
    /// rather than on the seconds — `-0.5` is half a second before the epoch and
    /// its whole part parses to zero.
    #[test]
    fn an_instant_before_the_epoch_keeps_its_sign_through_the_fraction() {
        assert_eq!(scaled("-0.5", 6), Some(-500_000));
        assert_eq!(scaled("-1.25", 6), Some(-1_250_000));
        assert_eq!(scaled("-2208988800", 6), Some(-2_208_988_800_000_000));
    }

    #[test]
    fn something_that_is_not_a_number_is_refused_rather_than_guessed_at() {
        assert_eq!(scaled("", 6), None);
        assert_eq!(scaled("twelve", 6), None);
        assert_eq!(scaled("1.2.3", 6), None);
        assert_eq!(scaled("1e9", 6), None);
    }

    /// The `+1440` bias, which is the single most consequential number in this
    /// file: reading the field as plain minutes puts every zoned timestamp
    /// exactly one day out, and one day is a difference nobody notices in a grid
    /// of dates.
    #[test]
    fn a_zoned_timestamp_renders_in_the_zone_it_was_stored_with() {
        // 2024-01-23T09:33:20Z, stored with a +08:00 offset: 1440 + 480.
        assert_eq!(
            instant("1706002400.123456789 1920", 9, true).as_deref(),
            Some("2024-01-23T17:33:20.123456789+08:00")
        );
        // The same instant in UTC, which is `1440` and not `0`.
        assert_eq!(
            instant("1706002400.123456789 1440", 9, true).as_deref(),
            Some("2024-01-23T09:33:20.123456789+00:00")
        );
        // And behind UTC, where the offset is negative after the bias comes off.
        assert_eq!(
            instant("1706002400.000000000 1140", 9, true).as_deref(),
            Some("2024-01-23T04:33:20.000000000-05:00")
        );
    }

    /// A column with no offset field renders the same way without one, and the
    /// digits it shows are the digits the column was declared with.
    #[test]
    fn an_unzoned_instant_shows_the_digits_its_column_declares() {
        assert_eq!(
            instant("1706002400.123456789", 9, false).as_deref(),
            Some("2024-01-23T09:33:20.123456789")
        );
        assert_eq!(
            instant("1706002400.123456789", 7, false).as_deref(),
            Some("2024-01-23T09:33:20.1234567")
        );
        assert_eq!(
            instant("1706002400.000000000", 0, false).as_deref(),
            Some("2024-01-23T09:33:20")
        );
    }

    /// Both cases, because which one arrives is a session setting rather than a
    /// property of the value.
    #[test]
    fn a_binary_reads_in_either_case_of_hex() {
        assert_eq!(hex("00FF10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(hex(""), Some(Vec::new()));
        assert_eq!(hex("0"), None);
        assert_eq!(hex("zz"), None);
    }

    /// The three values a `FLOAT` can hold that JSON has no spelling for.
    #[test]
    fn a_float_that_is_not_a_number_still_arrives() {
        assert!(float("NaN", "f").unwrap().is_nan());
        assert_eq!(float("inf", "f").unwrap(), f64::INFINITY);
        assert_eq!(float("-inf", "f").unwrap(), f64::NEG_INFINITY);
        assert_eq!(float("2.5", "f").unwrap(), 2.5);
        assert!(float("wibble", "f").is_err());
    }

    /// The whole point of `jsonv2`: a batch built from strings, with nulls where
    /// the account sent nulls.
    #[test]
    fn a_partition_of_strings_becomes_the_columns_it_describes() {
        let plan = Plan::of(&[
            column("N", "fixed", Some(38), Some(0)),
            column("D", "date", None, None),
            column("T", "timestamp_ntz", None, Some(6)),
            column("B", "binary", None, None),
            column("OK", "boolean", None, None),
        ]);
        let rows: Vec<Vec<Value>> = vec![
            vec![
                Value::String("123456789012345678901234567890".into()),
                Value::String("19723".into()),
                Value::String("1706002400.123456".into()),
                Value::String("00FF".into()),
                Value::String("true".into()),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ];
        let batch = plan.batch(&rows).expect("a batch");
        assert_eq!(batch.num_rows(), 2);
        for at in 0..5 {
            assert!(batch.column(at).is_null(1), "column {at} row 2 is a null");
            assert!(!batch.column(at).is_null(0), "column {at} row 1 is not");
        }

        let days =
            arrow::array::cast::as_primitive_array::<arrow::datatypes::Date32Type>(batch.column(1));
        assert_eq!(days.value(0), 19723);
        let raw = arrow::array::cast::as_generic_binary_array::<i32>(batch.column(3));
        assert_eq!(raw.value(0), &[0x00, 0xff]);
    }

    /// A `TIMESTAMP_LTZ` is an instant and says so, where a `TIMESTAMP_NTZ` is a
    /// wall clock and says nothing. Getting this backwards would shift every
    /// value by the reader's own zone.
    #[test]
    fn only_the_column_that_holds_an_instant_carries_a_zone() {
        let plan = Plan::of(&[
            column("L", "timestamp_ltz", None, Some(6)),
            column("N", "timestamp_ntz", None, Some(6)),
        ]);
        assert_eq!(
            plan.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(
            plan.schema().field(1).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, None)
        );
    }

    /// A value that cannot be what its column says it is fails loudly. This is
    /// the property that matters most in a driver nobody has run: a mapping that
    /// is wrong about a type should produce a message, not a column of nulls.
    #[test]
    fn a_value_its_column_cannot_hold_is_a_failure_and_not_a_null() {
        let plan = Plan::of(&[column("N", "fixed", Some(9), Some(0))]);
        let rows: Vec<Vec<Value>> = vec![vec![Value::String("not a number".into())]];
        let message = plan.batch(&rows).expect_err("a failure").to_string();
        assert!(message.contains('N'), "got: {message}");
    }
}
