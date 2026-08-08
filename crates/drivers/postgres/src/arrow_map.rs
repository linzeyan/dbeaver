//! PostgreSQL type -> Arrow type mapping, and the column builders that carry
//! values across.
//!
//! Phase 0 covers the type mix in the benchmark table. Unsupported types fail
//! loudly rather than silently degrading to text: a quiet fallback would make
//! the performance numbers meaningless, since text conversion is exactly the
//! cost we are trying to measure the absence of.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Float32Builder,
    Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow::datatypes::{DataType, Field, TimeUnit};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use tokio_postgres::Row;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::PgError;

/// Layout for a NUMERIC column whose scale is not declared. PostgreSQL's wire
/// format carries a per-value scale but Arrow requires one per column, so an
/// undeclared column is normalized: 10 leaves room for currency and scientific
/// values while staying inside Decimal128's range.
///
/// This exact pair also tells the front end that the scale was unknown, so it
/// can drop the padding zeros it would otherwise have to print. That makes a
/// genuine `numeric(38,10)` render as though it were undeclared — a cosmetic
/// loss on one exotic type, in exchange for not inventing a side channel.
const NUMERIC_SCALE: i8 = 10;
const NUMERIC_PRECISION: u8 = 38;

/// Size of the length word PostgreSQL subtracts before packing a type modifier.
const VARHDRSZ: i32 = 4;

/// A result column's type together with its modifier, which is where a
/// NUMERIC's declared precision and scale live. Carried as a pair because the
/// modifier means nothing without the type it modifies.
#[derive(Debug, Clone)]
pub struct ColumnType {
    pub pg_type: Type,
    pub modifier: i32,
}

/// Decimal layout for a NUMERIC column, read from its type modifier.
///
/// `numeric` with no declared precision arrives as -1 and has no scale to read.
/// A declared layout is used only where Arrow and `rust_decimal` can both
/// represent it: rescaling below runs through `rust_decimal`, whose own limit
/// is 28 fractional digits, and PostgreSQL 15 and later allow scales this
/// cannot express at all (`numeric(10,-2)` rounds to hundreds). Anything
/// outside that keeps the normalized layout, where the value is still exact.
fn numeric_layout(modifier: i32) -> (u8, i8) {
    let normalized = (NUMERIC_PRECISION, NUMERIC_SCALE);
    if modifier < VARHDRSZ {
        return normalized;
    }
    let packed = modifier - VARHDRSZ;
    let precision = (packed >> 16) & 0xffff;
    // Low 11 bits, two's complement: PostgreSQL 15 and later store scales down
    // to -1000 here, and the sign has to be read back rather than masked off.
    let raw_scale = packed & 0x7ff;
    let scale = if raw_scale > 0x3ff {
        raw_scale - 0x800
    } else {
        raw_scale
    };
    let representable = (1..=NUMERIC_PRECISION as i32).contains(&precision)
        && (0..=28).contains(&scale)
        && scale <= precision;
    if representable {
        (precision as u8, scale as i8)
    } else {
        normalized
    }
}

/// Days from 0001-01-01 (chrono's CE epoch) to 1970-01-01 (Arrow's epoch).
const CE_TO_UNIX_DAYS: i32 = 719_163;

pub fn arrow_field(name: &str, column: &ColumnType) -> Result<Field, PgError> {
    let dt = match column.pg_type {
        Type::BOOL => DataType::Boolean,
        Type::INT2 => DataType::Int16,
        Type::INT4 => DataType::Int32,
        Type::INT8 => DataType::Int64,
        Type::FLOAT4 => DataType::Float32,
        Type::FLOAT8 => DataType::Float64,
        Type::NUMERIC => {
            let (precision, scale) = numeric_layout(column.modifier);
            DataType::Decimal128(precision, scale)
        }
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::JSON | Type::JSONB => {
            DataType::Utf8
        }
        Type::TIMESTAMP => DataType::Timestamp(TimeUnit::Microsecond, None),
        Type::TIMESTAMPTZ => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        Type::DATE => DataType::Date32,
        Type::TIME => DataType::Time64(TimeUnit::Microsecond),
        Type::UUID => DataType::Utf8,
        Type::BYTEA => DataType::Binary,
        ref other => {
            return Err(PgError::UnsupportedType {
                column: name.to_string(),
                pg_type: other.to_string(),
            });
        }
    };
    // Every column is nullable: PostgreSQL NOT NULL is a constraint we have not
    // read at this point, and claiming non-null wrongly corrupts Arrow buffers.
    Ok(Field::new(name, dt, true))
}

