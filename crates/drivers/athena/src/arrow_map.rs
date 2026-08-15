//! Athena's types to Arrow's, and its text to Arrow's values.
//!
//! **No server has answered this driver**, so everything here is read from the
//! Athena API reference and from Presto's type documentation, which is what
//! Athena's engine is. What that leaves genuinely unknown is named on each
//! decision below rather than gathered at the end.
//!
//! **Every value in an Athena result is text.** The API has one field for a
//! cell and it is called `VarCharValue`; a `bigint`, a `date` and a `row` all
//! arrive as strings, and a null is that field being absent. So this file is a
//! parser where the Trino driver's equivalent is a reader of JSON types — and
//! the difference is not cosmetic. Trino can tell a JSON number from a JSON
//! string and this cannot tell anything from anything: the only thing saying
//! what a cell is, is the column's declared type.
//!
//! **The rule**, which is the same one the Cassandra and Trino drivers state: a
//! scalar gets the narrowest Arrow type that holds every value it can take
//! exactly, out of the twelve the reader at the other end of the FFI has a case
//! for — `apps/macos/Sources/DbClient/ArrowTable.swift` maps `b`, `s`, `i`, `l`,
//! `f`, `g`, `u`, `z`, `tdD`, `ttu`, `tsu:` and `d:`. Anything else becomes
//! text, and the text is Athena's own rendering, which is the form a person
//! would recognise because it is the form the console shows.
//!
//! **Four decisions, and each of them could be wrong in a way only an account
//! can settle.**
//!
//! - **A `varbinary` is text.** Athena renders one into `VarCharValue` and no
//!   document this was written from says with what encoding — base64, hex, and
//!   Presto's own `to_hex` spelling are all plausible and they are not
//!   distinguishable by looking at the string. Handing over the text Athena
//!   wrote cannot be wrong about the bytes; decoding a guess would corrupt them
//!   silently, which is the one outcome worth ruling out.
//! - **A `timestamp` is microseconds, and there is no precision to read.**
//!   `ColumnInfo` carries `Precision` and `Scale` for a `decimal` and says
//!   nothing about a datetime's digits — the type is the bare word `timestamp`
//!   whatever the column was declared as. Athena's engine defaults to
//!   milliseconds, which fits; an Iceberg table declaring `timestamp(9)` would
//!   not, and its values are **refused rather than truncated**. Loud is the
//!   right failure here for the reason the Trino driver gives about a value of
//!   the wrong shape: a column of silent nulls, or of quietly shortened times,
//!   is the one outcome that would hide the mistake.
//! - **A `timestamp with time zone` is text**, at any precision, for Arrow's
//!   reason rather than Athena's: Arrow carries one zone for a whole column and
//!   Presto carries one per *value*. Converting them to a common instant throws
//!   away the zone the row was stored with; claiming a column-wide zone states
//!   something that is not in the data. Same for `time with time zone`, which
//!   Arrow has no type for at all.
//! - **Every composite is one text cell.** `array`, `map` and `row` arrive as
//!   Presto's own rendering — `[1, 2, 3]`, `{k=v}`, `{n=1, w=abc}` — which is
//!   not JSON and would have to be parsed with a Presto-specific reader to
//!   become anything else. Arrow's `List`, `Map` and `Struct` are types the
//!   reader has no case for anyway, so the rendering goes across as itself.

use arrow::array::{
    ArrayRef, BooleanBuilder, Date32Builder, Decimal128Builder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, RecordBatch, StringBuilder, Time64MicrosecondBuilder,
    TimestampMicrosecondBuilder,
};
use arrow::compute::kernels::cast_utils::{Parser, parse_decimal};
use arrow::datatypes::{
    DataType, Date32Type, Decimal128Type, Field, Schema, SchemaRef, Time64MicrosecondType,
    TimeUnit, TimestampMicrosecondType,
};
use arrow::error::ArrowError;
use std::sync::Arc;

use crate::wire::{ColumnInfo, Row};

