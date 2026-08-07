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

/// Fixed scale for NUMERIC. PostgreSQL's wire format carries a per-value
/// scale, but Arrow requires one per column, so we normalize. 10 leaves room
/// for currency and scientific values while staying inside Decimal128's range.
const NUMERIC_SCALE: i8 = 10;
const NUMERIC_PRECISION: u8 = 38;

/// Days from 0001-01-01 (chrono's CE epoch) to 1970-01-01 (Arrow's epoch).
const CE_TO_UNIX_DAYS: i32 = 719_163;

pub fn arrow_field(name: &str, pg_type: &Type) -> Result<Field, PgError> {
    let dt = match *pg_type {
        Type::BOOL => DataType::Boolean,
        Type::INT2 => DataType::Int16,
        Type::INT4 => DataType::Int32,
        Type::INT8 => DataType::Int64,
        Type::FLOAT4 => DataType::Float32,
        Type::FLOAT8 => DataType::Float64,
        Type::NUMERIC => DataType::Decimal128(NUMERIC_PRECISION, NUMERIC_SCALE),
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
    Decimal(Decimal128Builder),
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
    pub fn new(pg_type: &Type, capacity: usize) -> Self {
        match *pg_type {
            Type::BOOL => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            Type::INT2 => Self::Int16(Int16Builder::with_capacity(capacity)),
            Type::INT4 => Self::Int32(Int32Builder::with_capacity(capacity)),
            Type::INT8 => Self::Int64(Int64Builder::with_capacity(capacity)),
            Type::FLOAT4 => Self::Float32(Float32Builder::with_capacity(capacity)),
            Type::FLOAT8 => Self::Float64(Float64Builder::with_capacity(capacity)),
            Type::NUMERIC => Self::Decimal(
                Decimal128Builder::with_capacity(capacity)
                    .with_precision_and_scale(NUMERIC_PRECISION, NUMERIC_SCALE)
                    .expect("static precision/scale is valid"),
            ),
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
            Self::Decimal(b) => {
                let v = row.try_get::<_, Option<Decimal>>(idx)?;
                b.append_option(v.map(rescale_decimal).transpose()?);
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
            Self::Decimal(b) => Arc::new(b.finish()),
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
fn rescale_decimal(d: Decimal) -> Result<i128, PgError> {
    let mut d = d;
    d.rescale(NUMERIC_SCALE as u32);
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

    #[test]
    fn decimal_rescales_to_column_scale() {
        // Values arrive at whatever scale PostgreSQL used; Arrow needs them all
        // at NUMERIC_SCALE or the decoded value is wrong by orders of magnitude.
        let d = Decimal::from_str("1.5").unwrap();
        assert_eq!(rescale_decimal(d).unwrap(), 15_000_000_000);

        let d = Decimal::from_str("1234.5678").unwrap();
        assert_eq!(rescale_decimal(d).unwrap(), 12_345_678_000_000);
    }

    #[test]
    fn decimal_handles_negatives_and_zero() {
        assert_eq!(rescale_decimal(Decimal::from_str("0").unwrap()).unwrap(), 0);
        assert_eq!(
            rescale_decimal(Decimal::from_str("-2.25").unwrap()).unwrap(),
            -22_500_000_000
        );
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
            let f = arrow_field("c", &pg).expect("type should be supported");
            assert_eq!(f.data_type(), &expected, "mapping for {pg}");
        }
    }

    #[test]
    fn all_columns_are_nullable() {
        // Claiming non-null without having read the constraint would corrupt
        // Arrow's validity buffers.
        assert!(arrow_field("c", &Type::INT4).unwrap().is_nullable());
    }

    #[test]
    fn unsupported_type_fails_loudly() {
        // Silently degrading to text would make throughput numbers meaningless,
        // since text conversion is the cost this path exists to avoid.
        let err = arrow_field("weird", &Type::POINT).unwrap_err();
        match err {
            PgError::UnsupportedType { column, pg_type } => {
                assert_eq!(column, "weird");
                assert_eq!(pg_type, "point");
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }
}
