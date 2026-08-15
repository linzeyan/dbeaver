//! CQL's types to Arrow's, decided from the column metadata the first page
//! carries.
//!
//! **The rule.** A scalar gets the narrowest Arrow type that holds every value
//! it can take exactly, out of the twelve the reader at the other end of the FFI
//! has a case for — `apps/macos/Sources/DbClient/ArrowTable.swift` maps format
//! strings for `b`, `s`, `i`, `l`, `f`, `g`, `u`, `z`, `tdD`, `ttu`, `tsu:` and
//! `d:`. Where no such type exists, the value becomes text. A column that
//! arrives as anything else is drawn as its own format string in every cell,
//! which is not a column.
//!
//! **Every composite is one JSON string cell.** `list`, `set`, `map`, `tuple`,
//! `vector` and a user-defined type all end up in `Utf8`, holding the JSON a
//! person would write. The reason is the same one MongoDB's `shape.rs` records
//! for a sub-document, and it is worth stating once rather than six times:
//! Arrow's `List`, `Map` and `Struct` exist and the reader has no case for any
//! of them, so mapping onto them would produce a grid of `<+l>`, `<+m>` and
//! `<+s>`. Flattening instead — a column per UDT field, a column per tuple
//! element — reads well on the rows that have them and explodes into mostly-null
//! columns on the rows that do not, and there is no flattening at all for a
//! `list` whose length differs per row.
//!
//! **Three scalars are text for a reason worth reading.**
//!
//! - `varint` and `decimal` are arbitrary precision. Arrow's `Decimal128` fixes
//!   one precision and scale for the whole column, and CQL declares neither —
//!   the scale travels with each value — so mapping onto it means choosing a
//!   scale from whatever the first page happened to hold and silently rescaling
//!   every value that disagrees. For a type whose purpose is money, the digits
//!   as stored are the only safe answer. This is MongoDB's `Decimal128`
//!   situation and it gets MongoDB's answer.
//! - `time` is nanoseconds since midnight and the reader's only time case is
//!   `ttu`, microseconds. Truncating would drop three digits of every value in
//!   the column, and unlike ClickHouse's `DateTime64(9)` there is no
//!   lower-precision spelling the user could have chosen instead: in CQL every
//!   `time` is nanoseconds. So the exact `HH:MM:SS.fffffffff` goes across as
//!   text, which is also the CQL literal syntax for it.
//!
//! `date` and `timestamp` do fit, and exactly. A CQL date is a `u32` day count
//! biased by 2^31, which is precisely Arrow's `Date32` shifted — the whole range
//! maps with nothing to spare and nothing lost. A CQL timestamp is milliseconds
//! and the reader wants microseconds, which is a multiplication.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, RecordBatch, StringBuilder,
    TimestampMicrosecondBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::error::ArrowError;
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::response::query_result::ColumnSpecs;
use scylla::value::{CqlDate, CqlDecimal, CqlDuration, CqlTime, CqlTimestamp, CqlValue, Row};
use std::sync::Arc;

/// The offset between CQL's day count and Arrow's.
///
/// CQL counts days from 2^31 days before the Unix epoch so that the count is
/// unsigned; Arrow counts signed days from the epoch itself. The subtraction
/// therefore takes the full `u32` range onto the full `i32` range and cannot
/// overflow in either direction, which is why nothing here is fallible.
const DAY_ZERO: i64 = 1 << 31;

/// How many zeros a decimal may be padded with before it is written in
/// exponent form instead.
///
/// A CQL `decimal` carries a signed 32-bit scale read off the wire, so
/// `10^2000000000` is a value the server can legally send. Padding that out
/// would allocate two gigabytes to render one cell, so past this width the
/// digits and the exponent are printed separately — which is exact, short, and
/// the same thing every big-decimal library prints.
const MAX_PLACES: usize = 4096;

