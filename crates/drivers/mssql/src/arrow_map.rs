//! SQL Server types to Arrow's, and the builders that carry values across.
//!
//! Two things make this driver's mapping different from the PostgreSQL one, and
//! both come from tiberius rather than from the database.
//!
//! **The declared precision of a decimal does not reach us.** TDS sends it in
//! `COLMETADATA`, but `tiberius::Column` exposes only a `ColumnType` and throws
//! the precision and scale away, so a `decimal(18,4)` and a `decimal(38,0)` are
//! indistinguishable before the first row — and Arrow needs the pair to name the
//! column's type. The layout is therefore asked of the server separately (see
//! `lib.rs`, which reads it out of `sys.dm_exec_describe_first_result_set`), and
//! `ColumnLayout` is what carries it here. Where the server declines to describe
//! a statement the column falls back to a normalized layout and every value is
//! rescaled into it, which is exactly what the PostgreSQL driver does for a
//! `numeric` with no declared precision.
//!
//! **`money` has already lost precision by the time we see it.** See
//! `money_to_arrow`.
//!
//! **Two column types have no one Arrow type, and both become text.** A
//! `sql_variant` states its type per value, so a column of them can hold an
//! `int` in one row and an `nvarchar` in the next; text is the only Arrow type
//! that holds every base type it can produce. A `geography`, `geometry` or
//! `hierarchyid` is a CLR type whose bytes are a private structure — see
//! `crate::udt` for what those become and why it is text and not hex.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Float32Builder,
    Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow::datatypes::{DataType, Field, TimeUnit};
use arrow::temporal_conversions::{date32_to_datetime, time64us_to_time, timestamp_us_to_datetime};
use std::sync::Arc;
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime, DateTime2, SmallDateTime, Time};
use tiberius::{ColumnData, ColumnType};

use crate::MsSqlError;
use crate::udt::{self, UdtKind};

/// Days from tiberius' date epoch (0001-01-01, which it numbers day 0) to
/// Arrow's (1970-01-01).
///
/// One less than the PostgreSQL driver's `CE_TO_UNIX_DAYS`, and the difference
/// is not a typo: chrono's `num_days_from_ce` calls 0001-01-01 day *one*, while
/// `tiberius::time::Date::days` calls it day zero. Copying the constant between
/// the two drivers puts every date out by a day.
const CE_TO_UNIX_DAYS: i32 = 719_162;

/// Days from SQL Server's `datetime` epoch (1900-01-01) to Arrow's.
///
/// `datetime` and `smalldatetime` count from 1900 rather than from year one,
/// which is why they cannot share the constant above.
const SQL1900_TO_UNIX_DAYS: i32 = 25_567;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// `money` and `smallmoney` are both a 4-decimal-place fixed-point number.
/// 19 digits holds `money`'s full documented range with room to spare;
/// `smallmoney` fits inside it, which matters because tiberius cannot tell the
/// two apart (see `arrow_field`).
const MONEY_PRECISION: u8 = 19;
const MONEY_SCALE: i8 = 4;

/// Layout for a decimal column the server would not describe.
///
/// The same pair and the same reasoning as the PostgreSQL driver's undeclared
/// `numeric`: wide enough for currency and for scientific values, still inside
/// `Decimal128`. Values are rescaled into it and a value that will not fit fails
/// loudly rather than arriving quietly wrong.
const NUMERIC_PRECISION: u8 = 38;
const NUMERIC_SCALE: i8 = 10;

/// A result column's TDS type, plus what else is needed to name and read it.
///
/// The TDS type alone is not enough twice over: a decimal needs the precision
/// and scale the server declared, and a `Udt` needs the CLR type name, because
/// `geometry`, `geography` and `hierarchyid` share one type byte and their
/// values are opaque byte strings.
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    pub column_type: ColumnType,
    /// Declared precision and scale, for `decimal`/`numeric` only.
    pub decimal: Option<(u8, i8)>,
    /// The CLR type name, for a `Udt` column only.
    pub udt: Option<String>,
}

impl ColumnLayout {
    /// The layout to build a `Decimal128` with: what the server declared, or the
    /// normalized fallback.
    fn decimal_layout(&self) -> (u8, i8) {
        self.decimal.unwrap_or((NUMERIC_PRECISION, NUMERIC_SCALE))
    }

    /// Which CLR type this column holds, for a `Udt` column this driver can
    /// read; the failure names the type so a reader knows what was refused.
    fn udt_kind(&self, name: &str) -> Result<UdtKind, MsSqlError> {
        let type_name = self.udt.as_deref().unwrap_or("");
        UdtKind::from_type_name(type_name).ok_or_else(|| MsSqlError::UnsupportedType {
            column: name.to_string(),
            sql_type: format!("the CLR user-defined type {type_name:?}"),
        })
    }
}

