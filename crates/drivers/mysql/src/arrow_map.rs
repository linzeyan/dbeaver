//! MySQL's result columns as Arrow types, and the builders that carry values
//! across.
//!
//! Everything here reads the column-definition packet and nothing else. A
//! result column can be an expression — `count(*)`, `a + b` — with no catalog
//! row behind it, so `information_schema.COLUMNS` is not available as a source
//! even when the query happens to name a table.
//!
//! Five of MySQL's distinctions are invisible in the wire type and live in the
//! flag word instead, which is why this driver reads flags rather than a
//! rendered type name:
//!
//! - `ENUM` and `SET` are both `MYSQL_TYPE_STRING`, separated from `CHAR` only
//!   by `ENUM_FLAG` / `SET_FLAG`.
//! - every `TEXT` and `BLOB` size arrives as `MYSQL_TYPE_BLOB` — verified
//!   against 8.4, where `TINYTEXT` through `LONGBLOB` all report 252 — and the
//!   only thing separating a `LONGTEXT` from a `LONGBLOB` is character set 63,
//!   the `binary` charset.
//! - `BIGINT` and `BIGINT UNSIGNED` are both `MYSQL_TYPE_LONGLONG`, and the
//!   difference between them is the top half of `u64`.
//!
//! Two mappings differ from the PostgreSQL driver on purpose:
//!
//! **`TIME` is a `Duration`, not a `Time64`.** MySQL's `TIME` is a signed
//! interval that runs to ±838 hours — `TIMEDIFF()` returns one — and Arrow's
//! `Time64` is elapsed time since midnight, which can hold neither the sign nor
//! anything past 24 h. The fixture stores `'-838:59:59'` precisely so that a
//! future edit back to `Time64` fails a test instead of quietly truncating.
//!
//! **`DECIMAL` beyond 38 digits is a `Decimal256`.** MySQL allows
//! `DECIMAL(65, 30)`, which `Decimal128` cannot hold; unlike PostgreSQL, whose
//! unconstrained `numeric` has to be normalized to a fixed pair, every MySQL
//! decimal has a declared precision and every one of them fits `Decimal256`.
//! Values are parsed straight out of the ASCII the server sends into the
//! integer Arrow stores, never through `rust_decimal`, whose 28 significant
//! digits would silently round the wide end of the range.
//!
//! Anything not listed below fails with `UnsupportedType` naming the column. A
//! quiet fallback to text would make the throughput numbers meaningless, since
//! text conversion is the cost this path exists to avoid.
//!
//! Seven of the types below are ahead of the Swift grid, which maps a closed set
//! of Arrow format strings and shows anything else as the format string itself.
//! `Int8` (`c`), the four unsigned widths (`C`, `S`, `I`, `L`), `Duration` (`tDu`)
//! and `Null` (`n`) all land there — which covers `TINYINT`, `BOOL`, every
//! `UNSIGNED` column, `BIT(2..64)` and `TIME`, none of them exotic in a MySQL
//! schema. They are still mapped honestly here: the reader is a closed `switch`
//! that grows by a line per format, and narrowing a `BIGINT UNSIGNED` to fit what
//! it reads today would be corrupting data to make a display work.
//!
//! `Decimal256` is the one that needs saying out loud, because it does not
//! degrade to a placeholder. Its format string is `d:65,30,256`, and the reader
//! recognises the `d:` prefix, ignores the third field and then reads sixteen
//! bytes of a thirty-two-byte value — so a `DECIMAL(65,30)` renders as a
//! confident wrong number rather than as something visibly unsupported. That is
//! reported rather than worked around here, because the alternative is this
//! driver misreporting the column's type.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Decimal256Builder,
    DurationMicrosecondBuilder, Float32Builder, Float64Builder, Int8Builder, Int16Builder,
    Int32Builder, Int64Builder, NullBuilder, StringBuilder, TimestampMicrosecondBuilder,
    UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, TimeUnit, i256};
use chrono::{Datelike, NaiveDate};
use mysql_async::Value;
use mysql_async::consts::{ColumnFlags, ColumnType as WireType};
use std::sync::Arc;

use crate::MySqlError;
use dbconn::DECLARED_NOT_NULL;

/// The `binary` character set. The documented way to tell `BINARY` from `CHAR`,
/// `VARBINARY` from `VARCHAR` and the `BLOB` family from the `TEXT` family,
/// none of which differ in any other field of the column definition.
const BINARY_CHARSET: u16 = 63;

/// Days from 0001-01-01, which is where chrono counts from, to 1970-01-01,
/// which is where Arrow does.
const CE_TO_UNIX_DAYS: i32 = 719_163;

/// The widest `DECIMAL` MySQL will accept, and also the widest `Decimal128` can
/// hold. Above this the column needs a `Decimal256`.
const DECIMAL128_DIGITS: u8 = 38;