/// Which Arrow builder a column's values go into.
///
/// A small closed set rather than `DataType` itself, because the decision this
/// makes is "which builder", and the dozen Athena types that land in `Text`
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
            Cell::Date => DataType::Date32,
            Cell::Time => DataType::Time64(TimeUnit::Microsecond),
            // No zone, and stated by its absence rather than as "UTC". A Presto
            // `timestamp` is a wall clock with no zone attached — the type that
            // has one is a different type, and it is text here.
            Cell::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Cell::Text => DataType::Utf8,
        }
    }
}

/// The builder one Athena type's values belong in.
///
/// Matched on the whole type string, which is what the API sends — `varchar(15)`
/// arrives as `varchar` with a separate length, and `decimal` arrives with its
/// digits in `Precision` and `Scale` rather than in the name. The parameterised
/// spellings are still handled, by taking the name up to the first `(`, because
/// the two zoned datetime types arrive with their words spelled out and a
/// prefix match on those would catch the wrong ones.
pub(crate) fn cell_of(column: &ColumnInfo) -> Cell {
    let name = column
        .r#type
        .split_once('(')
        .map_or(column.r#type.as_str(), |(head, _)| head);
    match name {
        "boolean" => Cell::Bool,
        // Arrow has an Int8 and the reader on the far side of the FFI does not,
        // so a `tinyint` is widened rather than left as a column of `<c>`.
        "tinyint" | "smallint" => Cell::Int16,
        "integer" | "int" => Cell::Int32,
        "bigint" => Cell::Int64,
        "real" => Cell::Float32,
        "double" | "float" => Cell::Float64,
        "decimal" => match (column.precision, column.scale) {
            (precision, scale)
                if (1..=38).contains(&precision) && (0..=precision).contains(&scale) =>
            {
                Cell::Decimal(precision as u8, scale as i8)
            }
            // A decimal whose own metadata does not describe a decimal. Not
            // reachable against an Athena that is working, and the alternative
            // to this arm is a cast that turns a service change into a panic.
            _ => Cell::Text,
        },
        "date" => Cell::Date,
        "time" => Cell::Time,
        "timestamp" => Cell::Timestamp,
        _ => Cell::Text,
    }
}

/// A result's columns and how to read their values.
pub(crate) struct Plan {
    schema: SchemaRef,
    cells: Vec<Cell>,
    /// The column names, kept apart from the schema so that the header-row check
    /// can compare against them without walking fields.
    names: Vec<String>,
}

impl Plan {
    pub fn of(columns: &[ColumnInfo]) -> Plan {
        let cells: Vec<Cell> = columns.iter().map(cell_of).collect();
        let fields: Vec<Field> = columns
            .iter()
            .zip(&cells)
            // Nullable throughout, and not from asking the catalog: this is a
            // result and not a table, and an outer join over a column declared
            // `NOT NULL` produces nulls in it. Athena's `ColumnInfo` does carry
            // a `Nullable`, and it is documented to be `UNKNOWN` for every
            // column of every result, which is worse than not asking.
            .map(|(column, cell)| Field::new(&column.name, cell.arrow(), true))
            .collect();
        Plan {
            schema: Arc::new(Schema::new(fields)),
            cells,
            names: columns.iter().map(|c| c.name.clone()).collect(),
        }
    }

    /// The plan for a statement that has no result set.
    pub fn empty() -> Plan {
        Plan {
            schema: Arc::new(Schema::empty()),
            cells: Vec::new(),
            names: Vec::new(),
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    pub fn columns(&self) -> usize {
        self.cells.len()
    }

    /// Whether `row` is the column headers repeated as data.
    ///
    /// **Athena's best-known quirk, and the awkward half of this driver.** The
    /// first row of the first page of a `SELECT`'s results is the column names,
    /// as strings, in a row that looks exactly like a data row. It is not there
    /// for a `SHOW`, and not for a statement that writes.
    ///
    /// Two guards, and the pairing is the decision. `crate::Rows` applies this
    /// only when `GetQueryExecution` classified the statement as `DML`, which is
    /// the protocol's own word for the shape that has a header; this then asks
    /// whether the row actually looks like one. Requiring both means the ways to
    /// be wrong are: a `DML` whose genuine first row is literally its own column
    /// names — `SELECT 'id' AS id` — which loses a row, and a statement Athena
    /// classifies as something else while still sending a header, which shows a
    /// spurious first row. Those are not equally bad. The second is visible in
    /// the grid and somebody reports it; the first is silent. Requiring both
    /// makes the second one the one that can happen.
    ///
    /// A null in the row is not a header value, because a header is always a
    /// name and never absent — which is also what saves `SELECT NULL AS n`.
    pub fn is_header(&self, row: &Row) -> bool {
        row.data.len() == self.names.len()
            && !self.names.is_empty()
            && row
                .data
                .iter()
                .zip(&self.names)
                .all(|(datum, name)| datum.var_char_value.as_deref() == Some(name.as_str()))
    }

    /// One page of Athena's row-major text, as a column-major batch.
    pub fn batch(&self, rows: &[Row]) -> Result<RecordBatch, ArrowError> {
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
/// A row shorter than the schema has its missing cells read as nulls rather than
/// refused, which is the opposite of what the Trino driver does with the same
/// situation — and the reason is that the two protocols mean different things by
/// it. Trino sends a value per column per row, so a short row is a protocol
/// violation. Athena sends a `Datum` that is `{}` for a null, and a trailing run
/// of nulls arriving as a shorter list is a shape the JSON permits.
fn build(cell: Cell, name: &str, rows: &[Row], at: usize) -> Result<ArrayRef, ArrowError> {
    let values = || {
        rows.iter()
            .map(move |row| row.data.get(at).and_then(|d| d.var_char_value.as_deref()))
    };

    Ok(match cell {
        Cell::Bool => {
            let mut builder = BooleanBuilder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    // Athena writes `true` and `false`; the upper-case spellings
                    // are accepted because a `CAST` chain can produce them and
                    // refusing a value nobody would notice writing is not worth
                    // the arm it would take to be strict.
                    Some(text) => builder.append_value(match text.to_ascii_lowercase().as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return Err(wrong(name, "true or false", text)),
                    }),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int16 => {
            let mut builder = Int16Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        text.parse()
                            .map_err(|_| wrong(name, "a 16-bit integer", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int32 => {
            let mut builder = Int32Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        text.parse()
                            .map_err(|_| wrong(name, "a 32-bit integer", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Int64 => {
            let mut builder = Int64Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        text.parse()
                            .map_err(|_| wrong(name, "a 64-bit integer", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Float32 => {
            let mut builder = Float32Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(float(text, name)? as f32),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Float64 => {
            let mut builder = Float64Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(float(text, name)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Decimal(precision, scale) => {
            let mut builder = Decimal128Builder::with_capacity(rows.len())
                .with_precision_and_scale(precision, scale)?;
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder
                        .append_value(parse_decimal::<Decimal128Type>(text, precision, scale)?),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Date => {
            let mut builder = Date32Builder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        Date32Type::parse(text).ok_or_else(|| wrong(name, "a date", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Time => {
            let mut builder = Time64MicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        Time64MicrosecondType::parse(text)
                            .ok_or_else(|| wrong(name, "a time", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Timestamp => {
            let mut builder = TimestampMicrosecondBuilder::with_capacity(rows.len());
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(
                        TimestampMicrosecondType::parse(text)
                            .ok_or_else(|| wrong(name, "a timestamp", text))?,
                    ),
                }
            }
            Arc::new(builder.finish())
        }
        Cell::Text => {
            let mut builder = StringBuilder::with_capacity(rows.len(), rows.len() * 16);
            for value in values() {
                match value {
                    None => builder.append_null(),
                    Some(text) => builder.append_value(text),
                }
            }
            Arc::new(builder.finish())
        }
    })
}

/// A value that arrived as something its column's type does not produce.
///
/// Loud rather than nulled, as in the Trino driver. Against an Athena that is
/// working this cannot happen; if it does, this file is wrong about a type, and
/// a column of silent nulls is the one outcome that would hide it.
fn wrong(name: &str, expected: &str, value: &str) -> ArrowError {
    ArrowError::ParseError(format!(
        "{name} should have arrived as {expected}, got {value:?}"
    ))
}

/// A float, including the three values a decimal point cannot spell.
///
/// Presto renders them as `NaN`, `Infinity` and `-Infinity`, which Rust's own
/// parser reads as `NaN` and `inf` — so the three are named here rather than
/// left to `str::parse`, which accepts `inf` and refuses `Infinity`.
fn float(text: &str, name: &str) -> Result<f64, ArrowError> {
    match text {
        "NaN" => Ok(f64::NAN),
        "Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        _ => text.parse().map_err(|_| wrong(name, "a number", text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Datum;

    fn column(name: &str, kind: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            r#type: kind.to_string(),
            precision: 0,
            scale: 0,
        }
    }

    fn row(values: &[Option<&str>]) -> Row {
        Row {
            data: values
                .iter()
                .map(|value| Datum {
                    var_char_value: value.map(str::to_string),
                })
                .collect(),
        }
    }

    /// The Presto names Athena sends, and the ones this driver deliberately does
    /// not narrow.
    #[test]
    fn a_type_with_no_arrow_home_arrives_as_the_text_athena_wrote() {
        for kind in [
            "varchar",
            "varchar(15)",
            "char(5)",
            "varbinary",
            "json",
            "uuid",
            "ipaddress",
            "array(integer)",
            "map(varchar, integer)",
            "row(n integer, w varchar)",
            "interval day to second",
            "unknown",
            "a type this driver has never met",
        ] {
            assert_eq!(cell_of(&column("c", kind)), Cell::Text, "{kind}");
        }
    }

    /// The zoned pair, which is about Arrow rather than about Athena: Arrow
    /// carries one zone per column and Presto one per value.
    #[test]
    fn a_zoned_datetime_is_text_and_a_plain_one_is_not() {
        assert_eq!(cell_of(&column("t", "timestamp")), Cell::Timestamp);
        assert_eq!(cell_of(&column("t", "time")), Cell::Time);
        assert_eq!(
            cell_of(&column("t", "timestamp with time zone")),
            Cell::Text
        );
        assert_eq!(cell_of(&column("t", "time with time zone")), Cell::Text);
    }

    /// A decimal's digits are separate fields here rather than part of the type
    /// name, and one whose metadata could not describe a decimal falls back to
    /// its text rather than panicking on the cast.
    #[test]
    fn a_decimal_carries_the_precision_the_metadata_states() {
        let mut money = column("amount", "decimal");
        money.precision = 18;
        money.scale = 2;
        assert_eq!(cell_of(&money), Cell::Decimal(18, 2));

        money.precision = 38;
        money.scale = 38;
        assert_eq!(cell_of(&money), Cell::Decimal(38, 38));

        money.precision = 39;
        money.scale = 0;
        assert_eq!(cell_of(&money), Cell::Text);

        // The default, which is what a `ColumnInfo` with no digits in it looks
        // like.
        assert_eq!(cell_of(&column("amount", "decimal")), Cell::Text);
    }

    /// The three values a `double` can hold that a decimal point cannot spell.
    /// Rust's own parser takes `inf` and refuses `Infinity`, which is the
    /// spelling Presto writes.
    #[test]
    fn a_double_that_is_not_a_number_still_arrives() {
        assert!(float("NaN", "d").unwrap().is_nan());
        assert_eq!(float("Infinity", "d").unwrap(), f64::INFINITY);
        assert_eq!(float("-Infinity", "d").unwrap(), f64::NEG_INFINITY);
        assert_eq!(float("2.5", "d").unwrap(), 2.5);
        assert!(float("wibble", "d").is_err());
    }

    /// The widest `bigint` there is. Every value in an Athena result is text, so
    /// this is the test that says the text is parsed as an integer rather than
    /// through a float on its way.
    #[test]
    fn the_ends_of_the_bigint_range_survive_the_text() {
        let plan = Plan::of(&[column("id", "bigint")]);
        let batch = plan
            .batch(&[
                row(&[Some("9223372036854775807")]),
                row(&[Some("-9223372036854775808")]),
            ])
            .expect("a batch");
        let ids =
            arrow::array::cast::as_primitive_array::<arrow::datatypes::Int64Type>(batch.column(0));
        assert_eq!(ids.value(0), i64::MAX);
        assert_eq!(ids.value(1), i64::MIN);
    }

    /// Athena's datetime spelling, and the null that is a missing field rather
    /// than an empty string.
    #[test]
    fn athenas_own_datetime_spelling_parses_and_a_missing_value_is_a_null() {
        let plan = Plan::of(&[
            column("ts", "timestamp"),
            column("tm", "time"),
            column("d", "date"),
            column("ok", "boolean"),
        ]);
        let batch = plan
            .batch(&[
                row(&[
                    Some("2024-01-15 12:34:56.123"),
                    Some("12:34:56.123456"),
                    Some("2024-01-15"),
                    Some("true"),
                ]),
                row(&[None, None, None, None]),
            ])
            .expect("a batch");
        assert_eq!(batch.num_rows(), 2);
        for at in 0..4 {
            assert!(
                !batch.column(at).is_null(0),
                "column {at} row 1 is not null"
            );
            assert!(batch.column(at).is_null(1), "column {at} row 2 is null");
        }
    }

    /// An empty string is a value a `varchar` can hold and a missing field is
    /// not — reading one as the other would put empty strings where the table
    /// has nulls.
    #[test]
    fn an_empty_string_is_not_a_null() {
        let plan = Plan::of(&[column("label", "varchar")]);
        let batch = plan
            .batch(&[row(&[Some("")]), row(&[None])])
            .expect("a batch");
        assert!(!batch.column(0).is_null(0));
        assert!(batch.column(0).is_null(1));
    }

    /// A trailing run of nulls may arrive as a shorter row, which the JSON
    /// permits — unlike Trino, where a short row would be a protocol violation.
    #[test]
    fn a_row_shorter_than_the_result_reads_the_rest_as_nulls() {
        let plan = Plan::of(&[column("a", "integer"), column("b", "integer")]);
        let batch = plan.batch(&[row(&[Some("1")])]).expect("a batch");
        assert!(!batch.column(0).is_null(0));
        assert!(batch.column(1).is_null(0));
    }

    /// The header row, and the two rows that look like one and are not.
    #[test]
    fn the_repeated_header_is_recognised_and_a_real_row_is_not() {
        let plan = Plan::of(&[column("id", "varchar"), column("label", "varchar")]);
        assert!(plan.is_header(&row(&[Some("id"), Some("label")])));
        // One name matching is not the header.
        assert!(!plan.is_header(&row(&[Some("id"), Some("row-1")])));
        // A null is never a header value, which is what saves `SELECT NULL`.
        assert!(!plan.is_header(&row(&[Some("id"), None])));
        // A row of a different width is not the header either.
        assert!(!plan.is_header(&row(&[Some("id")])));
        // And a result with no columns has no header to find.
        assert!(!Plan::empty().is_header(&row(&[])));
    }

    /// The case this rule gets wrong, pinned so that it is a known cost rather
    /// than a surprise: a genuine first row that is literally its own column
    /// names. `crate::Rows` narrows it further by only applying the rule to a
    /// `DML`, which is what keeps this to the one statement somebody wrote on
    /// purpose.
    #[test]
    fn a_row_that_really_is_its_own_column_names_is_indistinguishable() {
        let plan = Plan::of(&[column("id", "varchar")]);
        assert!(plan.is_header(&row(&[Some("id")])));
    }
}