pub fn arrow_field(name: &str, column: &ColumnLayout) -> Result<Field, MsSqlError> {
    let dt = match column.column_type {
        ColumnType::Bit | ColumnType::Bitn => DataType::Boolean,
        // `tinyint` is unsigned in SQL Server and Arrow's `UInt8` would state
        // that exactly, but the front end's reader maps a closed set of format
        // strings and has no case for `C`. `Int16` holds all 256 values without
        // loss and is one the reader knows, so the choice costs nothing a user
        // can see — the structure pane still says `tinyint`, because that comes
        // from the catalog and not from here.
        ColumnType::Int1 => DataType::Int16,
        ColumnType::Int2 => DataType::Int16,
        ColumnType::Int4 => DataType::Int32,
        ColumnType::Int8 | ColumnType::Intn => DataType::Int64,
        ColumnType::Float4 => DataType::Float32,
        ColumnType::Float8 | ColumnType::Floatn => DataType::Float64,
        // Both `money` and `smallmoney` arrive as `Money`: tiberius switches
        // `Intn` and `Floatn` on the declared length but not `Money`, so a
        // nullable `smallmoney` is indistinguishable from a nullable `money`
        // here. One type for both is therefore forced, and the wider one is the
        // only one that can hold both.
        ColumnType::Money | ColumnType::Money4 => {
            DataType::Decimal128(MONEY_PRECISION, MONEY_SCALE)
        }
        ColumnType::Decimaln | ColumnType::Numericn => {
            let (precision, scale) = column.decimal_layout();
            DataType::Decimal128(precision, scale)
        }
        ColumnType::Guid => DataType::Utf8,
        ColumnType::BigChar
        | ColumnType::BigVarChar
        | ColumnType::NChar
        | ColumnType::NVarchar
        | ColumnType::Text
        | ColumnType::NText
        | ColumnType::Xml => DataType::Utf8,
        // `rowversion` lands here too, as `binary(8)`. Its catalog name is
        // `timestamp`, which it is not: it is a row-change counter with no
        // relation to the clock, and rendering it as a time would invent one.
        ColumnType::BigBinary | ColumnType::BigVarBin | ColumnType::Image => DataType::Binary,
        ColumnType::Daten => DataType::Date32,
        // `time(7)` counts in 100 ns and Arrow's nanosecond unit would hold it
        // exactly, but the front end's reader knows only `ttu`. The seventh
        // fractional digit is dropped; `time(6)` and below are exact.
        ColumnType::Timen => DataType::Time64(TimeUnit::Microsecond),
        // Microsecond and not nanosecond, and this one is a hard constraint
        // rather than a preference: a nanosecond timestamp is an i64 count from
        // 1970 and so spans only 1677..2262, while `datetime2` reaches the year
        // 9999. Microsecond is the widest unit that can hold the type at all.
        ColumnType::Datetime2 | ColumnType::Datetime | ColumnType::Datetimen => {
            DataType::Timestamp(TimeUnit::Microsecond, None)
        }
        ColumnType::Datetime4 => DataType::Timestamp(TimeUnit::Microsecond, None),
        // tiberius hands `datetimeoffset` over already converted to UTC. Arrow
        // has one timezone for a whole column and this type stores one per row,
        // so the per-row offset is dropped: `2024-01-01 09:00+09:00` reads back
        // as `2024-01-01 00:00 UTC`. That keeps the column sortable and
        // comparable as an instant, which is what a grid needs; the alternative
        // that keeps the offset is text, which cannot be range-filtered as a
        // time.
        ColumnType::DatetimeOffsetn => {
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        }
        // `SELECT NULL` describes a column with no type at all. Utf8 is the
        // narrowest honest answer — every value in it is null, and a reader that
        // has a case for strings has a case for this.
        ColumnType::Null => DataType::Utf8,
        // `geometry`, `geography` and `hierarchyid`. Text, because the bytes are
        // a private serialization and the text is what SQL Server itself calls
        // the value; see `crate::udt`. A CLR type somebody registered has no
        // text this side could invent, and `udt_kind` refuses it by name here —
        // before a schema exists, which is the only place a whole column can
        // still be turned down cleanly.
        ColumnType::Udt => {
            column.udt_kind(name)?;
            DataType::Utf8
        }
        // A `sql_variant` states its type per value, so the column has no one
        // type and every value is rendered as text. See `variant_text`.
        ColumnType::SSVariant => DataType::Utf8,
    };
    // Every field is nullable. NOT NULL is a constraint this path has not read,
    // and claiming non-null without it corrupts Arrow's validity buffers.
    Ok(Field::new(name, dt, true))
}