/// MySQL's own ceiling on `DECIMAL` precision, comfortably inside
/// `Decimal256`'s 76.
const MYSQL_DECIMAL_DIGITS: u8 = 65;

/// Everything the wire says about one result column.
///
/// Carried as a group because no field means anything alone: the type says
/// `MYSQL_TYPE_STRING` for four different SQL types, and which one it is comes
/// from the flags and the character set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnType {
    pub wire: WireType,
    pub flags: ColumnFlags,
    pub charset: u16,
    /// The width the server would print the value in, which for `DECIMAL`
    /// includes the point and the sign, and for `BIT` is the declared number of
    /// bits rather than bytes.
    pub length: u32,
    pub decimals: u8,
}

impl ColumnType {
    pub fn of(column: &mysql_async::Column) -> Self {
        Self {
            wire: column.column_type(),
            flags: column.flags(),
            charset: column.character_set(),
            length: column.column_length(),
            decimals: column.decimals(),
        }
    }

    fn is_unsigned(&self) -> bool {
        self.flags.contains(ColumnFlags::UNSIGNED_FLAG)
    }

    fn is_binary(&self) -> bool {
        self.charset == BINARY_CHARSET
    }
}

/// Precision and scale for a `NEWDECIMAL` column.
///
/// `column_length` is the printed width, not the digit count: the server counts
/// the decimal point when there is a fraction and the minus sign when the
/// column is signed. Verified against 8.4 in all four combinations —
/// `DECIMAL(10,2)` reports 12, `DECIMAL(12,0)` reports 13, `DECIMAL(65,30)`
/// reports 67, and `DECIMAL(9,3) UNSIGNED` reports 10.
fn decimal_layout(column: &ColumnType) -> (u8, i8) {
    let scale = column.decimals.min(MYSQL_DECIMAL_DIGITS);
    let printed = u32::from(scale > 0) + u32::from(!column.is_unsigned());
    let precision = column
        .length
        .saturating_sub(printed)
        .clamp(1, MYSQL_DECIMAL_DIGITS as u32) as u8;
    // Arrow rejects a scale wider than its precision, and a server that
    // disagreed with the arithmetic above would otherwise take the whole result
    // down at builder construction rather than at the one cell involved.
    (precision.max(scale), scale as i8)
}

pub fn arrow_field(name: &str, column: &ColumnType) -> Result<Field, MySqlError> {
    let dt = match column.wire {
        WireType::MYSQL_TYPE_NULL => DataType::Null,

        // The unsigned widths are not the signed ones: `BIGINT UNSIGNED` runs to
        // 18446744073709551615, roughly twice `i64::MAX`, and an auto_increment
        // id of that type is an ordinary schema rather than an exotic one.
        // There is no `UInt24`, so `MEDIUMINT UNSIGNED` widens to `UInt32`.
        WireType::MYSQL_TYPE_TINY => int(column, DataType::Int8, DataType::UInt8),
        WireType::MYSQL_TYPE_SHORT => int(column, DataType::Int16, DataType::UInt16),
        WireType::MYSQL_TYPE_INT24 | WireType::MYSQL_TYPE_LONG => {
            int(column, DataType::Int32, DataType::UInt32)
        }
        WireType::MYSQL_TYPE_LONGLONG => int(column, DataType::Int64, DataType::UInt64),

        // A year is not a day. Rendering 2024 as `2024-01-01` would invent a
        // month and a day the column does not have, so this stays a number —
        // which is also what the value arrives as.
        WireType::MYSQL_TYPE_YEAR => DataType::Int16,

        WireType::MYSQL_TYPE_FLOAT => DataType::Float32,
        WireType::MYSQL_TYPE_DOUBLE => DataType::Float64,

        WireType::MYSQL_TYPE_NEWDECIMAL | WireType::MYSQL_TYPE_DECIMAL => {
            let (precision, scale) = decimal_layout(column);
            if precision <= DECIMAL128_DIGITS {
                DataType::Decimal128(precision, scale)
            } else {
                DataType::Decimal256(precision, scale)
            }
        }

        WireType::MYSQL_TYPE_DATE | WireType::MYSQL_TYPE_NEWDATE => DataType::Date32,
        // `DATETIME` is a wall-clock reading with no zone and `TIMESTAMP` is an
        // instant, which is the same split PostgreSQL has. The `UTC` tag is
        // honest only because every connection this driver opens sets its
        // session zone to `+00:00`; see `MySqlSource::connect`.
        WireType::MYSQL_TYPE_DATETIME | WireType::MYSQL_TYPE_DATETIME2 => {
            DataType::Timestamp(TimeUnit::Microsecond, None)
        }
        WireType::MYSQL_TYPE_TIMESTAMP | WireType::MYSQL_TYPE_TIMESTAMP2 => {
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        }
        WireType::MYSQL_TYPE_TIME | WireType::MYSQL_TYPE_TIME2 => {
            DataType::Duration(TimeUnit::Microsecond)
        }

        // `BIT(1)` is what MySQL is used for where another database would have a
        // boolean, and it is the one width where the meaning is unambiguous.
        // Anything wider is a mask, and `UInt64` represents every one of them
        // exactly at the cost of forgetting how many bits were declared.
        WireType::MYSQL_TYPE_BIT => {
            if column.length <= 1 {
                DataType::Boolean
            } else {
                DataType::UInt64
            }
        }

        // `ENUM` and `SET` reach here as strings with a flag set, and both are
        // reported as the text the server sends. A dictionary would suit an
        // `ENUM` better, but the member list is not in the column definition —
        // it is only in the catalog, which an expression column does not have.
        WireType::MYSQL_TYPE_STRING
        | WireType::MYSQL_TYPE_VAR_STRING
        | WireType::MYSQL_TYPE_VARCHAR
        | WireType::MYSQL_TYPE_ENUM
        | WireType::MYSQL_TYPE_SET
        | WireType::MYSQL_TYPE_TINY_BLOB
        | WireType::MYSQL_TYPE_MEDIUM_BLOB
        | WireType::MYSQL_TYPE_LONG_BLOB
        | WireType::MYSQL_TYPE_BLOB => text_or_bytes(column),

        // Sent as text even though it is stored parsed, so there is nothing to
        // gain from decoding it here.
        WireType::MYSQL_TYPE_JSON => DataType::Utf8,

        // Well-known binary behind a four-byte SRID, which is a shape the grid
        // cannot render either way; handing over the bytes at least lets an
        // export be correct.
        WireType::MYSQL_TYPE_GEOMETRY => DataType::Binary,

        other => {
            return Err(MySqlError::UnsupportedType {
                column: name.to_string(),
                mysql_type: format!("{other:?}"),
            });
        }
    };
    // Nullable whatever the server declared, because this driver is itself a
    // source of NULLs here: `append_null` substitutes one for a zero date, and a
    // field promising no NULLs over a validity buffer holding them is corrupt
    // rather than merely optimistic. The declaration still has to reach the
    // grid, which draws a substituted NULL differently from a real one, so it
    // travels beside the buffer instead of in it.
    let field = Field::new(name, dt, true);
    if column.flags.contains(ColumnFlags::NOT_NULL_FLAG) {
        return Ok(field.with_metadata([(DECLARED_NOT_NULL.to_string(), "1".to_string())].into()));
    }
    Ok(field)
}