/// One builder per column. An enum rather than `Box<dyn ArrayBuilder>` so the
/// per-value append stays a static call — this is the inner loop over every
/// cell in the result.
pub enum ColBuilder {
    Bool(BooleanBuilder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    Float32(Float32Builder),
    Float64(Float64Builder),
    /// Carries the column's scale: every value has to be rescaled to it, and it
    /// is no longer the same for every NUMERIC column.
    Decimal(Decimal128Builder, i8),
    Utf8(StringBuilder),
    Json(StringBuilder),
    Uuid(StringBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    TimestampTz(TimestampMicrosecondBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Binary(BinaryBuilder),
}

impl ColBuilder {
    pub fn new(column: &ColumnType, capacity: usize) -> Self {
        match column.pg_type {
            Type::BOOL => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            Type::INT2 => Self::Int16(Int16Builder::with_capacity(capacity)),
            Type::INT4 => Self::Int32(Int32Builder::with_capacity(capacity)),
            Type::INT8 => Self::Int64(Int64Builder::with_capacity(capacity)),
            Type::FLOAT4 => Self::Float32(Float32Builder::with_capacity(capacity)),
            Type::FLOAT8 => Self::Float64(Float64Builder::with_capacity(capacity)),
            Type::NUMERIC => {
                let (precision, scale) = numeric_layout(column.modifier);
                Self::Decimal(
                    Decimal128Builder::with_capacity(capacity)
                        .with_precision_and_scale(precision, scale)
                        .expect("numeric_layout only returns pairs Arrow accepts"),
                    scale,
                )
            }
            Type::JSON | Type::JSONB => {
                Self::Json(StringBuilder::with_capacity(capacity, capacity * 32))
            }
            Type::UUID => Self::Uuid(StringBuilder::with_capacity(capacity, capacity * 36)),
            Type::TIMESTAMP => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            Type::TIMESTAMPTZ => {
                Self::TimestampTz(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            Type::DATE => Self::Date(Date32Builder::with_capacity(capacity)),
            Type::TIME => Self::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            Type::BYTEA => Self::Binary(BinaryBuilder::with_capacity(capacity, capacity * 32)),
            // Text and anything that reached here past `arrow_field`'s check.
            _ => Self::Utf8(StringBuilder::with_capacity(capacity, capacity * 24)),
        }
    }

    pub fn append(&mut self, row: &Row, idx: usize) -> Result<(), PgError> {
        match self {
            Self::Bool(b) => b.append_option(row.try_get::<_, Option<bool>>(idx)?),
            Self::Int16(b) => b.append_option(row.try_get::<_, Option<i16>>(idx)?),
            Self::Int32(b) => b.append_option(row.try_get::<_, Option<i32>>(idx)?),
            Self::Int64(b) => b.append_option(row.try_get::<_, Option<i64>>(idx)?),
            Self::Float32(b) => b.append_option(row.try_get::<_, Option<f32>>(idx)?),
            Self::Float64(b) => b.append_option(row.try_get::<_, Option<f64>>(idx)?),
            Self::Decimal(b, scale) => {
                let scale = *scale;
                let v = row.try_get::<_, Option<Decimal>>(idx)?;
                b.append_option(v.map(|d| rescale_decimal(d, scale)).transpose()?);
            }
            Self::Utf8(b) => b.append_option(row.try_get::<_, Option<&str>>(idx)?),
            Self::Json(b) => {
                let v = row.try_get::<_, Option<serde_json::Value>>(idx)?;
                b.append_option(v.map(|j| j.to_string()));
            }
            Self::Uuid(b) => {
                let v = row.try_get::<_, Option<Uuid>>(idx)?;
                b.append_option(v.map(|u| u.to_string()));
            }
            Self::Timestamp(b) => {
                let v = row.try_get::<_, Option<NaiveDateTime>>(idx)?;
                b.append_option(v.map(timestamp_to_arrow));
            }
            Self::TimestampTz(b) => {
                let v = row.try_get::<_, Option<DateTime<Utc>>>(idx)?;
                b.append_option(v.map(|t| t.timestamp_micros()));
            }
            Self::Date(b) => {
                let v = row.try_get::<_, Option<NaiveDate>>(idx)?;
                b.append_option(v.map(date_to_arrow));
            }
            Self::Time(b) => {
                let v = row.try_get::<_, Option<NaiveTime>>(idx)?;
                b.append_option(v.map(time_to_arrow));
            }
            Self::Binary(b) => b.append_option(row.try_get::<_, Option<&[u8]>>(idx)?),
        }
        Ok(())
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int16(b) => Arc::new(b.finish()),
            Self::Int32(b) => Arc::new(b.finish()),
            Self::Int64(b) => Arc::new(b.finish()),
            Self::Float32(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Decimal(b, _) => Arc::new(b.finish()),
            Self::Utf8(b) | Self::Json(b) | Self::Uuid(b) => Arc::new(b.finish()),
            Self::Timestamp(b) => Arc::new(b.finish()),
            Self::TimestampTz(b) => Arc::new(b.finish().with_timezone("UTC")),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
        }
    }
}

/// `rust_decimal` carries its own scale; Arrow needs every value at the
/// column's fixed scale.
fn rescale_decimal(d: Decimal, scale: i8) -> Result<i128, PgError> {
    let mut d = d;
    d.rescale(scale as u32);
    d.mantissa()
        .to_i128()
        .ok_or_else(|| PgError::NumericOverflow(d.to_string()))
}

/// Arrow Date32 counts days from the Unix epoch; chrono counts from 0001-01-01.
fn date_to_arrow(d: NaiveDate) -> i32 {
    d.num_days_from_ce() - CE_TO_UNIX_DAYS
}

/// Arrow Time64 counts microseconds from midnight.
fn time_to_arrow(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000 + (t.nanosecond() as i64 / 1_000)
}

/// PostgreSQL TIMESTAMP has no zone; Arrow stores microseconds from the Unix
/// epoch. Treating the naive value as UTC keeps the wall-clock reading intact,
/// which is what a timezone-less column means.
fn timestamp_to_arrow(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_micros()
}

/// chrono's `Datelike` is needed for `num_days_from_ce`; imported here to keep
/// the trait import next to its single use.
use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn date_epoch_is_zero() {
        // The whole conversion hinges on this constant being right, and an
        // off-by-one here shifts every date in every result by a day.
        assert_eq!(date_to_arrow(date("1970-01-01")), 0);
    }

    #[test]
    fn date_converts_both_directions_from_epoch() {
        assert_eq!(date_to_arrow(date("1970-01-02")), 1);
        assert_eq!(date_to_arrow(date("1969-12-31")), -1);
        assert_eq!(date_to_arrow(date("2020-01-01")), 18262);
    }

    #[test]
    fn date_handles_leap_day() {
        assert_eq!(
            date_to_arrow(date("2020-03-01")) - date_to_arrow(date("2020-02-28")),
            2,
            "2020 is a leap year, so Feb 29 sits between these dates"
        );
    }

    #[test]
    fn time_counts_micros_from_midnight() {
        assert_eq!(time_to_arrow(NaiveTime::from_hms_opt(0, 0, 0).unwrap()), 0);
        assert_eq!(
            time_to_arrow(NaiveTime::from_hms_opt(1, 0, 0).unwrap()),
            3_600_000_000
        );
        assert_eq!(
            time_to_arrow(NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap()),
            86_399_999_999
        );
    }

    #[test]
    fn timestamp_preserves_wall_clock_reading() {
        let t = NaiveDate::from_ymd_opt(2020, 1, 1)
            .unwrap()
            .and_hms_opt(12, 30, 0)
            .unwrap();
        // 18262 days to 2020-01-01, plus 12h30m.
        assert_eq!(
            timestamp_to_arrow(t),
            18262 * 86_400_000_000 + 45_000_000_000
        );
    }

    /// A column of `pg_type` with no declared modifier.
    fn bare(pg_type: Type) -> ColumnType {
        ColumnType {
            pg_type,
            modifier: -1,
        }
    }

    #[test]
    fn decimal_rescales_to_column_scale() {
        // Values arrive at whatever scale PostgreSQL used; Arrow needs them all
        // at the column's scale or the decoded value is wrong by orders of
        // magnitude.
        let d = Decimal::from_str("1.5").unwrap();
        assert_eq!(rescale_decimal(d, NUMERIC_SCALE).unwrap(), 15_000_000_000);
        assert_eq!(rescale_decimal(d, 2).unwrap(), 150);

        let d = Decimal::from_str("1234.5678").unwrap();
        assert_eq!(
            rescale_decimal(d, NUMERIC_SCALE).unwrap(),
            12_345_678_000_000
        );
        assert_eq!(rescale_decimal(d, 4).unwrap(), 12_345_678);
    }

    #[test]
    fn decimal_handles_negatives_and_zero() {
        assert_eq!(
            rescale_decimal(Decimal::from_str("0").unwrap(), NUMERIC_SCALE).unwrap(),
            0
        );
        assert_eq!(
            rescale_decimal(Decimal::from_str("-2.25").unwrap(), NUMERIC_SCALE).unwrap(),
            -22_500_000_000
        );
        assert_eq!(
            rescale_decimal(Decimal::from_str("-2.25").unwrap(), 2).unwrap(),
            -225
        );
    }

    #[test]
    fn numeric_layout_reads_the_declared_precision_and_scale() {
        // Modifiers as PostgreSQL 17 packs them, taken from pg_attribute rather
        // than derived here — the point of the test is to pin the encoding, and
        // computing the input from the same formula as the output proves nothing.
        assert_eq!(numeric_layout(786_438), (12, 2)); // numeric(12,2)
        assert_eq!(numeric_layout(1_179_656), (18, 4)); // numeric(18,4)
        assert_eq!(numeric_layout(327_684), (5, 0)); // numeric(5,0)
        assert_eq!(numeric_layout(2_490_382), (38, 10)); // numeric(38,10)
    }

    #[test]
    fn numeric_layout_falls_back_where_it_cannot_be_represented() {
        // Undeclared: `numeric` with no precision at all.
        assert_eq!(numeric_layout(-1), (NUMERIC_PRECISION, NUMERIC_SCALE));
        // numeric(10,-2), legal since PostgreSQL 15: rounds to hundreds, which
        // the rescale below cannot express. The value stays exact at scale 10.
        assert_eq!(numeric_layout(657_410), (NUMERIC_PRECISION, NUMERIC_SCALE));
        // Wider than Decimal128 can hold: numeric(50,2).
        let wide = ((50 << 16) | 2) + VARHDRSZ;
        assert_eq!(numeric_layout(wide), (NUMERIC_PRECISION, NUMERIC_SCALE));
    }

    #[test]
    fn supported_types_map_to_expected_arrow_types() {
        let cases = [
            (Type::INT2, DataType::Int16),
            (Type::INT4, DataType::Int32),
            (Type::INT8, DataType::Int64),
            (Type::FLOAT4, DataType::Float32),
            (Type::FLOAT8, DataType::Float64),
            (Type::BOOL, DataType::Boolean),
            (Type::TEXT, DataType::Utf8),
            (Type::JSONB, DataType::Utf8),
            (Type::UUID, DataType::Utf8),
            (Type::BYTEA, DataType::Binary),
            (Type::DATE, DataType::Date32),
            (Type::TIME, DataType::Time64(TimeUnit::Microsecond)),
            (
                Type::NUMERIC,
                DataType::Decimal128(NUMERIC_PRECISION, NUMERIC_SCALE),
            ),
            (
                Type::TIMESTAMP,
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
        ];
        for (pg, expected) in cases {
            let name = pg.to_string();
            let f = arrow_field("c", &bare(pg)).expect("type should be supported");
            assert_eq!(f.data_type(), &expected, "mapping for {name}");
        }
    }

    #[test]
    fn a_declared_numeric_keeps_its_own_scale() {
        // The whole point: numeric(12,2) must not arrive claiming to be
        // numeric(38,10). A column that lies about its type makes every reader
        // downstream of it guess.
        let f = arrow_field(
            "revenue",
            &ColumnType {
                pg_type: Type::NUMERIC,
                modifier: 786_438,
            },
        )
        .unwrap();
        assert_eq!(f.data_type(), &DataType::Decimal128(12, 2));
    }

    #[test]
    fn all_columns_are_nullable() {
        // Claiming non-null without having read the constraint would corrupt
        // Arrow's validity buffers.
        assert!(arrow_field("c", &bare(Type::INT4)).unwrap().is_nullable());
    }

    #[test]
    fn unsupported_type_fails_loudly() {
        // Silently degrading to text would make throughput numbers meaningless,
        // since text conversion is the cost this path exists to avoid.
        let err = arrow_field("weird", &bare(Type::POINT)).unwrap_err();
        match err {
            PgError::UnsupportedType { column, pg_type } => {
                assert_eq!(column, "weird");
                assert_eq!(pg_type, "point");
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }
}