/// Which Arrow builder a column's values go into.
///
/// A small closed set rather than `DataType` itself, because the decision this
/// makes is "which builder", and two CQL types that land in the same builder —
/// `bigint` and `counter`, or the nine that land in `Text` — should not each be
/// a case further down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cell {
    Bool,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Binary,
    Date,
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
            Cell::Binary => DataType::Binary,
            Cell::Date => DataType::Date32,
            // UTC stated rather than left off. A CQL timestamp is an absolute
            // instant with no zone of its own, and a timestamp column with no
            // timezone means local time to every consumer of Arrow.
            Cell::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            Cell::Text => DataType::Utf8,
        }
    }
}

/// The builder one CQL type's values belong in.
pub(crate) fn cell_of(typ: &ColumnType<'_>) -> Cell {
    let ColumnType::Native(native) = typ else {
        // Collection, tuple, vector and user-defined type, all as JSON text —
        // see the module comment for why, and why it is one arm rather than
        // four.
        return Cell::Text;
    };
    match native {
        NativeType::Boolean => Cell::Bool,
        // Arrow has `Int8` and the reader does not, and every `tinyint` fits an
        // `Int16` exactly, so widening costs a byte per value and loses nothing.
        NativeType::TinyInt | NativeType::SmallInt => Cell::Int16,
        NativeType::Int => Cell::Int32,
        // A counter is a 64-bit integer that only a `+=` may write. Nothing
        // about reading one differs from a `bigint`.
        NativeType::BigInt | NativeType::Counter => Cell::Int64,
        NativeType::Float => Cell::Float32,
        NativeType::Double => Cell::Float64,
        NativeType::Blob => Cell::Binary,
        NativeType::Date => Cell::Date,
        NativeType::Timestamp => Cell::Timestamp,
        // `ascii`, `text`, `uuid`, `timeuuid`, `inet`, `duration`, `varint`,
        // `decimal` and `time`. The first six have no numeric or temporal
        // reading at all; the last three are the module comment's three.
        _ => Cell::Text,
    }
}

/// What one statement's result looks like, settled before any row is handed
/// over.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    schema: SchemaRef,
    cells: Vec<Cell>,
}

impl Plan {
    /// The plan for a result whose columns the server has described.
    pub(crate) fn of(specs: ColumnSpecs<'_, '_>) -> Plan {
        let cells: Vec<Cell> = specs.iter().map(|spec| cell_of(spec.typ())).collect();
        let fields: Vec<Field> = specs
            .iter()
            .zip(&cells)
            // Every column is nullable without exception, including a primary
            // key one. CQL forbids a null in a key of a *table*, but a result is
            // not a table: a `SELECT max(id)` over no rows produces a null in a
            // column that cannot hold one, and a schema promising otherwise
            // would be a promise about the wrong thing.
            .map(|(spec, cell)| Field::new(spec.name(), cell.arrow(), true))
            .collect();
        Plan {
            schema: Arc::new(Schema::new(fields)),
            cells,
        }
    }

    /// The plan for a statement that answered with no result set at all — an
    /// `INSERT`, a `CREATE TABLE`. No columns, because there were none.
    pub(crate) fn empty() -> Plan {
        Plan {
            schema: Arc::new(Schema::empty()),
            cells: Vec::new(),
        }
    }

    pub(crate) fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Packs one page of rows into a batch of this shape.
    pub(crate) fn batch(&self, rows: &[Row]) -> Result<RecordBatch, ArrowError> {
        let columns: Vec<ArrayRef> = (0..self.cells.len())
            .map(|at| self.column(at, rows))
            .collect();
        RecordBatch::try_new(self.schema(), columns)
    }