fn int(column: &ColumnType, signed: DataType, unsigned: DataType) -> DataType {
    if column.is_unsigned() {
        unsigned
    } else {
        signed
    }
}

fn text_or_bytes(column: &ColumnType) -> DataType {
    if column.is_binary() {
        DataType::Binary
    } else {
        DataType::Utf8
    }
}

/// One builder per column, for one batch.
///
/// An enum rather than `Box<dyn ArrayBuilder>` so the per-value append stays a
/// static call: this is the loop over every cell in the result.
pub enum ColBuilder {
    Null(NullBuilder),
    Bool(BooleanBuilder),
    Int8(Int8Builder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    UInt8(UInt8Builder),
    UInt16(UInt16Builder),
    UInt32(UInt32Builder),
    UInt64(UInt64Builder),
    Float32(Float32Builder),
    Float64(Float64Builder),
    /// Carries the scale because every value has to be rescaled to the column's,
    /// and MySQL sends the digits rather than a number.
    Decimal128(Decimal128Builder, i8),
    Decimal256(Decimal256Builder, i8),
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
    Date(Date32Builder),
    /// `DATETIME`, read as the wall-clock reading it is.
    Naive(TimestampMicrosecondBuilder),
    /// `TIMESTAMP`, which the server has already converted into the session
    /// zone — UTC, because the driver sets it.
    Utc(TimestampMicrosecondBuilder),
    Duration(DurationMicrosecondBuilder),
}

impl ColBuilder {
    pub fn new(field: &Field, capacity: usize) -> Self {
        match field.data_type() {
            DataType::Null => Self::Null(NullBuilder::new()),
            DataType::Boolean => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            DataType::Int8 => Self::Int8(Int8Builder::with_capacity(capacity)),
            DataType::Int16 => Self::Int16(Int16Builder::with_capacity(capacity)),
            DataType::Int32 => Self::Int32(Int32Builder::with_capacity(capacity)),
            DataType::Int64 => Self::Int64(Int64Builder::with_capacity(capacity)),
            DataType::UInt8 => Self::UInt8(UInt8Builder::with_capacity(capacity)),
            DataType::UInt16 => Self::UInt16(UInt16Builder::with_capacity(capacity)),
            DataType::UInt32 => Self::UInt32(UInt32Builder::with_capacity(capacity)),
            DataType::UInt64 => Self::UInt64(UInt64Builder::with_capacity(capacity)),
            DataType::Float32 => Self::Float32(Float32Builder::with_capacity(capacity)),
            DataType::Float64 => Self::Float64(Float64Builder::with_capacity(capacity)),
            DataType::Decimal128(precision, scale) => Self::Decimal128(
                Decimal128Builder::with_capacity(capacity)
                    .with_precision_and_scale(*precision, *scale)
                    .expect("decimal_layout only produces pairs Arrow accepts"),
                *scale,
            ),
            DataType::Decimal256(precision, scale) => Self::Decimal256(
                Decimal256Builder::with_capacity(capacity)
                    .with_precision_and_scale(*precision, *scale)
                    .expect("decimal_layout only produces pairs Arrow accepts"),
                *scale,
            ),
            DataType::Binary => Self::Binary(BinaryBuilder::with_capacity(capacity, capacity * 32)),
            DataType::Date32 => Self::Date(Date32Builder::with_capacity(capacity)),
            DataType::Timestamp(_, None) => {
                Self::Naive(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            DataType::Timestamp(_, Some(_)) => {
                Self::Utc(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            DataType::Duration(_) => {
                Self::Duration(DurationMicrosecondBuilder::with_capacity(capacity))
            }
            // Utf8 and anything that reached here past `arrow_field`'s check.
            _ => Self::Utf8(StringBuilder::with_capacity(capacity, capacity * 24)),
        }
    }

    /// Appends one cell, taking the value rather than borrowing it so a string
    /// or a blob is moved out of the row instead of copied.
    pub fn append(&mut self, name: &str, value: Value) -> Result<(), MySqlError> {
        if matches!(value, Value::NULL) {
            self.append_null();
            return Ok(());
        }
        match self {
            Self::Null(b) => b.append_null(),
            Self::Bool(b) => b.append_value(bit_value(&value, name)? != 0),
            Self::Int8(b) => b.append_value(fit(signed(&value, name)?, name)?),
            Self::Int16(b) => b.append_value(fit(signed(&value, name)?, name)?),
            Self::Int32(b) => b.append_value(fit(signed(&value, name)?, name)?),
            Self::Int64(b) => b.append_value(signed(&value, name)?),
            Self::UInt8(b) => b.append_value(fit_unsigned(unsigned(&value, name)?, name)?),
            Self::UInt16(b) => b.append_value(fit_unsigned(unsigned(&value, name)?, name)?),
            Self::UInt32(b) => b.append_value(fit_unsigned(unsigned(&value, name)?, name)?),
            Self::UInt64(b) => b.append_value(match &value {
                // A wide `BIT` is bytes on the wire, not a number.
                Value::Bytes(_) => bit_value(&value, name)?,
                other => unsigned(other, name)?,
            }),
            Self::Float32(b) => b.append_value(match value {
                Value::Float(f) => f,
                Value::Double(d) => d as f32,
                other => return Err(wrong(name, "a float", &other)),
            }),
            Self::Float64(b) => b.append_value(match value {
                Value::Double(d) => d,
                Value::Float(f) => f as f64,
                other => return Err(wrong(name, "a double", &other)),
            }),
            Self::Decimal128(b, scale) => {
                let digits = decimal_digits(&value, *scale, name)?;
                b.append_value(digits.parse::<i128>().map_err(|_| MySqlError::Decode {
                    column: name.to_string(),
                    expected: "a decimal inside 128 bits",
                    value: digits,
                })?);
            }
            Self::Decimal256(b, scale) => {
                let digits = decimal_digits(&value, *scale, name)?;
                b.append_value(
                    i256::from_string(&digits).ok_or_else(|| MySqlError::Decode {
                        column: name.to_string(),
                        expected: "a decimal inside 256 bits",
                        value: digits,
                    })?,
                );
            }
            Self::Utf8(b) => match value {
                // The server transcodes result strings into the connection's
                // character set, which is utf8mb4, so this is lossless in
                // practice; the replacement path is here for a server that has
                // been told to send something else rather than to paper over
                // one that has not.
                Value::Bytes(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => b.append_value(s),
                    Err(e) => b.append_value(String::from_utf8_lossy(e.as_bytes())),
                },
                other => b.append_value(other.as_sql(true)),
            },
            Self::Binary(b) => match value {
                Value::Bytes(bytes) => b.append_value(bytes),
                other => return Err(wrong(name, "bytes", &other)),
            },
            Self::Date(b) => match date_parts(&value, name)? {
                Some((y, m, d, _)) => match NaiveDate::from_ymd_opt(y.into(), m.into(), d.into()) {
                    Some(date) => b.append_value(date.num_days_from_ce() - CE_TO_UNIX_DAYS),
                    // A zero date. See `append_null`'s note.
                    None => b.append_null(),
                },
                None => b.append_null(),
            },
            Self::Naive(b) | Self::Utc(b) => match date_parts(&value, name)? {
                Some((y, m, d, micros)) => {
                    match NaiveDate::from_ymd_opt(y.into(), m.into(), d.into()) {
                        Some(date) => {
                            let days = (date.num_days_from_ce() - CE_TO_UNIX_DAYS) as i64;
                            b.append_value(days * 86_400_000_000 + micros)
                        }
                        None => b.append_null(),
                    }
                }
                None => b.append_null(),
            },
            Self::Duration(b) => match value {
                Value::Time(negative, days, hours, minutes, seconds, micros) => {
                    let total = i64::from(days) * 86_400_000_000
                        + i64::from(hours) * 3_600_000_000
                        + i64::from(minutes) * 60_000_000
                        + i64::from(seconds) * 1_000_000
                        + i64::from(micros);
                    b.append_value(if negative { -total } else { total });
                }
                other => return Err(wrong(name, "a time", &other)),
            },
        }
        Ok(())
    }

    /// A NULL, and also what a zero date becomes.
    ///
    /// `'0000-00-00'` is a value a MySQL column can legally hold and has no
    /// Arrow representation — year zero, month zero, day zero is not a point on
    /// any calendar, and `Date32` is a day offset with no room for a
    /// non-value. Reporting it as NULL loses the distinction from a real NULL;
    /// the alternatives were failing the whole column, which would make a table
    /// full of them unreadable, or picking a sentinel date, which is a date
    /// somebody's table also contains. It matches what the client this replaces
    /// already does — Connector/J is configured with
    /// `zeroDateTimeBehavior=CONVERT_TO_NULL` — and the true value stays
    /// reachable as `SELECT CAST(d AS CHAR)`.
    fn append_null(&mut self) {
        match self {
            Self::Null(b) => b.append_null(),
            Self::Bool(b) => b.append_null(),
            Self::Int8(b) => b.append_null(),
            Self::Int16(b) => b.append_null(),
            Self::Int32(b) => b.append_null(),
            Self::Int64(b) => b.append_null(),
            Self::UInt8(b) => b.append_null(),
            Self::UInt16(b) => b.append_null(),
            Self::UInt32(b) => b.append_null(),
            Self::UInt64(b) => b.append_null(),
            Self::Float32(b) => b.append_null(),
            Self::Float64(b) => b.append_null(),
            Self::Decimal128(b, _) => b.append_null(),
            Self::Decimal256(b, _) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Naive(b) | Self::Utc(b) => b.append_null(),
            Self::Duration(b) => b.append_null(),
        }
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Null(b) => Arc::new(b.finish()),
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int8(b) => Arc::new(b.finish()),
            Self::Int16(b) => Arc::new(b.finish()),
            Self::Int32(b) => Arc::new(b.finish()),
            Self::Int64(b) => Arc::new(b.finish()),
            Self::UInt8(b) => Arc::new(b.finish()),
            Self::UInt16(b) => Arc::new(b.finish()),
            Self::UInt32(b) => Arc::new(b.finish()),
            Self::UInt64(b) => Arc::new(b.finish()),
            Self::Float32(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Decimal128(b, _) => Arc::new(b.finish()),
            Self::Decimal256(b, _) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Naive(b) => Arc::new(b.finish()),
            Self::Utc(b) => Arc::new(b.finish().with_timezone("UTC")),
            Self::Duration(b) => Arc::new(b.finish()),
        }
    }
}

fn wrong(column: &str, expected: &'static str, value: &Value) -> MySqlError {
    MySqlError::Decode {
        column: column.to_string(),
        expected,
        value: value.as_sql(true),
    }
}

/// An integer column's value as `i64`.
///
/// Both variants are accepted at every width because the server only widens to
/// `UInt` where it has to: verified against 8.4, `INT UNSIGNED` arrives as
/// `Int(4294967295)` and only `BIGINT UNSIGNED` arrives as `UInt`.
fn signed(value: &Value, column: &str) -> Result<i64, MySqlError> {
    match value {
        Value::Int(i) => Ok(*i),
        Value::UInt(u) => i64::try_from(*u).map_err(|_| wrong(column, "a signed integer", value)),
        _ => Err(wrong(column, "an integer", value)),
    }
}

fn unsigned(value: &Value, column: &str) -> Result<u64, MySqlError> {
    match value {
        Value::UInt(u) => Ok(*u),
        Value::Int(i) => u64::try_from(*i).map_err(|_| wrong(column, "an unsigned integer", value)),
        _ => Err(wrong(column, "an integer", value)),
    }
}

/// Narrows to the width the column declared, refusing rather than wrapping.
///
/// A value outside it means the type mapping and the server disagree about what
/// this column is, and a wrapped number is a wrong answer that looks like a
/// right one.
fn fit<T: TryFrom<i64>>(wide: i64, column: &str) -> Result<T, MySqlError> {
    T::try_from(wide).map_err(|_| MySqlError::Decode {
        column: column.to_string(),
        expected: "an integer of the column's declared width",
        value: wide.to_string(),
    })
}

fn fit_unsigned<T: TryFrom<u64>>(wide: u64, column: &str) -> Result<T, MySqlError> {
    T::try_from(wide).map_err(|_| MySqlError::Decode {
        column: column.to_string(),
        expected: "an integer of the column's declared width",
        value: wide.to_string(),
    })
}

/// A `BIT(M)` value as the mask it is.
///
/// The server sends the minimum number of bytes that hold M bits, most
/// significant first — `BIT(17)` holding `0b1_0101_0101_0101_0101` arrives as
/// `01 55 55`. Reassembling here rather than exposing `FixedSizeBinary` keeps
/// the front end from having to know that.
fn bit_value(value: &Value, column: &str) -> Result<u64, MySqlError> {
    match value {
        Value::Bytes(bytes) if bytes.len() <= 8 => Ok(bytes
            .iter()
            .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte))),
        Value::Int(i) => Ok(*i as u64),
        Value::UInt(u) => Ok(*u),
        _ => Err(wrong(column, "a bit string", value)),
    }
}

/// Year, month, day and microseconds past midnight, or `None` for a value with
/// no calendar meaning.
fn date_parts(value: &Value, column: &str) -> Result<Option<(u16, u8, u8, i64)>, MySqlError> {
    match value {
        Value::Date(year, month, day, hour, minute, second, micros) => {
            let time = i64::from(*hour) * 3_600_000_000
                + i64::from(*minute) * 60_000_000
                + i64::from(*second) * 1_000_000
                + i64::from(*micros);
            Ok(Some((*year, *month, *day, time)))
        }
        _ => Err(wrong(column, "a date", value)),
    }
}

/// A decimal's digits as the integer Arrow stores at `scale`.
///
/// MySQL sends `NEWDECIMAL` as the ASCII the server would print — verified
/// against 8.4, where `DECIMAL(65,30)` arrives as its full 66 characters — so
/// the conversion is moving the point rather than any arithmetic. Nothing here
/// goes through a fixed-width decimal type: `rust_decimal` carries 28
/// significant digits and would round the top of MySQL's range away.
fn decimal_digits(value: &Value, scale: i8, column: &str) -> Result<String, MySqlError> {
    let text = match value {
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Float(f) => f.to_string(),
        _ => return Err(wrong(column, "a decimal", value)),
    };
    rescale(&text, scale).ok_or_else(|| MySqlError::Decode {
        column: column.to_string(),
        expected: "a decimal at the column's declared scale",
        value: text,
    })
}

/// `"-12.5"` at scale 4 becomes `"-125000"`.
///
/// A fraction longer than the column's scale is refused rather than rounded:
/// the server renders every value at the column's own scale, so a longer one
/// means this side has the scale wrong, and quietly dropping digits would hide
/// that behind values that are merely a little bit incorrect.
fn rescale(text: &str, scale: i8) -> Option<String> {
    let scale = usize::try_from(scale).ok()?;
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = match rest.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (rest, ""),
    };
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if fraction.len() > scale {
        return None;
    }
    let mut out = String::with_capacity(sign.len() + whole.len() + scale + 1);
    out.push_str(sign);
    out.push_str(if whole.is_empty() { "0" } else { whole });
    out.push_str(fraction);
    for _ in fraction.len()..scale {
        out.push('0');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column definition as the server would send one.
    fn column(wire: WireType, flags: u16, charset: u16, length: u32, decimals: u8) -> ColumnType {
        ColumnType {
            wire,
            flags: ColumnFlags::from_bits_truncate(flags),
            charset,
            length,
            decimals,
        }
    }

    fn plain(wire: WireType) -> ColumnType {
        column(wire, 0, BINARY_CHARSET, 0, 0)
    }

    const UNSIGNED: u16 = ColumnFlags::UNSIGNED_FLAG.bits();
    const ENUM: u16 = ColumnFlags::ENUM_FLAG.bits();
    const SET: u16 = ColumnFlags::SET_FLAG.bits();
    /// utf8mb4_general_ci, which is what the fixture's text columns report.
    const UTF8MB4: u16 = 45;

    fn mapped(c: &ColumnType) -> DataType {
        arrow_field("c", c)
            .expect("type should be supported")
            .data_type()
            .clone()
    }

    #[test]
    fn unsigned_integers_widen_rather_than_wrap() {
        // The case this exists for: `BIGINT UNSIGNED` runs past `i64::MAX`, so
        // `Int64` would report an auto_increment id as a negative number.
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_LONGLONG, UNSIGNED, 63, 20, 0)),
            DataType::UInt64
        );
        assert_eq!(
            mapped(&plain(WireType::MYSQL_TYPE_LONGLONG)),
            DataType::Int64
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_TINY, UNSIGNED, 63, 3, 0)),
            DataType::UInt8
        );
        // There is no UInt24, so a MEDIUMINT's unsigned form widens a step.
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_INT24, UNSIGNED, 63, 8, 0)),
            DataType::UInt32
        );
        assert_eq!(mapped(&plain(WireType::MYSQL_TYPE_INT24)), DataType::Int32);
    }

    #[test]
    fn the_binary_charset_is_what_separates_text_from_bytes() {
        // Every TEXT and BLOB size arrives as the same wire type, so this is
        // the only discriminator there is. Get it wrong and a JPEG is decoded
        // as text or an article is shown as a byte count.
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_BLOB, 16, UTF8MB4, 262_140, 0)),
            DataType::Utf8
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_BLOB, 144, 63, 65_535, 0)),
            DataType::Binary
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_VAR_STRING, 0, UTF8MB4, 400, 0)),
            DataType::Utf8
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_VAR_STRING, 128, 63, 100, 0)),
            DataType::Binary
        );
    }

    #[test]
    fn an_enum_and_a_set_are_strings_with_a_flag() {
        // Both are MYSQL_TYPE_STRING on the wire; without the flags they are
        // indistinguishable from CHAR, which is the reason this driver reads
        // the flag word at all.
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_STRING, ENUM, UTF8MB4, 20, 0)),
            DataType::Utf8
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_STRING, SET, UTF8MB4, 72, 0)),
            DataType::Utf8
        );
    }

    #[test]
    fn a_time_is_a_duration_and_not_a_time_of_day() {
        // `Time64` is elapsed time since midnight. MySQL's `TIME` is signed and
        // reaches 838 hours, so mapping it that way would corrupt every value
        // `TIMEDIFF()` ever returned.
        assert_eq!(
            mapped(&plain(WireType::MYSQL_TYPE_TIME)),
            DataType::Duration(TimeUnit::Microsecond)
        );
    }

    #[test]
    fn a_timestamp_carries_a_zone_and_a_datetime_does_not() {
        assert_eq!(
            mapped(&plain(WireType::MYSQL_TYPE_DATETIME)),
            DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        assert_eq!(
            mapped(&plain(WireType::MYSQL_TYPE_TIMESTAMP)),
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }

    #[test]
    fn one_bit_is_a_boolean_and_more_is_a_mask() {
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_BIT, UNSIGNED, 63, 1, 0)),
            DataType::Boolean
        );
        assert_eq!(
            mapped(&column(WireType::MYSQL_TYPE_BIT, UNSIGNED, 63, 17, 0)),
            DataType::UInt64
        );
    }

    #[test]
    fn decimal_precision_comes_out_of_the_printed_width() {
        // Lengths read off a live 8.4 server rather than derived here: computing
        // the input from the same formula as the output would prove nothing.
        // DECIMAL(10,2) signed, DECIMAL(12,0) signed, DECIMAL(65,30) signed,
        // DECIMAL(9,3) unsigned.
        assert_eq!(decimal_layout(&plain_decimal(12, 2, false)), (10, 2));
        assert_eq!(decimal_layout(&plain_decimal(13, 0, false)), (12, 0));
        assert_eq!(decimal_layout(&plain_decimal(67, 30, false)), (65, 30));
        assert_eq!(decimal_layout(&plain_decimal(10, 3, true)), (9, 3));
    }

    fn plain_decimal(length: u32, decimals: u8, unsigned: bool) -> ColumnType {
        column(
            WireType::MYSQL_TYPE_NEWDECIMAL,
            if unsigned { UNSIGNED } else { 0 },
            63,
            length,
            decimals,
        )
    }

    #[test]
    fn a_decimal_wider_than_128_bits_gets_256() {
        // MySQL allows 65 digits and Decimal128 holds 38, so the whole range is
        // representable only if the wide end moves up a type. Unlike the
        // PostgreSQL driver there is nothing to normalize away here.
        assert_eq!(
            mapped(&plain_decimal(40, 10, false)),
            DataType::Decimal128(38, 10)
        );
        assert_eq!(
            mapped(&plain_decimal(67, 30, false)),
            DataType::Decimal256(65, 30)
        );
    }

    #[test]
    fn decimal_digits_move_the_point_without_arithmetic() {
        assert_eq!(rescale("-12345678.90", 2).unwrap(), "-1234567890");
        assert_eq!(rescale("1.5", 4).unwrap(), "15000");
        assert_eq!(rescale("123456789012", 0).unwrap(), "123456789012");
        assert_eq!(rescale("0.000", 3).unwrap(), "0000");
        assert_eq!(rescale("-0.25", 2).unwrap(), "-025");
        // The whole 65-digit range, which is the case a fixed-width decimal
        // type would have quietly rounded.
        assert_eq!(
            rescale(
                "-12345678901234567890123456789012345.123456789012345678901234567890",
                30
            )
            .unwrap(),
            "-12345678901234567890123456789012345123456789012345678901234567890"
        );
    }

    #[test]
    fn a_decimal_with_more_digits_than_the_column_is_refused() {
        // Truncating would produce a value that is wrong by a factor of ten and
        // looks entirely plausible.
        assert!(rescale("1.234", 2).is_none());
        assert!(rescale("not a number", 2).is_none());
    }

    #[test]
    fn a_bit_string_is_read_most_significant_byte_first() {
        // `BIT(17)` holding 0b1_0101_0101_0101_0101, as the server sends it.
        assert_eq!(
            bit_value(&Value::Bytes(vec![0x01, 0x55, 0x55]), "c").unwrap(),
            0b1_0101_0101_0101_0101
        );
        assert_eq!(bit_value(&Value::Bytes(vec![0x01]), "c").unwrap(), 1);
        assert_eq!(bit_value(&Value::Bytes(vec![0x00]), "c").unwrap(), 0);
        assert_eq!(
            bit_value(&Value::Bytes(vec![0xff; 8]), "c").unwrap(),
            u64::MAX
        );
    }

    #[test]
    fn a_zero_date_becomes_null_rather_than_a_date_that_is_not_one() {
        // `'0000-00-00'` is a value MyISAM tables from the 5.x era are full of,
        // and no point on the proleptic Gregorian calendar. Failing here would
        // make the whole column unreadable over one legal row.
        let field = arrow_field("d", &plain(WireType::MYSQL_TYPE_DATE)).unwrap();
        let mut builder = ColBuilder::new(&field, 4);
        builder
            .append("d", Value::Date(0, 0, 0, 0, 0, 0, 0))
            .unwrap();
        // The zero-in-date case, which is a different sql_mode flag and the
        // same problem.
        builder
            .append("d", Value::Date(2010, 0, 1, 0, 0, 0, 0))
            .unwrap();
        builder
            .append("d", Value::Date(1970, 1, 1, 0, 0, 0, 0))
            .unwrap();
        let array = builder.finish();
        assert!(array.is_null(0));
        assert!(array.is_null(1));
        assert!(!array.is_null(2));
    }

    #[test]
    fn a_negative_time_survives_the_trip() {
        // Row 3 of the fixture, and the value that fails loudly if `TIME` is
        // ever mapped back to `Time64`.
        let field = arrow_field("t", &plain(WireType::MYSQL_TYPE_TIME)).unwrap();
        let mut builder = ColBuilder::new(&field, 2);
        builder
            .append("t", Value::Time(true, 34, 22, 59, 59, 0))
            .unwrap();
        builder
            .append("t", Value::Time(false, 0, 13, 45, 56, 123_456))
            .unwrap();
        let array = builder.finish();
        let durations = array
            .as_any()
            .downcast_ref::<arrow::array::DurationMicrosecondArray>()
            .unwrap();
        assert_eq!(durations.value(0), -(838 * 3600 + 59 * 60 + 59) * 1_000_000);
        assert_eq!(
            durations.value(1),
            (13 * 3600 + 45 * 60 + 56) * 1_000_000 + 123_456
        );
    }

    #[test]
    fn every_column_is_nullable() {
        // A NOT NULL column is the one that has to be checked, because it is the
        // one where being wrong is expensive: the zero-date substitution puts a
        // NULL in it, so a field that promised none would describe a buffer it
        // does not have.
        let notnull = column(WireType::MYSQL_TYPE_LONG, 1, 63, 11, 0);
        assert!(arrow_field("c", &notnull).unwrap().is_nullable());
    }

    #[test]
    fn a_not_null_column_says_so_in_its_metadata_and_a_nullable_one_says_nothing() {
        // What the grid reads to tell a substituted NULL from a real one. Absence
        // is the signal for a nullable column rather than a "0", so a reader that
        // has never heard of the key behaves the same as one that has.
        let notnull = column(WireType::MYSQL_TYPE_LONG, 1, 63, 11, 0);
        assert_eq!(
            arrow_field("c", &notnull)
                .unwrap()
                .metadata()
                .get(DECLARED_NOT_NULL),
            Some(&"1".to_string())
        );

        let nullable = column(WireType::MYSQL_TYPE_LONG, 0, 63, 11, 0);
        assert!(arrow_field("c", &nullable).unwrap().metadata().is_empty());
    }

    #[test]
    fn a_type_with_no_mapping_fails_loudly() {
        // MySQL 9's VECTOR, which has no Arrow equivalent worth guessing at.
        // Degrading to text quietly would make a benchmark of the conversion
        // path measure the wrong thing.
        let err = arrow_field("embedding", &plain(WireType::MYSQL_TYPE_VECTOR)).unwrap_err();
        match err {
            MySqlError::UnsupportedType { column, mysql_type } => {
                assert_eq!(column, "embedding");
                assert!(mysql_type.contains("VECTOR"), "{mysql_type}");
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }
}