/// One builder per column. An enum rather than `Box<dyn ArrayBuilder>` so the
/// per-value append stays a static call — this is the inner loop over every cell
/// in the result.
pub enum ColBuilder {
    Bool(BooleanBuilder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    Float32(Float32Builder),
    Float64(Float64Builder),
    Money(Decimal128Builder),
    /// Carries the column's scale, because every value has to be rescaled into
    /// it and the scale is no longer the same for every decimal column.
    Decimal(Decimal128Builder, i8),
    Utf8(StringBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    TimestampTz(TimestampMicrosecondBuilder),
    Binary(BinaryBuilder),
    /// Carries which CLR type the column holds, because the bytes of one are
    /// indistinguishable from the bytes of another.
    Udt(StringBuilder, UdtKind),
    /// `sql_variant`, where every value arrives under whichever `ColumnData`
    /// variant it actually is and is rendered from there.
    Variant(StringBuilder),
}

impl ColBuilder {
    pub fn new(column: &ColumnLayout, name: &str, capacity: usize) -> Result<Self, MsSqlError> {
        let builder = match column.column_type {
            ColumnType::Bit | ColumnType::Bitn => {
                Self::Bool(BooleanBuilder::with_capacity(capacity))
            }
            ColumnType::Int1 | ColumnType::Int2 => {
                Self::Int16(Int16Builder::with_capacity(capacity))
            }
            ColumnType::Int4 => Self::Int32(Int32Builder::with_capacity(capacity)),
            ColumnType::Int8 | ColumnType::Intn => {
                Self::Int64(Int64Builder::with_capacity(capacity))
            }
            ColumnType::Float4 => Self::Float32(Float32Builder::with_capacity(capacity)),
            ColumnType::Float8 | ColumnType::Floatn => {
                Self::Float64(Float64Builder::with_capacity(capacity))
            }
            ColumnType::Money | ColumnType::Money4 => Self::Money(
                Decimal128Builder::with_capacity(capacity)
                    .with_precision_and_scale(MONEY_PRECISION, MONEY_SCALE)
                    .expect("money's layout is a constant Arrow accepts"),
            ),
            ColumnType::Decimaln | ColumnType::Numericn => {
                let (precision, scale) = column.decimal_layout();
                Self::Decimal(
                    Decimal128Builder::with_capacity(capacity)
                        .with_precision_and_scale(precision, scale)
                        .expect("decimal_layout only returns pairs Arrow accepts"),
                    scale,
                )
            }
            ColumnType::Daten => Self::Date(Date32Builder::with_capacity(capacity)),
            ColumnType::Timen => Self::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            ColumnType::Datetime2
            | ColumnType::Datetime
            | ColumnType::Datetimen
            | ColumnType::Datetime4 => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            ColumnType::DatetimeOffsetn => {
                Self::TimestampTz(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            ColumnType::BigBinary | ColumnType::BigVarBin | ColumnType::Image => {
                Self::Binary(BinaryBuilder::with_capacity(capacity, capacity * 32))
            }
            ColumnType::Udt => Self::Udt(
                StringBuilder::with_capacity(capacity, capacity * 48),
                column.udt_kind(name)?,
            ),
            ColumnType::SSVariant => {
                Self::Variant(StringBuilder::with_capacity(capacity, capacity * 24))
            }
            // Strings, GUIDs, XML, and the typeless column, all of which render
            // as text. Anything that reached here past `arrow_field`'s check
            // lands here too.
            _ => Self::Utf8(StringBuilder::with_capacity(capacity, capacity * 24)),
        };
        Ok(builder)
    }

    pub fn append(&mut self, data: &ColumnData<'_>) -> Result<(), MsSqlError> {
        match (self, data) {
            (Self::Bool(b), ColumnData::Bit(v)) => b.append_option(*v),
            (Self::Int16(b), ColumnData::U8(v)) => b.append_option(v.map(i16::from)),
            (Self::Int16(b), ColumnData::I16(v)) => b.append_option(*v),
            (Self::Int32(b), ColumnData::I32(v)) => b.append_option(*v),
            (Self::Int64(b), ColumnData::I64(v)) => b.append_option(*v),
            (Self::Float32(b), ColumnData::F32(v)) => b.append_option(*v),
            (Self::Float64(b), ColumnData::F64(v)) => b.append_option(*v),
            (Self::Money(b), ColumnData::F64(v)) => {
                b.append_option(v.map(money_to_arrow));
            }
            (Self::Decimal(b, scale), ColumnData::Numeric(v)) => {
                let scale = *scale;
                b.append_option(v.map(|n| numeric_to_arrow(n, scale)).transpose()?);
            }
            (Self::Utf8(b), ColumnData::String(v)) => b.append_option(v.as_deref()),
            (Self::Utf8(b), ColumnData::Guid(v)) => {
                // tiberius has already swapped the byte order SQL Server stores
                // the first three GUID groups in, so this is the same text
                // `CAST(col AS varchar(36))` produces on the server.
                b.append_option(v.map(|u| u.to_string()));
            }
            (Self::Utf8(b), ColumnData::Xml(v)) => {
                b.append_option(v.as_ref().map(|x| x.as_ref().to_string()));
            }
            (Self::Date(b), ColumnData::Date(v)) => b.append_option(v.map(date_to_arrow)),
            (Self::Time(b), ColumnData::Time(v)) => b.append_option(v.map(time_to_arrow)),
            (Self::Timestamp(b), ColumnData::DateTime2(v)) => {
                b.append_option(v.map(datetime2_to_arrow));
            }
            (Self::Timestamp(b), ColumnData::DateTime(v)) => {
                b.append_option(v.map(datetime_to_arrow));
            }
            (Self::Timestamp(b), ColumnData::SmallDateTime(v)) => {
                b.append_option(v.map(small_datetime_to_arrow));
            }
            (Self::TimestampTz(b), ColumnData::DateTimeOffset(v)) => {
                b.append_option(v.map(|d| datetime2_to_arrow(d.datetime2())));
            }
            (Self::Binary(b), ColumnData::Binary(v)) => b.append_option(v.as_deref()),
            // The CLR types. tiberius hands the body over as bytes and the
            // column's type name says how to read them; both halves are needed,
            // which is why the kind travels in the builder.
            (Self::Udt(b, kind), ColumnData::Binary(v)) => {
                let text = match v {
                    Some(bytes) => udt::to_text(*kind, bytes)?,
                    None => None,
                };
                b.append_option(text);
            }
            // A `sql_variant` cell can be anything, so this is the one builder
            // that takes every variant there is.
            (Self::Variant(b), other) => b.append_option(variant_text(other)?),
            // A column whose every value is null still has to have a builder,
            // and the server sends the null under whichever variant it likes.
            (builder, other) if is_null(other) => builder.append_null(),
            (builder, other) => {
                return Err(MsSqlError::UnexpectedValue {
                    expected: builder.describe(),
                    found: variant_name(other),
                });
            }
        }
        Ok(())
    }

    fn append_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::Int16(b) => b.append_null(),
            Self::Int32(b) => b.append_null(),
            Self::Int64(b) => b.append_null(),
            Self::Float32(b) => b.append_null(),
            Self::Float64(b) => b.append_null(),
            Self::Money(b) | Self::Decimal(b, _) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Timestamp(b) | Self::TimestampTz(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Udt(b, _) | Self::Variant(b) => b.append_null(),
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Bool(_) => "boolean",
            Self::Int16(_) => "16-bit integer",
            Self::Int32(_) => "32-bit integer",
            Self::Int64(_) => "64-bit integer",
            Self::Float32(_) => "32-bit float",
            Self::Float64(_) => "64-bit float",
            Self::Money(_) => "money",
            Self::Decimal(..) => "decimal",
            Self::Utf8(_) => "text",
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(_) => "timestamp with offset",
            Self::Binary(_) => "binary",
            Self::Udt(..) => "a CLR user-defined type",
            Self::Variant(_) => "sql_variant",
        }
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int16(b) => Arc::new(b.finish()),
            Self::Int32(b) => Arc::new(b.finish()),
            Self::Int64(b) => Arc::new(b.finish()),
            Self::Float32(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Money(b) | Self::Decimal(b, _) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Timestamp(b) => Arc::new(b.finish()),
            Self::TimestampTz(b) => Arc::new(b.finish().with_timezone("UTC")),
            Self::Binary(b) => Arc::new(b.finish()),
            Self::Udt(b, _) | Self::Variant(b) => Arc::new(b.finish()),
        }
    }
}

/// One `sql_variant` cell as text.
///
/// Every base type a variant can hold is written here, because the alternative
/// is a cell that reads "unsupported" for a value the server was perfectly able
/// to send. The forms are ISO-8601 for the date and time types and the plain
/// number for everything numeric, which is what the rest of this driver's text
/// columns already look like; `binary` becomes the `0x…` literal SQL Server
/// itself accepts back.
///
/// A base type tiberius has no decoder for never reaches here — it fails in the
/// decoder, naming the type byte — so there is no arm for "something else".
fn variant_text(data: &ColumnData<'_>) -> Result<Option<String>, MsSqlError> {
    let text = match data {
        ColumnData::U8(v) => v.map(|v| v.to_string()),
        ColumnData::I16(v) => v.map(|v| v.to_string()),
        ColumnData::I32(v) => v.map(|v| v.to_string()),
        ColumnData::I64(v) => v.map(|v| v.to_string()),
        ColumnData::F32(v) => v.map(|v| v.to_string()),
        ColumnData::F64(v) => v.map(|v| v.to_string()),
        ColumnData::Bit(v) => v.map(|v| u8::from(v).to_string()),
        ColumnData::String(v) => v.as_ref().map(|v| v.to_string()),
        ColumnData::Guid(v) => v.map(|v| v.to_string()),
        ColumnData::Numeric(v) => v.map(|v| v.to_string()),
        ColumnData::Xml(v) => v.as_ref().map(|v| v.as_ref().to_string()),
        // `0x` and two hex digits per byte, which is the literal T-SQL reads
        // back — a bare run of hex would not be.
        ColumnData::Binary(v) => v.as_ref().map(|bytes| {
            let mut out = String::with_capacity(2 + bytes.len() * 2);
            out.push_str("0x");
            for byte in bytes.iter() {
                out.push_str(&format!("{byte:02X}"));
            }
            out
        }),
        ColumnData::Date(v) => v
            .map(|d| {
                date32_to_datetime(date_to_arrow(d))
                    .map(|d| d.date().to_string())
                    .ok_or_else(|| undecodable("date"))
            })
            .transpose()?,
        ColumnData::Time(v) => v
            .map(|t| {
                time64us_to_time(time_to_arrow(t))
                    .map(|t| t.to_string())
                    .ok_or_else(|| undecodable("time"))
            })
            .transpose()?,
        ColumnData::DateTime(v) => v.map(datetime_to_arrow).map(stamp).transpose()?,
        ColumnData::SmallDateTime(v) => v.map(small_datetime_to_arrow).map(stamp).transpose()?,
        ColumnData::DateTime2(v) => v.map(datetime2_to_arrow).map(stamp).transpose()?,
        // Already brought to UTC by tiberius, and said so rather than left to
        // look like a local reading. `datetimeoffset` is not a base type
        // `sql_variant` accepts, so this is here for completeness rather than
        // because a server sends it.
        ColumnData::DateTimeOffset(v) => v
            .map(|d| stamp(datetime2_to_arrow(d.datetime2())).map(|s| format!("{s} UTC")))
            .transpose()?,
    };
    Ok(text)
}

fn stamp(micros: i64) -> Result<String, MsSqlError> {
    timestamp_us_to_datetime(micros)
        .map(|d| d.to_string())
        .ok_or_else(|| undecodable("timestamp"))
}

fn undecodable(sql_type: &'static str) -> MsSqlError {
    MsSqlError::UndecodableValue {
        sql_type,
        reason: "the value is outside the range a calendar date can hold".to_string(),
    }
}

/// Whether a cell holds nothing, whatever variant it arrived under.
fn is_null(data: &ColumnData<'_>) -> bool {
    match data {
        ColumnData::U8(v) => v.is_none(),
        ColumnData::I16(v) => v.is_none(),
        ColumnData::I32(v) => v.is_none(),
        ColumnData::I64(v) => v.is_none(),
        ColumnData::F32(v) => v.is_none(),
        ColumnData::F64(v) => v.is_none(),
        ColumnData::Bit(v) => v.is_none(),
        ColumnData::String(v) => v.is_none(),
        ColumnData::Guid(v) => v.is_none(),
        ColumnData::Binary(v) => v.is_none(),
        ColumnData::Numeric(v) => v.is_none(),
        ColumnData::Xml(v) => v.is_none(),
        ColumnData::DateTime(v) => v.is_none(),
        ColumnData::SmallDateTime(v) => v.is_none(),
        ColumnData::Time(v) => v.is_none(),
        ColumnData::Date(v) => v.is_none(),
        ColumnData::DateTime2(v) => v.is_none(),
        ColumnData::DateTimeOffset(v) => v.is_none(),
    }
}

fn variant_name(data: &ColumnData<'_>) -> &'static str {
    match data {
        ColumnData::U8(_) => "an 8-bit integer",
        ColumnData::I16(_) => "a 16-bit integer",
        ColumnData::I32(_) => "a 32-bit integer",
        ColumnData::I64(_) => "a 64-bit integer",
        ColumnData::F32(_) => "a 32-bit float",
        ColumnData::F64(_) => "a 64-bit float",
        ColumnData::Bit(_) => "a bit",
        ColumnData::String(_) => "a string",
        ColumnData::Guid(_) => "a uniqueidentifier",
        ColumnData::Binary(_) => "binary",
        ColumnData::Numeric(_) => "a decimal",
        ColumnData::Xml(_) => "xml",
        ColumnData::DateTime(_) => "a datetime",
        ColumnData::SmallDateTime(_) => "a smalldatetime",
        ColumnData::Time(_) => "a time",
        ColumnData::Date(_) => "a date",
        ColumnData::DateTime2(_) => "a datetime2",
        ColumnData::DateTimeOffset(_) => "a datetimeoffset",
    }
}

/// Recovers the scaled integer SQL Server stored, from the `f64` tiberius made
/// of it.
///
/// The loss happens inside the decoder, before this driver sees a byte:
/// `money.rs` reads the two halves of the 8-byte integer, adds them as `f64` and
/// divides by 10 000. A `money` value needs 63 bits and an `f64` has 53, so the
/// largest values are already wrong when they arrive and nothing here can undo
/// it. Upstream reached the same conclusion from the other side and reads money
/// as a string rather than trust any numeric accessor.
///
/// Recovery is exact while the magnitude stays under 2^51 ten-thousandths —
/// about 225 billion currency units — because that is where the relative error
/// of the two `f64` operations first reaches half a scaled unit. Above it the
/// answer can be a few ten-thousandths out.
///
/// What is left is a choice between three imperfect answers, and this is the one
/// taken. `Float64` would be honest about the decoder but would then describe
/// every money column as approximate, including the overwhelming majority that
/// are exact. Refusing the column outright would make ordinary tables unreadable
/// — a ledger in a small-denomination currency passes 225 billion routinely — to
/// avoid an error of a few ten-thousandths at the very top of the range. So the
/// value is recovered as the exact decimal it almost always is, and both the
/// exact region and the lossy boundary are pinned by tests rather than left for
/// somebody to rediscover.
///
/// Fixing this properly means patching tiberius to decode `money` into
/// `ColumnData::Numeric`, which is four lines and outside what one driver crate
/// may touch.
fn money_to_arrow(v: f64) -> i128 {
    (v * 10_000.0).round() as i128
}

/// Brings a decimal to the column's scale, which is what Arrow stores.
///
/// TDS sends every value in a column at the column's declared scale, so this is
/// usually the identity. It is written out anyway because "usually" is not a
/// property this can check, and a value silently off by a factor of ten is worse
/// than a multiply that never happens.
fn numeric_to_arrow(n: Numeric, scale: i8) -> Result<i128, MsSqlError> {
    let from = i32::from(n.scale());
    let to = i32::from(scale);
    let overflow = || MsSqlError::NumericOverflow {
        value: format!("{n}"),
        scale,
    };
    match to.cmp(&from) {
        std::cmp::Ordering::Equal => Ok(n.value()),
        std::cmp::Ordering::Greater => 10i128
            .checked_pow((to - from) as u32)
            .and_then(|f| n.value().checked_mul(f))
            .ok_or_else(overflow),
        // Narrowing would drop digits the server sent. Allowed only where the
        // digits being dropped are zeros, because then nothing is lost.
        std::cmp::Ordering::Less => {
            let factor = 10i128
                .checked_pow((from - to) as u32)
                .ok_or_else(overflow)?;
            if n.value() % factor == 0 {
                Ok(n.value() / factor)
            } else {
                Err(overflow())
            }
        }
    }
}

/// Arrow `Date32` counts days from 1970-01-01; tiberius counts from 0001-01-01.
fn date_to_arrow(d: Date) -> i32 {
    d.days() as i32 - CE_TO_UNIX_DAYS
}

/// Arrow `Time64(Microsecond)` counts microseconds from midnight; TDS sends a
/// count of units whose size the column's scale decides.
///
/// `time(7)` counts in 100 ns, so its last digit is divided away here. That is
/// the truncation `arrow_field` documents, and it truncates rather than rounds
/// so a value can never move past the second it was in.
fn time_to_arrow(t: Time) -> i64 {
    let nanos_per_increment = 10i64.pow(9 - u32::from(t.scale()));
    (t.increments() as i64 * nanos_per_increment) / 1_000
}

/// `datetime2` and, once tiberius has converted it to UTC, `datetimeoffset`.
fn datetime2_to_arrow(d: DateTime2) -> i64 {
    date_to_arrow(d.date()) as i64 * MICROS_PER_DAY + time_to_arrow(d.time())
}

/// `datetime`: days from 1900-01-01, and a count of three-hundredths of a second
/// since midnight.
///
/// The tick really is 1/300 s — MS-TDS calls it "one three-hundredths of a
/// second (300 counts per second)" — which is why the stored values end in .000,
/// .003 and .007 milliseconds. Dividing into microseconds is therefore not exact
/// and rounds: 23:59:59.997 is stored as 25 919 999 ticks, whose true value is
/// 86 399.996666… seconds.
fn datetime_to_arrow(d: DateTime) -> i64 {
    let days = (d.days() - SQL1900_TO_UNIX_DAYS) as i64;
    let micros = (d.seconds_fragments() as i64 * 1_000_000 + 150) / 300;
    days * MICROS_PER_DAY + micros
}

/// `smalldatetime`: days from 1900-01-01, and whole minutes since midnight.
///
/// The day count is a `u16`, which is the whole reason this type stops at
/// 2079-06-06.
fn small_datetime_to_arrow(d: SmallDateTime) -> i64 {
    let days = (i32::from(d.days()) - SQL1900_TO_UNIX_DAYS) as i64;
    days * MICROS_PER_DAY + d.seconds_fragments() as i64 * 60_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scaled units of `money` that survive tiberius' `f64` round trip
    /// unchanged. See `money_to_arrow` for where the number comes from.
    const MONEY_EXACT_TO: i128 = 1 << 51;

    fn bare(column_type: ColumnType) -> ColumnLayout {
        ColumnLayout {
            column_type,
            decimal: None,
            udt: None,
        }
    }

    #[test]
    fn date_epoch_is_zero() {
        // The constant is one below the PostgreSQL driver's because tiberius
        // numbers 0001-01-01 as day zero and chrono numbers it as day one.
        // Borrowing the other driver's constant moves every date by a day.
        assert_eq!(date_to_arrow(Date::new(719_162)), 0);
    }

    #[test]
    fn date_converts_both_directions_from_epoch() {
        assert_eq!(date_to_arrow(Date::new(719_163)), 1);
        assert_eq!(date_to_arrow(Date::new(719_161)), -1);
        // 0001-01-01, the earliest `date` there is, read off a live server.
        assert_eq!(date_to_arrow(Date::new(0)), -719_162);
        // 1990-02-28, likewise.
        assert_eq!(date_to_arrow(Date::new(726_525)), 7_363);
    }

    #[test]
    fn time_counts_micros_from_midnight() {
        assert_eq!(time_to_arrow(Time::new(0, 3)), 0);
        // 09:30:00.123 at scale 3, as a live server sends it.
        assert_eq!(time_to_arrow(Time::new(34_200_123, 3)), 34_200_123_000);
        // 23:59:59.999 at scale 3.
        assert_eq!(time_to_arrow(Time::new(86_399_999, 3)), 86_399_999_000);
    }

    #[test]
    fn a_seventh_fractional_digit_is_truncated_not_rounded() {
        // time(7) counts 100 ns units. Arrow's microsecond column cannot hold
        // the last digit, and rounding it up would move 23:59:59.9999999 into
        // the next day.
        let end_of_day = Time::new(863_999_999_999, 7);
        assert_eq!(time_to_arrow(end_of_day), 86_399_999_999);
    }

    #[test]
    fn datetime2_places_a_wall_clock_reading_at_utc() {
        // 1970-01-01 12:30:00, which is 45 000 seconds into the epoch day.
        let noonish = DateTime2::new(Date::new(719_162), Time::new(45_000 * 10_000_000, 7));
        assert_eq!(datetime2_to_arrow(noonish), 45_000_000_000);
    }

    #[test]
    fn datetime2_reaches_the_year_it_claims_to() {
        // 9999-12-31 09:59:59.9999999 UTC, which is what a live server sends for
        // a `datetimeoffset` of 9999-12-31 23:59:59.9999999 +14:00. The point is
        // that it fits an i64 of microseconds at all — a nanosecond column would
        // overflow four thousand years earlier.
        let last = DateTime2::new(Date::new(3_652_058), Time::new(359_999_999_999, 7));
        assert_eq!(datetime2_to_arrow(last), 253_402_250_399_999_999);
    }

    #[test]
    fn datetime_reads_three_hundredths_of_a_second() {
        // 1999-12-31 23:59:59.997 as a live server sends it: day 36 523 from
        // 1900, tick 25 919 999 of 300 per second.
        let legacy = DateTime::new(36_523, 25_919_999);
        let day = 10_956i64 * MICROS_PER_DAY;
        assert_eq!(datetime_to_arrow(legacy), day + 86_399_996_667);
    }

    #[test]
    fn datetime_epoch_is_nineteen_hundred() {
        assert_eq!(datetime_to_arrow(DateTime::new(25_567, 0)), 0);
    }

    #[test]
    fn small_datetime_counts_whole_minutes() {
        assert_eq!(small_datetime_to_arrow(SmallDateTime::new(25_567, 0)), 0);
        assert_eq!(
            small_datetime_to_arrow(SmallDateTime::new(25_567, 90)),
            90 * 60_000_000
        );
        // 2079-06-06 23:59, the last value the type can hold: the day count is a
        // u16 and this is 65 535.
        assert_eq!(
            small_datetime_to_arrow(SmallDateTime::new(65_535, 1_439)),
            (65_535i64 - 25_567) * MICROS_PER_DAY + 1_439 * 60_000_000
        );
    }

    #[test]
    fn money_recovers_the_scaled_integer_exactly() {
        // Values read off a live server, decoded by tiberius into f64 and turned
        // back here. Each of these is what the column actually holds.
        assert_eq!(money_to_arrow(99.9999), 999_999);
        assert_eq!(money_to_arrow(-0.0001), -1);
        assert_eq!(money_to_arrow(0.0), 0);
        // smallmoney's maximum, which is the whole of that type's range.
        assert_eq!(money_to_arrow(214748.3647), 2_147_483_647);
        // The last magnitude the recovery is provably exact at.
        let boundary = (MONEY_EXACT_TO - 1) as f64 / 10_000.0;
        assert_eq!(money_to_arrow(boundary), MONEY_EXACT_TO - 1);
    }

    #[test]
    fn money_beyond_the_f64_mantissa_is_already_lost() {
        // 922 337 203 685 477.5807 is `money`'s documented maximum, and it is
        // not even expressible as an f64 — the nearest one is .6, which is what
        // tiberius hands over, and this is what that recovers to: one
        // ten-thousandth above the stored value. The test asserts the wrong
        // answer on purpose — it is a defect in the decoder, not in this
        // conversion, and pinning it here means a tiberius that ever fixes it
        // fails loudly instead of changing results quietly.
        let as_tiberius_sends_it = 922_337_203_685_477.6f64;
        assert_eq!(
            money_to_arrow(as_tiberius_sends_it),
            9_223_372_036_854_775_808
        );
        assert!(
            money_to_arrow(as_tiberius_sends_it) > MONEY_EXACT_TO,
            "the boundary constant has to sit below where the error starts"
        );
    }

    #[test]
    fn a_decimal_already_at_the_column_scale_is_taken_as_it_is() {
        // TDS sends every value at the declared scale, so this is the path
        // nearly every value takes.
        let n = Numeric::new_with_scale(12_345_678, 4);
        assert_eq!(numeric_to_arrow(n, 4).unwrap(), 12_345_678);
    }

    #[test]
    fn a_decimal_is_widened_to_the_column_scale() {
        let n = Numeric::new_with_scale(15, 1);
        assert_eq!(numeric_to_arrow(n, 4).unwrap(), 15_000);
        assert_eq!(numeric_to_arrow(n, NUMERIC_SCALE).unwrap(), 15_000_000_000);
    }

    #[test]
    fn a_decimal_that_cannot_be_narrowed_without_loss_fails_loudly() {
        // Dropping the digits would report 1.2345 as 1.23, which is a wrong
        // number wearing the right type.
        let n = Numeric::new_with_scale(12_345, 4);
        assert!(numeric_to_arrow(n, 2).is_err());
        // Trailing zeros carry nothing, so narrowing past them is safe.
        assert_eq!(
            numeric_to_arrow(Numeric::new_with_scale(12_300, 4), 2).unwrap(),
            123
        );
    }

    #[test]
    fn the_widest_decimal_survives_the_i128_path() {
        // decimal(38,0) at full magnitude. `rust_decimal` has 96 bits of
        // mantissa and would overflow here, which is why the values are read
        // through `tiberius::numeric::Numeric` instead.
        let widest = 99_999_999_999_999_999_999_999_999_999_999_999_999i128;
        let n = Numeric::new_with_scale(widest, 0);
        assert_eq!(numeric_to_arrow(n, 0).unwrap(), widest);
    }

    #[test]
    fn a_decimal_that_will_not_fit_the_normalized_layout_fails_loudly() {
        // A decimal(38,0) reaching the fallback layout cannot be scaled up by
        // ten decimal places, and quietly wrapping would be far worse than
        // refusing.
        let n = Numeric::new_with_scale(10i128.pow(30), 0);
        assert!(numeric_to_arrow(n, NUMERIC_SCALE).is_err());
    }

    #[test]
    fn supported_types_map_to_expected_arrow_types() {
        let cases = [
            (ColumnType::Bit, DataType::Boolean),
            (ColumnType::Bitn, DataType::Boolean),
            (ColumnType::Int1, DataType::Int16),
            (ColumnType::Int2, DataType::Int16),
            (ColumnType::Int4, DataType::Int32),
            (ColumnType::Int8, DataType::Int64),
            (ColumnType::Float4, DataType::Float32),
            (ColumnType::Float8, DataType::Float64),
            (ColumnType::Guid, DataType::Utf8),
            (ColumnType::NVarchar, DataType::Utf8),
            (ColumnType::BigVarChar, DataType::Utf8),
            (ColumnType::Xml, DataType::Utf8),
            (ColumnType::BigVarBin, DataType::Binary),
            (ColumnType::BigBinary, DataType::Binary),
            (ColumnType::Daten, DataType::Date32),
            (ColumnType::Timen, DataType::Time64(TimeUnit::Microsecond)),
            (
                ColumnType::Datetime2,
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
            (
                ColumnType::Datetimen,
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
            (
                ColumnType::DatetimeOffsetn,
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            ),
            (
                ColumnType::Money,
                DataType::Decimal128(MONEY_PRECISION, MONEY_SCALE),
            ),
            (
                ColumnType::Money4,
                DataType::Decimal128(MONEY_PRECISION, MONEY_SCALE),
            ),
        ];
        for (column_type, expected) in cases {
            let f = arrow_field("c", &bare(column_type)).expect("type should be supported");
            assert_eq!(f.data_type(), &expected, "mapping for {column_type:?}");
        }
    }

    #[test]
    fn a_declared_decimal_keeps_its_own_layout() {
        // decimal(18,4) must not arrive claiming to be the fallback layout: a
        // column that lies about its type makes every reader downstream guess.
        let declared = ColumnLayout {
            column_type: ColumnType::Decimaln,
            decimal: Some((18, 4)),
            udt: None,
        };
        assert_eq!(
            arrow_field("credit_limit", &declared).unwrap().data_type(),
            &DataType::Decimal128(18, 4)
        );
    }

    #[test]
    fn an_undescribed_decimal_falls_back_to_one_layout() {
        assert_eq!(
            arrow_field("d", &bare(ColumnType::Numericn))
                .unwrap()
                .data_type(),
            &DataType::Decimal128(NUMERIC_PRECISION, NUMERIC_SCALE)
        );
    }

    #[test]
    fn all_columns_are_nullable() {
        // Claiming non-null without having read the constraint corrupts Arrow's
        // validity buffers.
        assert!(
            arrow_field("c", &bare(ColumnType::Int4))
                .unwrap()
                .is_nullable()
        );
    }

    fn clr(type_name: &str) -> ColumnLayout {
        ColumnLayout {
            column_type: ColumnType::Udt,
            decimal: None,
            udt: Some(type_name.to_string()),
        }
    }

    #[test]
    fn the_clr_types_and_sql_variant_are_text_columns() {
        // Text and not binary. These used to be refused outright — reading one
        // panicked inside tiberius and took the process with it — and the point
        // of the patched client is that the value now arrives and can be shown
        // as what SQL Server itself calls it.
        for type_name in ["geography", "geometry", "hierarchyid"] {
            assert_eq!(
                arrow_field("place", &clr(type_name)).unwrap().data_type(),
                &DataType::Utf8,
                "mapping for {type_name}"
            );
        }
        // A `sql_variant` states its type per value, so text is the only Arrow
        // type that can hold a column of them.
        assert_eq!(
            arrow_field("v", &bare(ColumnType::SSVariant))
                .unwrap()
                .data_type(),
            &DataType::Utf8
        );
    }

    #[test]
    fn a_clr_type_this_driver_cannot_read_is_refused_by_name() {
        // A type somebody registered themselves. Its bytes mean whatever its
        // assembly says they mean, so there is nothing honest to show; the
        // refusal has to name it, or nobody can tell which column was the
        // problem.
        let err = arrow_field("shape", &clr("Point3D")).unwrap_err();
        match err {
            MsSqlError::UnsupportedType { column, sql_type } => {
                assert_eq!(column, "shape");
                assert!(sql_type.contains("Point3D"), "got {sql_type}");
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }
}