    fn column(&self, at: usize, rows: &[Row]) -> ArrayRef {
        // A row shorter than the schema is not something a CQL result produces,
        // but reading past its end would panic where a null is the honest
        // answer. `CqlValue::Empty` is folded in with the nulls here: it is the
        // zero-length legacy value, distinct from null on the wire and carrying
        // no content, and a grid has one empty cell for both.
        let values = rows.iter().map(|row| {
            row.columns
                .get(at)
                .and_then(Option::as_ref)
                .filter(|value| !matches!(value, CqlValue::Empty))
        });
        let rows_len = rows.len();

        match self.cells[at] {
            Cell::Bool => {
                let mut b = BooleanBuilder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::Boolean(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Int16 => {
                let mut b = Int16Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::SmallInt(x)) => b.append_value(*x),
                        Some(CqlValue::TinyInt(x)) => b.append_value(i16::from(*x)),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Int32 => {
                let mut b = Int32Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::Int(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Int64 => {
                let mut b = Int64Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::BigInt(x)) => b.append_value(*x),
                        Some(CqlValue::Counter(x)) => b.append_value(x.0),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Float32 => {
                let mut b = Float32Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::Float(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Float64 => {
                let mut b = Float64Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::Double(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Binary => {
                let mut b = BinaryBuilder::new();
                for value in values {
                    match value {
                        Some(CqlValue::Blob(x)) => b.append_value(x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Date => {
                let mut b = Date32Builder::with_capacity(rows_len);
                for value in values {
                    match value {
                        Some(CqlValue::Date(x)) => b.append_value(days_of(*x)),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Timestamp => {
                let mut b =
                    TimestampMicrosecondBuilder::with_capacity(rows_len).with_timezone("UTC");
                for value in values {
                    match value {
                        // The one place a scalar can fail to fit. Milliseconds
                        // reach year 292 million and microseconds only reach
                        // 292 thousand, so a timestamp past the year 294247 has
                        // nowhere to go — and `Timestamp(Millisecond)` is not a
                        // format string the reader has a case for, so widening
                        // the column instead is not available.
                        Some(CqlValue::Timestamp(CqlTimestamp(ms))) => {
                            match ms.checked_mul(1_000) {
                                Some(us) => b.append_value(us),
                                None => b.append_null(),
                            }
                        }
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            Cell::Text => {
                let mut b = StringBuilder::new();
                for value in values {
                    match value {
                        Some(x) => b.append_value(text_of(x)),
                        None => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
        }
    }
}

/// A CQL date as the day count Arrow uses.
fn days_of(date: CqlDate) -> i32 {
    (i64::from(date.0) - DAY_ZERO) as i32
}

/// One value as the text a grid cell shows.
///
/// Not `CqlValue`'s own `Display`, which renders a `varint` as
/// `blobAsVarint(0x…)` and a `decimal` as `blobAsDecimal(0x…)`. Those are CQL
/// expressions that reconstruct the value, which is what a driver writing a
/// statement needs and the opposite of what a person reading a cell needs.
fn text_of(value: &CqlValue) -> String {
    scalar_text(value).unwrap_or_else(|| json_of(value))
}

/// A value that is not a composite, as text; `None` for one that is.
///
/// The `None` is what keeps this and `json_of` from calling each other forever.
/// `CqlValue` is `#[non_exhaustive]`, so a variant added by a later driver
/// release lands in the final arm — and that arm has to be one that terminates,
/// which is why it renders rather than deferring.
fn scalar_text(value: &CqlValue) -> Option<String> {
    let text = match value {
        CqlValue::List(_)
        | CqlValue::Set(_)
        | CqlValue::Map(_)
        | CqlValue::Tuple(_)
        | CqlValue::Vector(_)
        | CqlValue::UserDefinedType { .. } => return None,

        CqlValue::Ascii(s) | CqlValue::Text(s) => s.clone(),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::TinyInt(n) => n.to_string(),
        CqlValue::SmallInt(n) => n.to_string(),
        CqlValue::Int(n) => n.to_string(),
        CqlValue::BigInt(n) => n.to_string(),
        CqlValue::Counter(n) => n.0.to_string(),
        CqlValue::Float(n) => n.to_string(),
        CqlValue::Double(n) => n.to_string(),
        // `0x…` is CQL's own blob literal, which is what `cqlsh` shows and what
        // can be pasted back into a statement.
        CqlValue::Blob(bytes) => hex_of(bytes),
        CqlValue::Uuid(id) => id.to_string(),
        CqlValue::Timeuuid(id) => id.to_string(),
        CqlValue::Inet(address) => address.to_string(),
        CqlValue::Varint(n) => {
            num_bigint::BigInt::from_signed_bytes_be(n.as_signed_bytes_be_slice()).to_string()
        }
        CqlValue::Decimal(n) => decimal_text(n),
        CqlValue::Duration(d) => duration_text(*d),
        CqlValue::Date(d) => date_text(*d),
        CqlValue::Time(t) => time_text(*t),
        CqlValue::Timestamp(t) => timestamp_text(*t),
        CqlValue::Empty => String::new(),
        other => other.to_string(),
    };
    Some(text)
}

/// A value as the JSON a person would write.
///
/// Numbers stay numbers and strings stay strings, so a `list<int>` reads as
/// `[1,2,3]` rather than `["1","2","3"]`. A map becomes an object with its keys
/// rendered as text, which is what Cassandra's own `toJson` does with a map
/// whose keys are not strings — JSON has no other shape for one. Fields keep the
/// order the value arrived in: declaration order for a user-defined type, and
/// key order for a map, which Cassandra stores sorted and returns that way.
///
/// Written out as text rather than assembled as a `serde_json::Value`, and that
/// is the whole reason this function exists in this shape. `Value` holds an
/// object's keys in a `BTreeMap` unless the `preserve_order` feature is on, and
/// cargo unifies features across a build: `bson` turns it on, `bson` arrives
/// with the MongoDB driver, so a user-defined type came out in declaration order
/// under `cargo test --workspace` and in alphabetical order under `cargo test -p
/// driver-cassandra`. The same driver reading the same row must not print two
/// different cells depending on which other crates were compiled beside it.
/// `serde_json` still does the escaping and the number formatting, which is the
/// part worth borrowing.
fn json_of(value: &CqlValue) -> String {
    match value {
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::TinyInt(n) => n.to_string(),
        CqlValue::SmallInt(n) => n.to_string(),
        CqlValue::Int(n) => n.to_string(),
        CqlValue::BigInt(n) => n.to_string(),
        CqlValue::Counter(n) => n.0.to_string(),
        CqlValue::Float(n) => real(f64::from(*n)),
        CqlValue::Double(n) => real(*n),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            wrapped('[', items.iter().map(json_of), ']')
        }
        CqlValue::Map(pairs) => wrapped(
            '{',
            pairs
                .iter()
                .map(|(key, item)| format!("{}:{}", quoted(&text_of(key)), json_of(item))),
            '}',
        ),
        CqlValue::Tuple(items) => wrapped('[', items.iter().map(maybe_json), ']'),
        CqlValue::UserDefinedType { fields, .. } => wrapped(
            '{',
            fields
                .iter()
                .map(|(name, item)| format!("{}:{}", quoted(name), maybe_json(item))),
            '}',
        ),
        CqlValue::Empty => "null".to_string(),
        other => match scalar_text(other) {
            Some(text) => quoted(&text),
            // Unreachable while every composite is listed above, and a null
            // rather than a panic if a later release adds one that is not.
            None => "null".to_string(),
        },
    }
}

fn maybe_json(value: &Option<CqlValue>) -> String {
    value.as_ref().map_or_else(|| "null".to_string(), json_of)
}

/// `[a,b,c]` or `{a,b,c}`, from the pieces between the brackets.
fn wrapped(open: char, parts: impl Iterator<Item = String>, close: char) -> String {
    let mut out = String::from(open);
    for (at, part) in parts.enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push_str(&part);
    }
    out.push(close);
    out
}

/// A string as a JSON string, escaped by the crate that owns the rules.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// A float as JSON, or as its name in quotes where JSON has no spelling for it.
///
/// NaN and the infinities are values CQL stores and JSON cannot write. Their
/// names are better than a null, which would claim the element was absent.
fn real(value: f64) -> String {
    serde_json::Number::from_f64(value)
        .map_or_else(|| quoted(&value.to_string()), |n| n.to_string())
}

fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2 + 2);
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A CQL decimal as its digits, with the point where the scale puts it.
///
/// The value is `unscaled × 10⁻ˢᶜᵃˡᵉ`, so a positive scale moves the point left
/// and a negative one appends zeros.
fn decimal_text(value: &CqlDecimal) -> String {
    let (bytes, scale) = value.as_signed_be_bytes_slice_and_exponent();
    let rendered = num_bigint::BigInt::from_signed_bytes_be(bytes).to_string();
    let (sign, digits) = match rendered.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", rendered.as_str()),
    };
    let places = scale.unsigned_abs() as usize;
    if places > MAX_PLACES {
        return format!("{sign}{digits}E{}", -i64::from(scale));
    }
    if scale == 0 {
        format!("{sign}{digits}")
    } else if scale < 0 {
        format!("{sign}{digits}{}", "0".repeat(places))
    } else if digits.len() > places {
        let (whole, fraction) = digits.split_at(digits.len() - places);
        format!("{sign}{whole}.{fraction}")
    } else {
        format!("{sign}0.{}{digits}", "0".repeat(places - digits.len()))
    }
}

/// A CQL duration in the literal syntax it was written in.
///
/// Three independent counts and not one span: a month is not a fixed number of
/// days and a day is not a fixed number of nanoseconds, which is the whole
/// reason the type has three fields. Collapsing them into one number would
/// require choosing a calendar the value does not have.
fn duration_text(value: CqlDuration) -> String {
    format!("{}mo{}d{}ns", value.months, value.days, value.nanoseconds)
}

/// A CQL date as `YYYY-MM-DD`, for one nested in a collection.
///
/// A `date` *column* never comes through here — it is `Date32` and carries the
/// day count itself. This is for a `list<date>`, where the cell is JSON and a
/// day count would be unreadable.
///
/// CQL's range runs from -5877641-06-23 to 5881580-07-11 and `chrono`'s stops
/// at ±262143, so the far ends have no calendar rendering. They keep the day
/// count, which is what the value is.
fn date_text(value: CqlDate) -> String {
    let days = i64::from(value.0) - DAY_ZERO;
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|epoch| {
            chrono::TimeDelta::try_days(days).and_then(|d| epoch.checked_add_signed(d))
        })
        .map_or_else(
            || format!("{days} days from 1970-01-01"),
            |date| date.to_string(),
        )
}

/// A CQL time as `HH:MM:SS.fffffffff`, which is its literal syntax.
fn time_text(value: CqlTime) -> String {
    let nanos = value.0;
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        nanos / 3_600_000_000_000,
        nanos / 60_000_000_000 % 60,
        nanos / 1_000_000_000 % 60,
        nanos % 1_000_000_000
    )
}

/// A CQL timestamp as an RFC 3339 instant in UTC, for one nested in a
/// collection.
fn timestamp_text(value: CqlTimestamp) -> String {
    chrono::DateTime::from_timestamp_millis(value.0).map_or_else(
        || format!("{} ms from 1970-01-01T00:00:00Z", value.0),
        |at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{Counter, CqlTimeuuid, CqlVarint};

    fn native(typ: NativeType) -> ColumnType<'static> {
        ColumnType::Native(typ)
    }

    /// The rule this module exists to keep. Every CQL type has to land in the
    /// set the reader on the other side of the FFI has a format string for; a
    /// column that arrives as anything else is drawn as `<+l>` in every cell.
    #[test]
    fn every_type_lands_somewhere_the_grid_can_draw() {
        use scylla::frame::response::result::CollectionType;

        let readable = [
            DataType::Boolean,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::Binary,
            DataType::Date32,
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        ];
        let mut types: Vec<ColumnType<'static>> = [
            NativeType::Ascii,
            NativeType::Boolean,
            NativeType::Blob,
            NativeType::Counter,
            NativeType::Date,
            NativeType::Decimal,
            NativeType::Double,
            NativeType::Duration,
            NativeType::Float,
            NativeType::Int,
            NativeType::BigInt,
            NativeType::Text,
            NativeType::Timestamp,
            NativeType::Inet,
            NativeType::SmallInt,
            NativeType::TinyInt,
            NativeType::Time,
            NativeType::Timeuuid,
            NativeType::Uuid,
            NativeType::Varint,
        ]
        .into_iter()
        .map(native)
        .collect();
        types.push(ColumnType::Collection {
            frozen: false,
            typ: CollectionType::List(Box::new(native(NativeType::Int))),
        });
        types.push(ColumnType::Collection {
            frozen: true,
            typ: CollectionType::Map(
                Box::new(native(NativeType::Text)),
                Box::new(native(NativeType::Int)),
            ),
        });
        types.push(ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Set(Box::new(native(NativeType::Text))),
        });
        types.push(ColumnType::Tuple(vec![
            native(NativeType::Int),
            native(NativeType::Text),
        ]));
        types.push(ColumnType::Vector {
            typ: Box::new(native(NativeType::Float)),
            dimensions: 3,
        });

        for typ in types {
            let arrow = cell_of(&typ).arrow();
            assert!(
                readable.contains(&arrow),
                "{typ:?} would arrive as {arrow:?}, which the grid draws as a format string"
            );
        }
    }

    /// The two ends of CQL's date range, which is exactly Arrow's once the bias
    /// is taken off. Getting the offset wrong is invisible in the middle of the
    /// range and wrong by 5.8 million years at the edges.
    #[test]
    fn a_date_keeps_its_whole_range_and_the_epoch_lands_on_zero() {
        assert_eq!(days_of(CqlDate(1 << 31)), 0);
        assert_eq!(days_of(CqlDate(0)), i32::MIN);
        assert_eq!(days_of(CqlDate(u32::MAX)), i32::MAX);
        // 2024-01-15 is 19737 days after the epoch.
        assert_eq!(days_of(CqlDate((1 << 31) + 19_737)), 19_737);
    }

    /// The digits are the whole point of a `varint`: the values people store in
    /// one are exactly the ones no fixed-width integer holds.
    #[test]
    fn a_varint_is_rendered_from_its_bytes_rather_than_squeezed_into_an_integer() {
        // 2^80, which is 27 digits and fits nothing narrower than a bignum.
        let mut bytes = vec![0x01];
        bytes.extend(std::iter::repeat_n(0x00, 10));
        let huge = CqlValue::Varint(CqlVarint::from_signed_bytes_be(bytes));
        assert_eq!(text_of(&huge), "1208925819614629174706176");

        // Two's complement, so the sign is in the top bit and not in a field.
        let negative = CqlValue::Varint(CqlVarint::from_signed_bytes_be(vec![0xff, 0x85]));
        assert_eq!(text_of(&negative), "-123");
        let zero = CqlValue::Varint(CqlVarint::from_signed_bytes_be(vec![0x00]));
        assert_eq!(text_of(&zero), "0");
    }

    /// The scale travels with the value, which is why the column cannot be a
    /// `Decimal128` — and why the point has to be placed here.
    #[test]
    fn a_decimal_puts_the_point_where_its_own_scale_says() {
        let of = |unscaled: i64, scale: i32| {
            let bytes = num_bigint::BigInt::from(unscaled).to_signed_bytes_be();
            text_of(&CqlValue::Decimal(
                CqlDecimal::from_signed_be_bytes_and_exponent(bytes, scale),
            ))
        };
        assert_eq!(of(123_456, 2), "1234.56");
        assert_eq!(of(-123_456, 2), "-1234.56");
        assert_eq!(of(123_456, 0), "123456");
        // Fewer digits than places: the point goes in front and the gap is
        // padded, or `5` at scale 4 reads as `5.` and then nothing.
        assert_eq!(of(5, 4), "0.0005");
        assert_eq!(of(-5, 4), "-0.0005");
        // A negative scale multiplies, which is zeros on the right.
        assert_eq!(of(12, -3), "12000");
        // And a scale nobody could pad out is written with its exponent rather
        // than allocating a gigabyte of zeros.
        assert_eq!(of(7, -2_000_000_000), "7E2000000000");
    }

    /// Nanoseconds, all nine digits of them, because the reader's only time
    /// case is microseconds and this is the column that would lose three.
    #[test]
    fn a_time_keeps_the_nanoseconds_the_reader_has_no_column_for() {
        let midday = CqlValue::Time(CqlTime(12 * 3_600_000_000_000 + 123_456_789));
        assert_eq!(text_of(&midday), "12:00:00.123456789");
        assert_eq!(text_of(&CqlValue::Time(CqlTime(0))), "00:00:00.000000000");
    }

    /// A composite is one cell of readable JSON, with its elements still
    /// typed — a `list<int>` that came back as strings would sort as strings.
    #[test]
    fn a_collection_is_one_cell_of_json_with_its_numbers_still_numbers() {
        let list = CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2), CqlValue::Int(3)]);
        assert_eq!(text_of(&list), "[1,2,3]");

        let map = CqlValue::Map(vec![
            (
                CqlValue::Text("city".to_string()),
                CqlValue::Text("Taipei".to_string()),
            ),
            (CqlValue::Text("zip".to_string()), CqlValue::Int(100)),
        ]);
        assert_eq!(text_of(&map), r#"{"city":"Taipei","zip":100}"#);

        let udt = CqlValue::UserDefinedType {
            keyspace: "bench".to_string(),
            name: "address".to_string(),
            fields: vec![
                (
                    "street".to_string(),
                    Some(CqlValue::Text("Ren'ai".to_string())),
                ),
                ("number".to_string(), None),
            ],
        };
        // Declaration order, and `number` before `street` would mean this had
        // gone through a `serde_json::Value` again — where the order depends on
        // a feature another crate in the workspace turns on. See `json_of`.
        assert_eq!(text_of(&udt), r#"{"street":"Ren'ai","number":null}"#);

        let tuple = CqlValue::Tuple(vec![Some(CqlValue::Int(7)), None]);
        assert_eq!(text_of(&tuple), "[7,null]");
    }

    /// A map key that is not a string still has to be a JSON key, and a nested
    /// temporal value still has to be readable.
    #[test]
    fn a_nested_value_is_rendered_rather_than_left_as_a_number_of_days() {
        let map = CqlValue::Map(vec![(CqlValue::Int(7), CqlValue::Boolean(true))]);
        assert_eq!(text_of(&map), r#"{"7":true}"#);

        let dates = CqlValue::List(vec![CqlValue::Date(CqlDate((1 << 31) + 19_737))]);
        assert_eq!(text_of(&dates), r#"["2024-01-15"]"#);

        let instants = CqlValue::Set(vec![CqlValue::Timestamp(CqlTimestamp(1_700_000_000_000))]);
        assert_eq!(text_of(&instants), r#"["2023-11-14T22:13:20.000Z"]"#);
    }

    /// A blob is CQL's own literal so it can be pasted back into a statement,
    /// and a counter is a number rather than the wrapper it arrives in.
    #[test]
    fn the_remaining_scalars_read_the_way_cqlsh_prints_them() {
        assert_eq!(text_of(&CqlValue::Blob(vec![0x00, 0x0f, 0xff])), "0x000fff");
        assert_eq!(text_of(&CqlValue::Counter(Counter(42))), "42");
        assert_eq!(
            text_of(&CqlValue::Duration(CqlDuration {
                months: 1,
                days: 4,
                nanoseconds: 3_600_000_000_000,
            })),
            "1mo4d3600000000000ns"
        );
        let id: CqlTimeuuid = "8e14e760-7fa8-11eb-bc66-000000000001"
            .parse()
            .expect("a version 1 UUID");
        assert_eq!(
            text_of(&CqlValue::Timeuuid(id)),
            "8e14e760-7fa8-11eb-bc66-000000000001"
        );
    }

    /// The zero-length legacy value is not the empty string and is not a
    /// number; a grid has one empty cell for it and for null alike.
    #[test]
    fn the_legacy_empty_value_reads_as_an_empty_cell() {
        let plan = Plan {
            schema: Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, true)])),
            cells: vec![Cell::Int32],
        };
        let rows = vec![
            Row {
                columns: vec![Some(CqlValue::Empty)],
            },
            Row {
                columns: vec![Some(CqlValue::Int(5))],
            },
        ];
        let batch = plan.batch(&rows).expect("batch");
        assert!(batch.column(0).is_null(0));
        assert!(!batch.column(0).is_null(1));
    }
}
